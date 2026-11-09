//! E2E-тесты выбора файлов (FEATURE-1.1, «Ядро»).
//!
//! Проверяется то, чего до 1.1 не было:
//! 1. набор нужных кусков строится по выбранным файлам и виден в снимке;
//! 2. загрузка считается завершённой по выбранным файлам, чужие куски не качаются;
//! 3. смена выбора на лету пересчитывает куски, состояние и событие
//!    `StateChanged`, а уже скачанные файлы остаются на диске;
//! 4. `left` в announce считается по нужным байтам, а не по всему торренту;
//! 5. действующий лимит скорости — минимум из глобального и локального (0 — без);
//! 6. `preview_file`/`preview_magnet` отдают метаданные, не добавляя торрент.
//!
//! Сеть полностью локальная: моки биндятся на `127.0.0.1:0`, движок слушает
//! `listen_port: 0`. Интернет не нужен.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    hex, require_snapshot, wait_for, Corruption, MockMetadataPeer, MockSeeder, MockTracker, Sample,
    TrackerAnnounce, TrackerReply, PIECE_LENGTH,
};
use torrent_core::engine::EngineEvent;
use torrent_core::{AddOptions, EngineConfig, EngineError, TorrentManager, TorrentState};

/// Конфиг движка для теста: свой каталог загрузки, случайный порт прослушки.
fn test_config(download_dir: &std::path::Path) -> EngineConfig {
    EngineConfig {
        download_dir: download_dir.to_path_buf(),
        listen_port: 0,
        // DHT ходит в настоящую сеть — в тестах он не нужен и только шумит.
        dht: false,
        ..EngineConfig::default()
    }
}

/// Торрент из трёх файлов ровно по одному куску: выбор файла — это выбор куска.
fn three_files() -> Arc<Sample> {
    Arc::new(Sample::new(
        "torrenta-selection",
        PIECE_LENGTH,
        &[
            ("a.bin", PIECE_LENGTH as usize),
            ("b.bin", PIECE_LENGTH as usize),
            ("c.bin", PIECE_LENGTH as usize),
        ],
    ))
}

/// Ждёт в журнале mock-трекера announce, удовлетворяющий условию.
async fn wait_announce<F>(
    tracker: &MockTracker,
    limit: Duration,
    what: &str,
    mut found: F,
) -> TrackerAnnounce
where
    F: FnMut(&[TrackerAnnounce]) -> Option<TrackerAnnounce>,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let log = tracker.log();
        if let Some(announce) = found(&log) {
            return announce;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "не дождались announce «{what}», журнал трекера: {log:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Выбор файлов сразу задаёт нужные байты и нужные куски.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_with_files_limits_wanted_set() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let sample = three_files();
    let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let manager = TorrentManager::new(test_config(&temp.path().join("downloads")));

    let id = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                save_path: None,
                files: Some(vec![1]),
            },
        )
        .await
        .expect("торрент с выбором файла должен добавиться");

    let snapshot = require_snapshot(&manager, id);
    assert_eq!(
        snapshot.total_bytes,
        3 * PIECE_LENGTH as u64,
        "полный размер торрента не зависит от выбора файлов"
    );
    assert_eq!(
        snapshot.wanted_bytes, PIECE_LENGTH as u64,
        "нужные байты — это сумма выбранных файлов"
    );
    assert_eq!(snapshot.piece_count, 3, "кусков в торренте три");
    assert_eq!(
        snapshot.progress, 0.0,
        "пока ничего не скачано, прогресс по нужным файлам нулевой"
    );
    assert_eq!(snapshot.eta_seconds, None, "без скорости ETA неизвестен");
    assert_eq!(snapshot.files.len(), 3, "снимок перечисляет все файлы");
    assert!(
        snapshot.files[1].selected,
        "выбранный файл должен быть помечен в снимке"
    );
    assert!(
        !snapshot.files[0].selected && !snapshot.files[2].selected,
        "невыбранные файлы не должны быть помечены как выбранные"
    );
    assert!(
        matches!(snapshot.state, TorrentState::Downloading),
        "после добавления ожидалась загрузка, получено {:?}",
        snapshot.state
    );
}

