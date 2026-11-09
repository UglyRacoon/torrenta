//! E2E: очередь активных торрентов, перепроверка кусков и приоритеты файлов.
//!
//! Всё, что проверяется здесь, видно пользователю в интерфейсе:
//!
//! * лимиты `max_active_downloads`/`max_active_seeds` держат «лишние» торренты
//!   в [`TorrentState::Queued`], а пауза пользователя слот не занимает;
//! * `TorrentManager::recheck` сверяет куски на диске, показывает прогресс в
//!   `pieces_have` и возвращает торрент в строй или на докачку;
//! * `FilePriority` реально управляет тем, какие файлы качаются и в каком
//!   порядке, а `FileView` показывает приоритет в снимке;
//! * завершённый торрент держит файлы открытыми только для чтения, поэтому
//!   Windows не запрещает запустить скачанный `.exe` другой программой.
//!
//! Сеть локальная: mock-трекер и mock-сидер биндятся на `127.0.0.1:0`.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{
    require_snapshot, wait_for, Corruption, MockSeeder, MockTracker, Sample, TrackerReply,
    PIECE_LENGTH,
};
use torrent_core::{
    AddOptions, EngineConfig, EngineError, EngineEvent, FilePriority, TorrentManager, TorrentState,
};

/// Конфиг движка для теста: свой каталог загрузки, случайный порт прослушки.
fn test_config(download_dir: &Path) -> EngineConfig {
    EngineConfig {
        download_dir: download_dir.to_path_buf(),
        listen_port: 0,
        // DHT ходит в настоящую сеть — в тестах он не нужен и только шумит.
        dht: false,
        ..EngineConfig::default()
    }
}

/// Ждёт в потоке событий движка смену состояния на `wanted`.
///
/// Проверки состояния на диске слишком быстрые, чтобы ловить `Checking`
/// опросом снимков, а событие `StateChanged` отправляется синхронно.
async fn wait_for_state_event(
    events: &mut tokio::sync::broadcast::Receiver<EngineEvent>,
    wanted: TorrentState,
    limit: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(EngineEvent::StateChanged { state, .. })) if state == wanted => return true,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return false,
            Err(_) => {}
        }
    }
    false
}

/// Сколько торрентов сейчас в указанном состоянии.
fn count_state(manager: &Arc<TorrentManager>, state: TorrentState) -> usize {
    manager
        .snapshot()
        .into_iter()
        .filter(|item| item.state == state)
        .count()
}

