//! Лимиты раздачи и жизненный цикл обмена (1.9.1).
//!
//! Проверяет два дефекта, найденных на ревью:
//! * D14 — при выключенной раздаче или сработавшем лимите обмен должен
//!   останавливаться, а не только меняться бейдж;
//! * D11 — для готовых данных (скачано 0, отдано > 0) коэффициент раздачи
//!   бесконечен, иначе лимит ratio не срабатывает никогда.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use torrent_core::{AddOptions, ContentLayout, EngineConfig, TorrentManager, TorrentSnapshot};

use common::{MockTracker, Sample, TrackerReply, PIECE_LENGTH};

/// Раскладывает данные торрента в каталог так же, как их увидит движок
/// при проверке (раскладка `NoSubfolder`).
fn write_data(sample: &Sample, dir: &Path) {
    for (name, content) in &sample.files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("каталог файла");
        }
        std::fs::write(path, content).expect("файл торрента");
    }
}

fn snapshot(manager: &Arc<TorrentManager>, id: u64) -> TorrentSnapshot {
    common::require_snapshot(manager, id)
}

/// Ждёт выполнения условия с таймаутом; возвращает последний снимок.
async fn wait_for<F>(manager: &Arc<TorrentManager>, id: u64, mut ready: F) -> TorrentSnapshot
where
    F: FnMut(&TorrentSnapshot) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let snap = snapshot(manager, id);
        if ready(&snap) {
            return snap;
        }
        if Instant::now() >= deadline {
            panic!("условие не наступило: {snap:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn ready_data_has_infinite_ratio_and_limit_stops_exchange() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let dir = temp.path().join("downloads");
    std::fs::create_dir_all(&dir).expect("каталог загрузки");
    let sample = Sample::new("seed-limits", PIECE_LENGTH, &[("data.bin", 64 * 1024)]);
    write_data(&sample, &dir);

    let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let config = EngineConfig {
        download_dir: dir,
        listen_port: 0,
        dht: false,
        content_layout: ContentLayout::NoSubfolder,
        // Раздача включена, но лимит ratio = 1.0 при бесконечном коэффициенте
        // срабатывает сразу.
        seed_after_complete: true,
        seed_ratio_limit: 1.0,
        ..EngineConfig::default()
    };
    let manager = TorrentManager::new(config);
    let id = manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("торрент добавлен");
    manager.recheck(id).expect("проверка запущена");

    let seeding = wait_for(&manager, id, |snap| {
        matches!(snap.state, torrent_core::TorrentState::Seeding)
    })
    .await;
    // Данные были на диске: скачано 0, отдано 0 — коэффициент ещё ноль.
    assert_eq!(seeding.downloaded_bytes, 0);
    assert_eq!(seeding.seed_ratio, Some(0.0));

    // Появилась отдача (в реальности — счётчик байт): ratio становится
    // бесконечным и лимит 1.0 обязан сработать.
    manager
        .set_transfer_totals(id, 5_000_000, 0)
        .expect("счётчики записаны");
    let stopped = wait_for(&manager, id, |snap| snap.seed_limit_reached).await;
    assert_eq!(
        stopped.seed_ratio, None,
        "готовые данные: скачано 0, отдано > 0 — коэффициент бесконечен"
    );
    // D14: лимит не просто меняет флаг, а останавливает обмен.
    let paused = wait_for(&manager, id, |snap| {
        matches!(snap.state, torrent_core::TorrentState::Paused)
    })
    .await;
    assert!(paused.seed_limit_reached, "лимит остаётся сработавшим");

    // Ручное возобновление сбрасывает лимит и возвращает раздачу.
    manager.resume(id).expect("возобновление");
    let resumed = wait_for(&manager, id, |snap| {
        matches!(snap.state, torrent_core::TorrentState::Seeding)
    })
    .await;
    assert!(!resumed.seed_limit_reached, "resume сбрасывает лимит");
    manager.shutdown().await;
}

#[tokio::test]
async fn seed_off_in_settings_stops_exchange_after_complete() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let dir = temp.path().join("downloads");
    std::fs::create_dir_all(&dir).expect("каталог загрузки");
    let sample = Sample::new("seed-off", PIECE_LENGTH, &[("data.bin", 64 * 1024)]);
    write_data(&sample, &dir);

    let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let config = EngineConfig {
        download_dir: dir,
        listen_port: 0,
        dht: false,
        content_layout: ContentLayout::NoSubfolder,
        seed_after_complete: false,
        ..EngineConfig::default()
    };
    let manager = TorrentManager::new(config);
    let id = manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("торрент добавлен");
    manager.recheck(id).expect("проверка запущена");

    // «Не раздавать после завершения»: обмен обязан встать, а состояние
    // остаётся «завершён» (как у qBittorrent), а не «пауза».
    let stopped = wait_for(&manager, id, |snap| {
        matches!(snap.state, torrent_core::TorrentState::Completed)
    })
    .await;
    assert_eq!(stopped.downloaded_bytes, 0);
    // Отдача больше не идёт: сессия остановлена, счётчик замирает.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let later = snapshot(&manager, id);
    assert_eq!(later.uploaded_bytes, stopped.uploaded_bytes);
    manager.shutdown().await;
}

#[tokio::test]
async fn pause_and_remove_send_stopped_to_tracker() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let sample = Sample::new("stopped", PIECE_LENGTH, &[("data.bin", 32 * 1024)]);
    let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let config = EngineConfig {
        download_dir: temp.path().join("downloads"),
        listen_port: 0,
        dht: false,
        content_layout: ContentLayout::NoSubfolder,
        ..EngineConfig::default()
    };
    let manager = TorrentManager::new(config);
    let id = manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("торрент добавлен");

    // Первый announce — started.
    let deadline = Instant::now() + Duration::from_secs(20);
    while tracker.announce_count() == 0 {
        assert!(Instant::now() < deadline, "трекер не получил announce");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        tracker.log().last().and_then(|entry| entry.event.clone()),
        Some("started".to_string())
    );

    // Пауза — stopped.
    manager.pause(id).expect("пауза");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if tracker
            .log()
            .iter()
            .any(|entry| entry.event.as_deref() == Some("stopped"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "event=stopped не отправлен");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Возобновление — снова started (трекер должен видеть нас активными).
    manager.resume(id).expect("возобновление");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let started_again = tracker
            .log()
            .iter()
            .filter(|entry| entry.event.as_deref() == Some("started"))
            .count();
        if started_again >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "после resume нет повторного started"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    manager.shutdown().await;
}