/// Пустой и кривой список файлов — это `EngineError::Selection`, торрент не добавляется.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_or_invalid_selection_is_rejected() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let sample = three_files();
    let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let manager = TorrentManager::new(test_config(&temp.path().join("downloads")));

    let empty = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                save_path: None,
                files: Some(Vec::new()),
            },
        )
        .await;
    assert!(
        matches!(empty, Err(EngineError::Selection)),
        "пустой выбор обязан отвергаться, получено {empty:?}"
    );

    let out_of_range = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                save_path: None,
                files: Some(vec![0, 3]),
            },
        )
        .await;
    assert!(
        matches!(out_of_range, Err(EngineError::Selection)),
        "индекс за пределами списка файлов обязан отвергаться, получено {out_of_range:?}"
    );
    assert!(
        manager.snapshot().is_empty(),
        "после отвергнутых выборов в списке не должно остаться торрентов"
    );

    // Без опций (старый путь) выбираются все файлы.
    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("торрент без выбора файлов должен добавиться");
    let all = require_snapshot(&manager, id);
    assert_eq!(
        all.wanted_bytes, all.total_bytes,
        "без выбора нужны все байты торрента"
    );
    assert!(
        all.files.iter().all(|file| file.selected),
        "без выбора все файлы помечены выбранными"
    );

    let cleared = manager.set_file_selection(id, Vec::new());
    assert!(
        matches!(cleared, Err(EngineError::Selection)),
        "пустой выбор при смене набора обязан отвергаться, получено {cleared:?}"
    );
    let unknown = manager.set_file_selection(9_999, vec![0]);
    assert!(
        matches!(unknown, Err(EngineError::NotFound(_))),
        "неизвестный id обязан давать NotFound, получено {unknown:?}"
    );
    assert_eq!(
        require_snapshot(&manager, id).wanted_bytes,
        all.wanted_bytes,
        "отвергнутая смена выбора не должна менять текущий набор"
    );
}

/// Частичный выбор завершается, когда скачаны только нужные куски.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_selection_completes_without_other_files() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = three_files();
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                save_path: None,
                files: Some(vec![1]),
            },
        )
        .await
        .expect("торрент с выбором файла должен добавиться");

    let done = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "выбранный файл скачан, торрент завершён",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;

    assert_eq!(
        done.total_bytes,
        3 * PIECE_LENGTH as u64,
        "полный размер торрента не меняется от выбора"
    );
    assert_eq!(
        done.wanted_bytes, PIECE_LENGTH as u64,
        "нужные байты — только выбранный файл"
    );
    assert_eq!(
        done.progress, 1.0,
        "прогресс по нужным файлам должен дойти до единицы"
    );
    assert_eq!(done.pieces_have, 1, "скачаться должен ровно один кусок");
    assert_eq!(
        done.downloaded_bytes, PIECE_LENGTH as u64,
        "счётчик трафика — только скачанный нужный кусок"
    );
    assert_eq!(
        done.files[1].downloaded, PIECE_LENGTH as u64,
        "выбранный файл скачан целиком"
    );
    assert_eq!(
        done.files[0].downloaded, 0,
        "невыбранный файл не должен качаться"
    );
    assert_eq!(
        done.files[2].downloaded, 0,
        "невыбранный файл не должен качаться"
    );
    assert_eq!(done.eta_seconds, None, "докачивать нечего — ETA нет");

    // Данные выбранного файла действительно лежат на диске.
    let relative = &done.files[1].path;
    assert!(
        relative.ends_with("b.bin"),
        "путь выбранного файла в снимке должен оканчиваться его именем, получено {relative:?}"
    );
    let expected = &sample.files[1].1;
    let path = download_dir.join(relative);
    let actual = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("не удалось прочитать {}: {err}", path.display()));
    assert!(
        actual == *expected,
        "содержимое выбранного файла должно совпасть с исходником"
    );
}

