//! E2E: движок действительно скачивает торрент по сети.
//!
//! Сеть полностью локальная: mock-трекер и mock-сидер биндятся на
//! `127.0.0.1:0`, движок слушает `listen_port: 0`. Внешние адреса не
//! используются, интернет не нужен.
//!
//! Что проверяется:
//! * `TorrentManager::add_file` принимает `.torrent`, сам объявляется трекеру,
//!   получает список пиров и качает куски по peer wire protocol;
//! * итоговые файлы на диске байт-в-байт совпадают с исходными;
//! * снимок состояния доходит до `Seeding`/`Completed` с `progress == 1.0`,
//!   `pieces_have == piece_count` и `downloaded_bytes == total_bytes`;
//! * глобальные счётчики (`totals()`) видят реально принятые байты.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{wait_for, Corruption, MockSeeder, MockTracker, Sample, TrackerReply, PIECE_LENGTH};
use torrent_core::{EngineConfig, TorrentManager, TorrentState};

/// Конфиг движка для теста: свой каталог загрузки, случайный порт прослушки.
fn test_config(download_dir: &Path) -> EngineConfig {
    EngineConfig {
        download_dir: download_dir.to_path_buf(),
        listen_port: 0,
        ..EngineConfig::default()
    }
}

/// Читает скачанный файл и сверяет его с исходником.
fn assert_file_matches(root: &Path, expected: &[(String, Vec<u8>)]) {
    for (name, content) in expected {
        let path = root.join(name);
        let actual = std::fs::read(&path)
            .unwrap_or_else(|err| panic!("не удалось прочитать {}: {err}", path.display()));
        assert_eq!(
            actual.len(),
            content.len(),
            "размер файла {name} не совпал с исходником"
        );
        assert!(
            actual == *content,
            "файл {name} скачан с расхождениями в содержимом"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_two_file_torrent_from_mock_swarm() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "torrenta-e2e",
        PIECE_LENGTH,
        &[("part1.bin", 70_000), ("part2.bin", 61_536)],
    ));
    assert_eq!(sample.piece_count(), 5, "ожидалось 5 кусков");

    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "загрузка завершилась (Seeding/Completed)",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;

    let total = sample.data.len() as u64;
    assert_eq!(snapshot.name, "torrenta-e2e", "имя торрента в снимке");
    assert_eq!(snapshot.info_hash, common::hex(&sample.info_hash));
    assert_eq!(snapshot.total_bytes, total, "полный размер торрента");
    assert_eq!(
        snapshot.downloaded_bytes, total,
        "движок должен скачать ровно total_bytes"
    );
    assert_eq!(snapshot.pieces_have, snapshot.piece_count);
    assert!(
        (snapshot.progress - 1.0).abs() < f64::EPSILON,
        "прогресс должен быть 1.0, получено {}",
        snapshot.progress
    );
    assert_eq!(snapshot.error, None, "ошибок быть не должно");

    assert_file_matches(&download_dir, &sample.files);

    // Файлы на диске лежат там, где их ждёт UI (пути из снимка).
    let mut paths: Vec<String> = snapshot
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["part1.bin".to_string(), "part2.bin".to_string()]
    );

    let totals = manager.totals();
    assert_eq!(totals.torrents, 1, "менеджер видит один торрент");
    assert!(
        totals.downloaded_bytes > 0,
        "глобальный счётчик принятых байт должен быть больше нуля"
    );
    assert_eq!(
        totals.downloaded_bytes, total,
        "глобальный счётчик должен совпасть с размером торрента"
    );

    assert!(
        tracker.announce_count() >= 1,
        "движок должен был объявиться mock-трекеру"
    );
    assert!(
        seeder
            .connections
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1,
        "движок должен был подключиться к mock-сидеру"
    );
    assert!(
        seeder.requests.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "движок должен был запросить куски у mock-сидера"
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_single_file_torrent_and_stops_in_completed_state() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("single");
    let sample = Arc::new(Sample::new(
        "torrenta-single",
        PIECE_LENGTH,
        &[("payload.bin", 40_000)],
    ));
    assert_eq!(sample.piece_count(), 2, "ожидалось 2 куска");

    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(EngineConfig {
        download_dir: download_dir.clone(),
        listen_port: 0,
        seed_after_complete: false,
        ..EngineConfig::default()
    });

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "загрузка дошла до Completed",
        |item| item.state == TorrentState::Completed,
    )
    .await;

    assert_eq!(snapshot.downloaded_bytes, 40_000);
    assert_eq!(snapshot.progress, 1.0);
    assert_file_matches(&download_dir, &sample.files);

    // Повторное добавление того же торрента — не дубль-скачивание, а ошибка Duplicate.
    let duplicate = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await;
    assert!(
        duplicate.is_err(),
        "повторное добавление того же info-hash должно отклоняться"
    );

    manager.shutdown().await;
}