/// Два торрента по одному куску и трекер без пиров: качать нечего, поэтому
/// состояния стабильны и тест не гонкует с реальной загрузкой.
async fn add_idle_pair(
    manager: &Arc<TorrentManager>,
    tracker_url: &str,
    first: &Sample,
    second: &Sample,
) -> (u64, u64) {
    let a = manager
        .add_file(first.torrent_bytes(tracker_url), None)
        .await
        .expect("движок не принял первый .torrent");
    let b = manager
        .add_file(second.torrent_bytes(tracker_url), None)
        .await
        .expect("движок не принял второй .torrent");
    (a, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_limits_active_downloads() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("queue");
    let first = Arc::new(Sample::new(
        "queue-first",
        PIECE_LENGTH,
        &[("first.bin", PIECE_LENGTH as usize)],
    ));
    let second = Arc::new(Sample::new(
        "queue-second",
        PIECE_LENGTH,
        &[("second.bin", PIECE_LENGTH as usize)],
    ));
    // Пиров нет: оба торрента «качаются» бесконечно, завершение не мешает
    // проверять выдачу слотов.
    let tracker = MockTracker::start(TrackerReply::Peers(Vec::new())).await;
    let manager = TorrentManager::new(EngineConfig {
        max_active_downloads: 1,
        ..test_config(&download_dir)
    });

    let (a, b) = add_idle_pair(&manager, &tracker.url(), &first, &second).await;

    wait_for(
        &manager,
        a,
        Duration::from_secs(20),
        "первый торрент занял единственный слот загрузки",
        |item| item.state == TorrentState::Downloading,
    )
    .await;
    let queued = wait_for(
        &manager,
        b,
        Duration::from_secs(20),
        "второй торрент ждёт слот в очереди",
        |item| item.state == TorrentState::Queued,
    )
    .await;
    assert_eq!(
        queued.state,
        TorrentState::Queued,
        "лишний сверх лимита торрент должен стоять в очереди"
    );
    assert_eq!(
        require_snapshot(&manager, a).state,
        TorrentState::Downloading,
        "первый торрент должен остаться активным"
    );

    // Освободившийся слот забирает следующий в очереди.
    manager.pause(a).expect("пауза первого торрента");
    wait_for(
        &manager,
        b,
        Duration::from_secs(20),
        "второй торрент занял освободившийся слот",
        |item| item.state == TorrentState::Downloading,
    )
    .await;
    assert_eq!(
        require_snapshot(&manager, a).state,
        TorrentState::Paused,
        "первый торрент стоит на паузе пользователя"
    );

    // Вернувшийся торрент встаёт в очередь по времени добавления: первый
    // добавлен раньше, значит слот достаётся ему, а второй ждёт.
    manager.resume(a).expect("возобновление первого торрента");
    wait_for(
        &manager,
        b,
        Duration::from_secs(20),
        "второй торрент вернулся в очередь после возврата первого",
        |item| item.state == TorrentState::Queued,
    )
    .await;
    assert_eq!(
        require_snapshot(&manager, a).state,
        TorrentState::Downloading,
        "возобновлённый торрент с более ранним добавлением получает слот"
    );

    // Лимит 0 — без ограничения: слоты достаются всем.
    let mut config = manager.config();
    config.max_active_downloads = 0;
    manager.set_config(config);
    wait_for(
        &manager,
        b,
        Duration::from_secs(20),
        "снятие лимита вывело второй торрент из очереди",
        |item| item.state == TorrentState::Downloading,
    )
    .await;
    assert_eq!(
        require_snapshot(&manager, a).state,
        TorrentState::Downloading,
        "без лимита первый торрент тоже активен"
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_paused_torrents_do_not_take_slots() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("paused-slots");
    let first = Arc::new(Sample::new(
        "paused-first",
        PIECE_LENGTH,
        &[("first.bin", PIECE_LENGTH as usize)],
    ));
    let second = Arc::new(Sample::new(
        "paused-second",
        PIECE_LENGTH,
        &[("second.bin", PIECE_LENGTH as usize)],
    ));
    let tracker = MockTracker::start(TrackerReply::Peers(Vec::new())).await;
    let manager = TorrentManager::new(EngineConfig {
        max_active_downloads: 1,
        ..test_config(&download_dir)
    });

    let (a, b) = add_idle_pair(&manager, &tracker.url(), &first, &second).await;
    wait_for(
        &manager,
        a,
        Duration::from_secs(20),
        "первый торрент качается",
        |item| item.state == TorrentState::Downloading,
    )
    .await;
    wait_for(
        &manager,
        b,
        Duration::from_secs(20),
        "второй торрент в очереди",
        |item| item.state == TorrentState::Queued,
    )
    .await;

    // Пауза пользователя — это `Paused`, а не `Queued`: место в очереди
    // освобождается для других.
    manager.pause(b).expect("пауза второго торрента");
    wait_for(
        &manager,
        b,
        Duration::from_secs(20),
        "второй торрент на паузе",
        |item| item.state == TorrentState::Paused,
    )
    .await;
    assert_eq!(
        require_snapshot(&manager, a).state,
        TorrentState::Downloading,
        "пауза второго торрента не должна останавливать первый"
    );

    // Оба на паузе — активных загрузок нет ни одной.
    manager.pause(a).expect("пауза первого торрента");
    wait_for(
        &manager,
        a,
        Duration::from_secs(20),
        "первый торрент на паузе",
        |item| item.state == TorrentState::Paused,
    )
    .await;
    assert_eq!(
        require_snapshot(&manager, b).state,
        TorrentState::Paused,
        "второй торрент остался на паузе пользователя"
    );

    // Возобновление первого занимает слот, а второй по-прежнему на паузе:
    // пользовательская пауза важнее очереди.
    manager.resume(a).expect("возобновление первого торрента");
    wait_for(
        &manager,
        a,
        Duration::from_secs(20),
        "первый торрент снова качается",
        |item| item.state == TorrentState::Downloading,
    )
    .await;
    assert_eq!(
        require_snapshot(&manager, b).state,
        TorrentState::Paused,
        "возобновление первого торрента не снимает паузу со второго"
    );

    // Явное возобновление второго ставит его в очередь за первым.
    manager.resume(b).expect("возобновление второго торрента");
    wait_for(
        &manager,
        b,
        Duration::from_secs(20),
        "второй торрент встал в очередь",
        |item| item.state == TorrentState::Queued,
    )
    .await;

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seed_slots_are_limited_separately() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("seeds");
    let first = Arc::new(Sample::new(
        "seed-first",
        PIECE_LENGTH,
        &[("first.bin", PIECE_LENGTH as usize)],
    ));
    let second = Arc::new(Sample::new(
        "seed-second",
        PIECE_LENGTH,
        &[("second.bin", PIECE_LENGTH as usize)],
    ));
    let seeder_first = MockSeeder::start(first.clone(), Corruption::None).await;
    let seeder_second = MockSeeder::start(second.clone(), Corruption::None).await;
    let tracker_first = MockTracker::start(TrackerReply::Peers(vec![seeder_first.addr])).await;
    let tracker_second = MockTracker::start(TrackerReply::Peers(vec![seeder_second.addr])).await;
    // Загрузки не ограничены (иначе второй торрент не успеет скачаться), а
    // раздача — один слот на двоих.
    let manager = TorrentManager::new(EngineConfig {
        max_active_downloads: 0,
        max_active_seeds: 1,
        ..test_config(&download_dir)
    });

    let a = manager
        .add_file(first.torrent_bytes(&tracker_first.url()), None)
        .await
        .expect("движок не принял первый .torrent");
    let _second = manager
        .add_file(second.torrent_bytes(&tracker_second.url()), None)
        .await
        .expect("движок не принял второй .torrent");

    // Оба скачались, но раздаёт только один: второй ждёт слот раздачи.
    let guard = manager.clone();
    wait_for(
        &manager,
        a,
        Duration::from_secs(60),
        "один торрент раздаёт, второй ждёт слот раздачи",
        move |_| {
            count_state(&guard, TorrentState::Seeding) == 1
                && count_state(&guard, TorrentState::Queued) == 1
        },
    )
    .await;

    // Снятие лимита раздач выпускает второй торрент из очереди.
    let mut config = manager.config();
    config.max_active_seeds = 0;
    manager.set_config(config);
    let guard = manager.clone();
    wait_for(
        &manager,
        a,
        Duration::from_secs(30),
        "лимит раздач снят — раздают оба",
        move |_| count_state(&guard, TorrentState::Seeding) == 2,
    )
    .await;

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recheck_rejects_unknown_and_unready_torrents() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("recheck-errors");
    let sample = Arc::new(Sample::new(
        "recheck-unready",
        PIECE_LENGTH,
        &[("payload.bin", PIECE_LENGTH as usize)],
    ));
    // Трекер отказывает: метаданные magnet получить негде, рантайма нет.
    let tracker = MockTracker::start(TrackerReply::Failure("нет пиров".to_string())).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    assert!(
        matches!(manager.recheck(9999), Err(EngineError::NotFound(9999))),
        "перепроверка несуществующего торрента должна давать NotFound"
    );

    let id = manager
        .add_magnet(&sample.magnet_link(&tracker.url()), None)
        .await
        .expect("движок не принял корректную magnet-ссылку");
    assert!(
        matches!(manager.recheck(id), Err(EngineError::NotReady)),
        "magnet без метаданных перепроверять нечего — ожидался NotReady"
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recheck_verifies_finished_torrent() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("recheck-ok");
    let sample = Arc::new(Sample::new(
        "recheck-ok",
        PIECE_LENGTH,
        &[("data.bin", PIECE_LENGTH as usize * 4)],
    ));
    assert_eq!(sample.piece_count(), 4, "ожидалось 4 куска");
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");
    wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "загрузка завершилась",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;
    // Сидер уходит: перепроверка не должна «починиться» сетью, иначе
    // результат перестанет быть детерминированным.
    drop(seeder);

    let mut events = manager.subscribe();
    manager.recheck(id).expect("перепроверка началась");
    assert!(
        wait_for_state_event(&mut events, TorrentState::Checking, Duration::from_secs(10)).await,
        "перепроверка должна начинаться с состояния Checking"
    );

    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "перепроверка закончилась и торрент вернулся в раздачу",
        |item| {
            matches!(item.state, TorrentState::Seeding | TorrentState::Completed)
                && item.pieces_have == item.piece_count
        },
    )
    .await;
    assert_eq!(
        snapshot.pieces_have, snapshot.piece_count,
        "все куски на диске целые — после проверки они должны быть засчитаны"
    );
    assert!(
        (snapshot.progress - 1.0).abs() < f64::EPSILON,
        "прогресс после успешной проверки должен быть 1.0, получено {}",
        snapshot.progress
    );
    assert_eq!(snapshot.error, None, "ошибок после проверки быть не должно");

    // Данные на диске не тронуты.
    let on_disk = std::fs::read(download_dir.join("data.bin")).expect("файл должен читаться");
    assert!(
        on_disk == sample.files[0].1,
        "перепроверка не должна менять содержимое файла"
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recheck_marks_manually_corrupted_pieces_as_missing() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("recheck-broken");
    let sample = Arc::new(Sample::new(
        "recheck-broken",
        PIECE_LENGTH,
        &[("data.bin", PIECE_LENGTH as usize * 2)],
    ));
    assert_eq!(sample.piece_count(), 2, "ожидалось 2 куска");
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");
    wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "загрузка завершилась",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;
    drop(seeder);

    // «Другая программа» портит начало файла: первый кусок перестаёт сходиться
    // с SHA-1, а второй остаётся целым.
    let path = download_dir.join("data.bin");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("файл должен открываться на запись после завершения загрузки");
        file.write_all(&[0xFFu8; 64])
            .expect("не удалось испортить файл");
        file.sync_all().expect("не удалось сбросить порченые байты");
    }

    let mut events = manager.subscribe();
    manager.recheck(id).expect("перепроверка началась");
    assert!(
        wait_for_state_event(&mut events, TorrentState::Checking, Duration::from_secs(10)).await,
        "перепроверка должна начинаться с состояния Checking"
    );

    // Один кусок цел, второй — нет, значит торрент снова недокачан.
    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(30),
        "порченый кусок найден и торрент ушёл на докачку",
        |item| item.state == TorrentState::Downloading && item.pieces_have == 1,
    )
    .await;
    assert_eq!(
        snapshot.pieces_have, 1,
        "из двух кусков целым остался только второй"
    );
    assert!(
        snapshot.progress < 1.0,
        "прогресс недокачанного торрента должен быть меньше 1.0"
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_priorities_control_download_and_selection() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("priorities");
    // По одному куску на файл: приоритеты не пересекаются на общих кусках.
    let sample = Arc::new(Sample::new(
        "priorities",
        PIECE_LENGTH,
        &[
            ("keep.bin", PIECE_LENGTH as usize),
            ("drop.bin", PIECE_LENGTH as usize),
        ],
    ));
    assert_eq!(sample.piece_count(), 2, "ожидалось 2 куска");
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("движок не принял корректный .torrent");

    // Приоритеты выставляем на паузе: так движок не успеет запросить куски
    // пропускаемого файла до того, как узнает о новом выборе.
    manager.pause(id).expect("пауза торрента");

    // Число приоритетов обязано совпадать с числом файлов.
    assert!(
        matches!(
            manager.set_file_priorities(id, vec![FilePriority::High]),
            Err(EngineError::Selection)
        ),
        "неверное число приоритетов должно отвергаться"
    );

    manager
        .set_file_priorities(id, vec![FilePriority::High, FilePriority::Skip])
        .expect("приоритеты приняты");
    manager.resume(id).expect("возобновление торрента");

    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "скачался только файл с приоритетом High",
        |item| {
            matches!(item.state, TorrentState::Seeding | TorrentState::Completed)
                && item.pieces_have == 1
        },
    )
    .await;

    assert_eq!(
        snapshot.wanted_bytes, PIECE_LENGTH as u64,
        "к загрузке выбран только один файл"
    );
    assert_eq!(
        snapshot.piece_count, 2,
        "счётчик кусков считает весь торрент"
    );
    assert_eq!(
        snapshot.files.len(),
        2,
        "в снимке должны быть оба файла торрента"
    );
    assert_eq!(
        snapshot.files[0].priority,
        FilePriority::High,
        "приоритет первого файла должен попасть в снимок"
    );
    assert!(
        snapshot.files[0].selected,
        "файл с приоритетом High выбран к загрузке"
    );
    assert_eq!(
        snapshot.files[1].priority,
        FilePriority::Skip,
        "приоритет второго файла должен попасть в снимок"
    );
    assert!(
        !snapshot.files[1].selected,
        "файл с приоритетом Skip не выбран"
    );

    // Первый файл скачан полностью. Второй файл создаётся на диске заранее
    // (хранилище выделяет длину), но данные в него не пишутся: все байты
    // остаются нулевыми.
    let kept = std::fs::read(download_dir.join("keep.bin")).expect("keep.bin должен читаться");
    assert!(
        kept == sample.files[0].1,
        "выбранный файл должен быть скачан целиком"
    );
    let dropped = std::fs::read(download_dir.join("drop.bin")).unwrap_or_default();
    assert!(
        dropped.iter().all(|byte| *byte == 0),
        "пропущенный файл не должен содержать скачанных данных"
    );

    manager.shutdown().await;
}

/// Завершённый торрент не держит файлы на запись — Windows разрешает открыть
/// их другой программе (жалоба «скачанные файлы заняты другой программой»).
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finished_torrent_releases_file_handles() {
    use std::os::windows::fs::OpenOptionsExt;

    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("release");
    let sample = Arc::new(Sample::new(
        "release",
        PIECE_LENGTH,
        &[("payload.bin", 12_345)],
    ));
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");
    wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "загрузка завершилась",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;

    let path = download_dir.join("payload.bin");
    // «Другая программа» открывает файл на запись, разрешая только чтение и
    // удаление: если движок держит дескриптор на запись, Windows вернёт
    // ERROR_SHARING_VIOLATION.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    let foreign = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(&path);
    assert!(
        foreign.is_ok(),
        "завершённый торрент не должен удерживать файл на запись: {foreign:?}"
    );
    drop(foreign);

    // Обычное чтение тоже работает, данные целы.
    let bytes = std::fs::read(&path).expect("файл должен читаться другой программой");
    assert!(
        bytes == sample.files[0].1,
        "данные файла должны совпадать с исходными"
    );

    // Файл удаляется, пока торрент жив (раздача не держит его на запись).
    let removed = std::fs::remove_file(&path);
    assert!(
        removed.is_ok(),
        "файл раздачи не должен быть заблокирован от удаления: {removed:?}"
    );

    manager.shutdown().await;
}