/// Смена выбора на лету пересчитывает куски, состояние и событие, данные остаются.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selection_change_switches_wanted_set_and_state() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let sample = three_files();
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&temp.path().join("downloads")));
    let mut events = manager.subscribe();

    let id = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                save_path: None,
                files: Some(vec![0]),
            },
        )
        .await
        .expect("торрент с выбором файла должен добавиться");
    let first = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "первый выбранный файл скачан",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;
    assert_eq!(first.files[0].downloaded, PIECE_LENGTH as u64);

    // Переключаемся на другой файл: старый кусок уже не нужен, нужен новый.
    manager
        .set_file_selection(id, vec![2])
        .expect("смена выбора должна принять корректный список файлов");
    let switched = require_snapshot(&manager, id);
    assert_eq!(
        switched.wanted_bytes, PIECE_LENGTH as u64,
        "нужные байты пересчитаны под новый выбор"
    );
    assert!(
        switched.files[2].selected && !switched.files[0].selected,
        "снимок должен сразу видеть новый набор файлов"
    );
    assert_eq!(
        switched.state,
        TorrentState::Downloading,
        "после смены выбора торрент снова качает"
    );

    let mut notifications = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EngineEvent::StateChanged {
            id: event_id,
            state,
        } = event
        {
            if event_id == id {
                notifications.push(state);
            }
        }
    }
    assert!(
        notifications.contains(&TorrentState::Downloading),
        "смена выбора обязана уведомить о новом состоянии, получено {notifications:?}"
    );

    let done = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "новый выбранный файл скачан",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;
    assert_eq!(done.progress, 1.0, "прогресс снова дошёл до единицы");
    assert_eq!(done.pieces_have, 2, "к старому куску добавился новый");
    assert_eq!(
        done.files[2].downloaded, PIECE_LENGTH as u64,
        "новый выбранный файл скачан"
    );
    assert_eq!(
        done.files[0].downloaded, PIECE_LENGTH as u64,
        "данные файла, который перестал быть нужным, остаются на диске"
    );
    assert_eq!(
        done.files[1].downloaded, 0,
        "файл, который не выбирали, так и не качался"
    );
}

/// `left` в announce считается по нужным байтам и обновляется после смены выбора.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tracker_left_counts_wanted_bytes() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let sample = three_files();
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&temp.path().join("downloads")));

    let id = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                save_path: None,
                files: Some(vec![1]),
            },
        )
        .await
        .expect("торрент с выбором файла должен добавиться");

    // Первый announce: started и остаток только по выбранному файлу (а не по 96 КиБ).
    let started = wait_announce(&tracker, Duration::from_secs(15), "started", |log| {
        log.iter()
            .find(|announce| announce.event.as_deref() == Some("started"))
            .cloned()
    })
    .await;
    assert_eq!(
        started.left, PIECE_LENGTH as u64,
        "в объявлении остаток должен считаться по выбранным файлам"
    );

    wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "выбранный файл скачан",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;
    // Переключаем выбор: теперь нужен другой кусок, и остаток снова не ноль.
    manager
        .set_file_selection(id, vec![2])
        .expect("смена выбора должна принять корректный список файлов");

    // Следующее объявление (интервал мока — 30 с) идёт уже по новому набору:
    // к этому моменту скачаны два куска из трёх, но нужные байты — ноль.
    let completed = wait_announce(&tracker, Duration::from_secs(45), "completed", |log| {
        log.iter()
            .find(|announce| announce.event.as_deref() == Some("completed") && announce.left == 0)
            .cloned()
    })
    .await;
    assert_eq!(
        completed.left, 0,
        "остаток по нужным файлам равен нулю, хотя скачаны не все куски торрента"
    );
}

/// Завершение из-за сужения выбора не ждёт интервала трекера: объявление
/// `event=completed` уходит сразу (мок-трекер отдаёт интервал 30 с).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn narrowing_selection_announces_completed_without_waiting_interval() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let sample = three_files();
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&temp.path().join("downloads")));

    let id = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                save_path: None,
                files: Some(vec![0, 1]),
            },
        )
        .await
        .expect("торрент с выбором файлов должен добавиться");

    // Дожидаемся первого announce: цикл трекера уже спит и примет будильник.
    wait_announce(&tracker, Duration::from_secs(15), "started", |log| {
        log.iter()
            .find(|announce| announce.event.as_deref() == Some("started"))
            .cloned()
    })
    .await;

    // Первый выбранный файл целиком на диске.
    wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "первый файл скачан",
        |item| item.files[0].downloaded == PIECE_LENGTH as u64,
    )
    .await;

    // Сужаем выбор до уже скачанного файла: нужных байт не осталось, торрент готов.
    manager
        .set_file_selection(id, vec![0])
        .expect("смена выбора должна принять корректный список файлов");

    // Без побудки объявление пришло бы только через интервал мока (30 с).
    let completed = wait_announce(&tracker, Duration::from_secs(10), "completed", |log| {
        log.iter()
            .find(|announce| announce.event.as_deref() == Some("completed") && announce.left == 0)
            .cloned()
    })
    .await;
    assert_eq!(
        completed.left, 0,
        "завершение по смене выбора объявляется с нулевым остатком"
    );

    manager.shutdown().await;
}