/// Проверяет, что два одновременных торрента скачиваются независимо.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_two_torrents_in_parallel() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let root = temp.path().to_path_buf();
    let first = Arc::new(Sample::new(
        "torrenta-a",
        PIECE_LENGTH,
        &[("a.bin", 50_000)],
    ));
    let second = Arc::new(Sample::new(
        "torrenta-b",
        PIECE_LENGTH,
        &[("b1.bin", 20_000), ("b2.bin", 30_000)],
    ));

    let seeder_a = MockSeeder::start(first.clone(), Corruption::None).await;
    let seeder_b = MockSeeder::start(second.clone(), Corruption::None).await;
    let tracker_a = MockTracker::start(TrackerReply::Peers(vec![seeder_a.addr])).await;
    let tracker_b = MockTracker::start(TrackerReply::Peers(vec![seeder_b.addr])).await;
    let manager = TorrentManager::new(test_config(&root));

    let id_a = manager
        .add_file(first.torrent_bytes(&tracker_a.url()), None)
        .await
        .expect("движок не принял первый торрент");
    let id_b = manager
        .add_file(second.torrent_bytes(&tracker_b.url()), None)
        .await
        .expect("движок не принял второй торрент");
    assert_ne!(id_a, id_b, "у торрентов должны быть разные id");

    // Оба торрента пишутся в один каталог, но в разные файлы.
    let wait_dir: PathBuf = root.clone();
    for (id, total) in [
        (id_a, first.data.len() as u64),
        (id_b, second.data.len() as u64),
    ] {
        let snapshot = wait_for(
            &manager,
            id,
            Duration::from_secs(60),
            "оба торрента докачались",
            |item| {
                matches!(item.state, TorrentState::Seeding | TorrentState::Completed)
                    && item.downloaded_bytes == total
            },
        )
        .await;
        assert_eq!(snapshot.progress, 1.0);
    }

    assert_file_matches(&wait_dir, &first.files);
    assert_file_matches(&wait_dir, &second.files);
    assert_eq!(manager.totals().torrents, 2);

    manager.shutdown().await;
}

/// Глобальный лимит скорости общий на сессию: два торрента вместе не
/// превышают потолок (в 1.0 лимит действовал на каждый торрент отдельно,
/// и суммарная скорость доходила до N × лимит).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_limit_is_shared_between_torrents() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let root = temp.path().to_path_buf();
    // По 640 КиБ на торрент: при 64 КиБ/с за окно измерения не докачаются.
    let first = Arc::new(Sample::new(
        "torrenta-limit-a",
        PIECE_LENGTH,
        &[("a.bin", 640_000)],
    ));
    let second = Arc::new(Sample::new(
        "torrenta-limit-b",
        PIECE_LENGTH,
        &[("b.bin", 640_000)],
    ));
    let seeder_a = MockSeeder::start(first.clone(), Corruption::None).await;
    let seeder_b = MockSeeder::start(second.clone(), Corruption::None).await;
    let tracker_a = MockTracker::start(TrackerReply::Peers(vec![seeder_a.addr])).await;
    let tracker_b = MockTracker::start(TrackerReply::Peers(vec![seeder_b.addr])).await;
    let manager = TorrentManager::new(EngineConfig {
        download_dir: root,
        listen_port: 0,
        download_limit_kib: 64,
        ..EngineConfig::default()
    });

    let ids = [
        manager
            .add_file(first.torrent_bytes(&tracker_a.url()), None)
            .await
            .expect("движок не принял первый торрент"),
        manager
            .add_file(second.torrent_bytes(&tracker_b.url()), None)
            .await
            .expect("движок не принял второй торрент"),
    ];
    assert_ne!(ids[0], ids[1], "у торрентов должны быть разные id");

    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let elapsed = started.elapsed().as_secs_f64();

    let snapshots = manager.snapshot();
    let downloaded: u64 = snapshots.iter().map(|item| item.downloaded_bytes).sum();
    // Запас — несколько кусков: ведро доливается во время запуска, а
    // `downloaded_bytes` учитывает только целиком проверенные куски.
    let cap = (64.0 * 1024.0 * elapsed) as u64 + 4 * PIECE_LENGTH as u64;
    assert!(
        downloaded <= cap,
        "суммарно скачано {downloaded} Б при потолке {cap} Б за {elapsed:.1} с"
    );
    assert!(
        downloaded > 0,
        "за окно измерения ничего не скачалось — тест не показателен"
    );
    for item in &snapshots {
        assert!(
            !matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
            "при лимите 64 КиБ/с торрент не должен успеть докачаться"
        );
    }

    manager.shutdown().await;
}