/// Побудка не теряется, пока ответ трекера ещё в пути.
///
/// На медленном CI объявление завершения терялось ровно в этом окне: движок
/// ждёт ответа, уведомление уходит в пустоту, и `completed` приходит только
/// через интервал (30 с). Здесь окно делается явным: трекер отвечает с
/// задержкой 1.5 с, сужение выбора будит цикл, пока ответ ещё не отдан, а
/// перепроверка готового торрента будит его ещё раз — уже внутри следующего
/// запроса. Без починки объявление ждало бы интервал мока (30 с).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wake_is_not_lost_while_announce_is_in_flight() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let sample = three_files();
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    // Каждый ответ идёт 1.5 с: цикл объявлений занят запросом, а не ждёт.
    let tracker = MockTracker::start_with_interval_and_delay(
        TrackerReply::Peers(vec![seeder.addr]),
        30,
        Duration::from_millis(1500),
    )
    .await;
    let manager = TorrentManager::new(test_config(&temp.path().join("downloads")));

    // Выбраны два файла, и оба целиком лежат на диске к моменту сужения выбора.
    let id = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                save_path: None,
                files: Some(vec![0, 1]),
            },
        )
        .await
        .expect("торрент с выбором файлов должен добавиться");

    wait_announce(&tracker, Duration::from_secs(20), "started", |log| {
        log.iter()
            .find(|announce| announce.event.as_deref() == Some("started"))
            .cloned()
    })
    .await;
    wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "выбранные файлы скачаны и проверены",
        |item| item.pieces_have == 2 && item.files[1].downloaded == PIECE_LENGTH as u64,
    )
    .await;

    // Сужение до первого файла делает торрент готовым: побудка уходит в цикл,
    // который сейчас ждёт ответа трекера (в полёте — до 1.5 с).
    manager
        .set_file_selection(id, vec![0])
        .expect("смена выбора должна принять корректный список файлов");
    wait_announce(
        &tracker,
        Duration::from_secs(20),
        "completed в пути",
        |log| {
            log.iter()
                .find(|announce| announce.event.as_deref() == Some("completed"))
                .cloned()
        },
    )
    .await;

    // Второй раз будим цикл внутри того же запроса: перепроверка готового
    // торрента завершается «скачано» и просит объявление.
    let sent = tracker.log().len();
    manager
        .recheck(id)
        .expect("перепроверка готового торрента должна запуститься");

    // Побудка сработала → следующее объявление сразу после ответа трекера.
    // Потерянная побудка ждала бы интервал объявлений 30 с. Нижняя граница
    // проверяет, что ответ действительно был в пути: цикл не мог отправить
    // запрос раньше, чем трекер отдал предыдущий ответ (≈1.4 с).
    let started_at = std::time::Instant::now();
    let next = wait_announce(
        &tracker,
        Duration::from_secs(8),
        "следующее после побудки",
        |log| log.get(sent).cloned(),
    )
    .await;
    let elapsed = started_at.elapsed();
    assert_eq!(
        next.left, 0,
        "готовый торрент объявляет нулевой остаток (запись #{sent}: {next:?})"
    );
    assert!(
        elapsed >= Duration::from_secs(1),
        "объявление пришло через {elapsed:?}: ответа трекера ещё не было, окно не воспроизведено"
    );

    manager.shutdown().await;
}

/// Действующий лимит — минимум из глобального и локального, 0 значит «без лимита».
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn effective_limit_is_min_of_global_and_torrent() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let sample = three_files();
    let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let mut config = test_config(&temp.path().join("downloads"));
    config.download_limit_kib = 50;
    config.upload_limit_kib = 0;
    let manager = TorrentManager::new(config);

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("торрент должен добавиться");

    let global_only = require_snapshot(&manager, id);
    assert_eq!(
        global_only.down_limit_kib, 50,
        "глобальный лимит действует, если у торрента своего нет"
    );
    assert_eq!(
        global_only.up_limit_kib, 0,
        "нулевой глобальный лимит отдачи значит «без ограничения»"
    );

    // Локальный лимит выше глобального — действует глобальный.
    manager
        .set_limits(id, 100, 7)
        .expect("лимиты существующего торрента должны применяться");
    let capped = require_snapshot(&manager, id);
    assert_eq!(
        capped.down_limit_kib, 50,
        "действующий лимит — минимум из глобального и локального"
    );
    assert_eq!(
        capped.up_limit_kib, 7,
        "локальный лимит отдачи действует, когда глобального нет"
    );

    // Локальный лимит ниже глобального — действует локальный.
    manager
        .set_limits(id, 10, 5)
        .expect("лимиты существующего торрента должны применяться");
    let tightened = require_snapshot(&manager, id);
    assert_eq!(
        tightened.down_limit_kib, 10,
        "строгий локальный лимит важнее"
    );
    assert_eq!(
        tightened.up_limit_kib, 5,
        "строгий локальный лимит отдачи важнее"
    );

    // Без глобальных лимитов действуют локальные.
    manager
        .set_limits(id, 64, 12)
        .expect("лимиты существующего торрента должны применяться");
    let other = {
        let mut config = test_config(&temp.path().join("downloads-2"));
        config.download_limit_kib = 0;
        config.upload_limit_kib = 0;
        let manager = TorrentManager::new(config);
        let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
        let id = manager
            .add_file(sample.torrent_bytes(&tracker.url()), None)
            .await
            .expect("торрент должен добавиться");
        manager
            .set_limits(id, 64, 12)
            .expect("лимиты существующего торрента должны применяться");
        let snapshot = require_snapshot(&manager, id);
        assert_eq!(
            snapshot.down_limit_kib, 64,
            "при нулевом глобальном лимите действует локальный"
        );
        assert_eq!(
            snapshot.up_limit_kib, 12,
            "при нулевом глобальном лимите действует локальный лимит отдачи"
        );
        snapshot
    };
    assert_eq!(other.wanted_bytes, other.total_bytes);
}

/// `preview_file` разбирает `.torrent` и ничего не добавляет в менеджер.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preview_file_exposes_metadata() {
    let sample = three_files();
    let announce = "http://tracker.local/announce";
    let preview = TorrentManager::preview_file(&sample.torrent_bytes(announce))
        .expect("корректный .torrent должен разбираться");

    assert_eq!(preview.name, sample.name);
    assert_eq!(
        preview.info_hash,
        hex(&sample.info_hash),
        "info-hash в предпросмотре должен совпадать с торрентом"
    );
    assert_eq!(preview.total_bytes, 3 * PIECE_LENGTH as u64);
    assert_eq!(preview.piece_length, PIECE_LENGTH);
    assert_eq!(preview.piece_count, 3);
    assert_eq!(preview.files.len(), 3);
    assert_eq!(preview.files[0].index, 0);
    assert!(
        preview.files[0].path.ends_with("a.bin"),
        "путь файла в предпросмотре должен оканчиваться именем файла, получено {:?}",
        preview.files[0].path
    );
    assert_eq!(preview.files[1].length, PIECE_LENGTH as u64);
    assert_eq!(preview.trackers, vec![announce.to_string()]);

    let broken = TorrentManager::preview_file(b"not a torrent at all");
    assert!(
        matches!(broken, Err(EngineError::Metainfo(_))),
        "мусор должен давать Metainfo-ошибку, получено {broken:?}"
    );
}

/// `preview_magnet` достаёт метаданные у пира, не добавляя торрент в список.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preview_magnet_fetches_metadata_without_adding() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let sample = three_files();
    let metadata_peer = MockMetadataPeer::start(sample.info_bytes.clone(), sample.info_hash).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![metadata_peer.addr])).await;
    let manager = TorrentManager::new(test_config(&temp.path().join("downloads")));

    let preview = manager
        .preview_magnet(&sample.magnet_link(&tracker.url()), Duration::from_secs(20))
        .await
        .expect("метаданные должны подтянуться у mock-пира");
    assert_eq!(preview.name, sample.name);
    assert_eq!(preview.info_hash, hex(&sample.info_hash));
    assert_eq!(preview.total_bytes, 3 * PIECE_LENGTH as u64);
    assert_eq!(preview.files.len(), 3);
    assert!(
        manager.snapshot().is_empty(),
        "предпросмотр не должен добавлять торрент в список"
    );

    // Magnet без трекеров: метаданные брать негде — сразу понятная ошибка.
    let bare = format!("magnet:?xt=urn:btih:{}", hex(&sample.info_hash));
    let no_trackers = manager.preview_magnet(&bare, Duration::from_secs(5)).await;
    assert!(
        matches!(no_trackers, Err(EngineError::Magnet(_))),
        "magnet без трекеров обязан давать Magnet-ошибку, получено {no_trackers:?}"
    );

    // Трекер без пиров: укладываемся в таймаут и ничего не оставляем после себя.
    let empty_tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let timed_out = manager
        .preview_magnet(
            &sample.magnet_link(&empty_tracker.url()),
            Duration::from_millis(1_200),
        )
        .await;
    assert!(
        matches!(timed_out, Err(EngineError::Io(_))),
        "без пиров предпросмотр обязан истечь по таймауту, получено {timed_out:?}"
    );
    assert!(
        manager.snapshot().is_empty(),
        "после неудачного предпросмотра список торрентов должен остаться пустым"
    );
}
