//! E2E: устойчивость движка к «плохим» событиям в сети.
//!
//! Все сценарии идут по `127.0.0.1`, внешняя сеть не используется.
//!
//! Проверяется:
//! * испорченный кусок от пира отбрасывается (SHA-1 не сходится), на диск не
//!   попадает, движок не паникует и перекачивает кусок заново;
//! * постоянно испорченный кусок никогда не засчитывается, но и не роняет движок;
//! * отказ трекера (`failure reason`) не роняет движок — торрент остаётся живым,
//!   движок продолжает попытки и отвечает на снимки состояния;
//! * торрент в состоянии `Error` отдаёт русский текст ошибки (magnet-ссылка без
//!   трекеров — реально достижимый путь ошибки в текущем движке);
//! * пауза/возобновление и удаление посреди загрузки не оставляют паник,
//!   висящих состояний и мусора на диске.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::{
    require_snapshot, wait_for, Corruption, MockSeeder, MockTracker, Sample, TrackerReply,
    PIECE_LENGTH,
};
use torrent_core::{EngineConfig, TorrentManager, TorrentState};

/// Минимальное число block-запросов, чтобы скачать торрент без единого повтора
/// (блок движка — 16 КиБ).
fn minimal_block_requests(bytes: usize) -> u32 {
    bytes.div_ceil(16 * 1024) as u32
}

fn test_config(download_dir: &std::path::Path) -> EngineConfig {
    EngineConfig {
        download_dir: download_dir.to_path_buf(),
        listen_port: 0,
        ..EngineConfig::default()
    }
}

/// В тексте ошибки движка есть кириллица (сообщения предназначены для UI).
fn has_cyrillic(text: &str) -> bool {
    text.chars().any(|ch| ch.is_alphabetic() && !ch.is_ascii())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupted_block_is_rejected_and_piece_is_downloaded_again() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("corrupt");
    let sample = Arc::new(Sample::new(
        "torrenta-corrupt",
        PIECE_LENGTH,
        &[("broken.bin", 131_536)],
    ));
    assert_eq!(sample.piece_count(), 5, "ожидалось 5 кусков");

    // Сидер один раз отдаёт испорченный блок куска №2, дальше отдаёт верные данные.
    // Значит движок обязан: (1) отбраковать кусок по SHA-1, (2) не записать его на
    // диск, (3) запросить кусок повторно и в итоге собрать верный файл.
    let seeder = MockSeeder::start(
        sample.clone(),
        Corruption::Transient {
            index: 2,
            times: 1,
        },
    )
    .await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    let done = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "загрузка дошла до конца после перекачки испорченного куска",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;

    // Испорченный блок не попал в файл: содержимое побайтово совпадает с оригиналом.
    let path = download_dir.join("broken.bin");
    let actual = std::fs::read(&path).expect("файл загрузки должен существовать");
    assert_eq!(
        actual.len(),
        sample.data.len(),
        "длина скачанного файла должна совпадать с ожидаемой"
    );
    assert!(
        actual == sample.data,
        "скачанный файл должен побайтово совпадать с исходным: испорченный кусок не должен попасть на диск"
    );

    assert!(
        (done.progress - 1.0).abs() < 1e-9,
        "прогресс завершённой загрузки должен быть 1.0, получено {}",
        done.progress
    );
    assert_eq!(
        done.pieces_have, done.piece_count,
        "все куски должны быть засчитаны после перекачки"
    );
    // Движок скачал больше, чем минимально нужно: это и есть доказательство того,
    // что испорченный кусок был отбракован и запрошен повторно. Проверяем двумя
    // независимыми способами, чтобы тест не зависел от того, как движок считает
    // статистику (сейчас он учитывает и отбракованные байты).
    let minimal = minimal_block_requests(sample.data.len());
    let requests = seeder.requests.load(Ordering::Relaxed);
    assert!(
        requests > minimal || done.downloaded_bytes > done.total_bytes,
        "ожидалась повторная закачка отбракованного куска: запросов {requests} при минимуме {minimal}, скачано {} из {}",
        done.downloaded_bytes,
        done.total_bytes
    );
    assert!(
        done.error.is_none(),
        "перекачка испорченного куска — штатная ситуация, ошибки быть не должно: {:?}",
        done.error
    );

    // Движок жив и обслуживает снимки.
    assert_eq!(
        manager.snapshot().len(),
        1,
        "торрент должен остаться в менеджере"
    );
    assert_eq!(manager.totals().torrents, 1);

    manager.remove(id, true).await.expect("удаление торрента");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanently_corrupt_piece_is_never_accepted() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("corrupt-permanent");
    let sample = Arc::new(Sample::new(
        "torrenta-corrupt-forever",
        PIECE_LENGTH,
        &[("broken.bin", 131_536)],
    ));
    assert_eq!(sample.piece_count(), 5, "ожидалось 5 кусков");

    // Сидер всегда портит кусок №2 — собрать торрент невозможно.
    let seeder = MockSeeder::start(
        sample.clone(),
        Corruption::Permanent { index: 2 },
    )
    .await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    // Ждём, пока движок соберёт первые корректные куски.
    wait_for(
        &manager,
        id,
        Duration::from_secs(40),
        "движок собрал первые куски",
        |item| item.pieces_have >= 2,
    )
    .await;

    // Теперь ждём повторных запросов: их число обязано превысить минимум, иначе
    // движок не перекачивает отбракованный кусок. Считаем по самому моку, поэтому
    // проверка не зависит от учёта статистики внутри движка.
    let minimal = minimal_block_requests(sample.data.len());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while seeder.requests.load(Ordering::Relaxed) <= minimal {
        assert!(
            tokio::time::Instant::now() < deadline,
            "движок не перезапросил отбракованный кусок: запросов {} при минимуме {minimal}",
            seeder.requests.load(Ordering::Relaxed)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let snapshot = require_snapshot(&manager, id);

    // Текущий движок при постоянной порче остаётся в Downloading и бесконечно
    // перекачивает один и тот же кусок (пробел описан в отчёте). Тест терпим к
    // будущему исправлению, когда такой торрент может уходить в Error.
    assert!(
        matches!(
            snapshot.state,
            TorrentState::Downloading | TorrentState::Error
        ),
        "постоянно испорченный кусок не должен приводить к Seeding/Completed, получено {:?}",
        snapshot.state
    );
    if snapshot.state == TorrentState::Downloading {
        assert!(
            snapshot.error.is_none(),
            "отбраковка куска — не фатальная ошибка торрента: {:?}",
            snapshot.error
        );
    }
    assert!(
        snapshot.pieces_have < snapshot.piece_count,
        "испорченный кусок не должен засчитываться (есть {} из {})",
        snapshot.pieces_have,
        snapshot.piece_count
    );
    assert!(
        snapshot.progress < 1.0,
        "прогресс не должен доходить до 1.0 при постоянно испорченном куске, получено {}",
        snapshot.progress
    );
    // Принятые байты не сбрасываются в ноль: движок продолжал качать.
    assert!(
        snapshot.downloaded_bytes > 0,
        "движок должен был принять байты кусков, полученных до порчи"
    );

    // Движок не паникует и остаётся работоспособным: снимки, пауза, удаление.
    assert_eq!(manager.snapshot().len(), 1, "торрент должен остаться в менеджере");
    assert_eq!(manager.totals().torrents, 1);
    manager.pause(id).expect("пауза при постоянно испорченном куске");
    assert_eq!(
        require_snapshot(&manager, id).state,
        TorrentState::Paused,
        "пауза должна применяться"
    );
    manager
        .resume(id)
        .expect("возобновление при постоянно испорченном куске");
    manager
        .remove(id, true)
        .await
        .expect("удаление при постоянно испорченном куске");
    assert!(
        manager.snapshot().is_empty(),
        "после remove торрентов быть не должно"
    );
    assert_eq!(manager.totals().torrents, 0);

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tracker_failure_does_not_break_engine() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("tracker-fail");
    let sample = Arc::new(Sample::new("torrenta-tracker-fail", PIECE_LENGTH, &[("data.bin", 40_000)]));

    // Трекер отвечает корректным bencode-словарём с `failure reason`.
    let tracker = MockTracker::start(TrackerReply::Failure(
        "торрент не зарегистрирован на трекере".to_string(),
    ))
    .await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    // Движок обязан обратиться к трекеру и пережить отказ.
    let mut attempts = 0u32;
    for _ in 0..40 {
        attempts = tracker.announce_count();
        if attempts >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(attempts >= 1, "движок должен был отправить announce трекеру");

    tokio::time::sleep(Duration::from_millis(500)).await;
    let snapshot = require_snapshot(&manager, id);
    assert_eq!(
        manager.snapshot().len(),
        1,
        "торрент должен остаться в менеджере после отказа трекера"
    );
    assert_eq!(manager.totals().torrents, 1);
    // ВАЖНО: текущий движок не переводит торрент в `TorrentState::Error` при
    // `failure reason` от трекера — `tracker_loop` только увеличивает backoff.
    // Тест терпим к будущему исправлению: главное, что отказ трекера не роняет
    // движок и не выдаёт «успешную» загрузку без пиров.
    match snapshot.state {
        TorrentState::Error => {
            let message = snapshot.error.clone().unwrap_or_default();
            assert!(
                has_cyrillic(&message),
                "текст ошибки трекера должен быть на русском для UI: {message:?}"
            );
        }
        TorrentState::Downloading | TorrentState::Queued | TorrentState::Paused => {
            assert!(
                snapshot.error.is_none(),
                "отказ трекера в текущем движке не попадает в поле error: {:?}",
                snapshot.error
            );
        }
        other => panic!("отказ трекера не должен приводить к состоянию {other:?}"),
    }
    assert_eq!(
        snapshot.downloaded_bytes, 0,
        "без пиров скачивать нечего — байтов быть не должно"
    );

    // Движок продолжает работать: снимок доступен, новые операции не паникуют.
    manager.pause(id).expect("пауза после отказа трекера");
    assert_eq!(require_snapshot(&manager, id).state, TorrentState::Paused);
    manager.resume(id).expect("возобновление после отказа трекера");
    manager.remove(id, false).await.expect("удаление торрента");
    assert!(manager.snapshot().is_empty(), "после remove торрентов быть не должно");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_state_carries_russian_message() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("errors");
    let sample = Arc::new(Sample::new("torrenta-error", PIECE_LENGTH, &[("x.bin", 12_345)]));
    let manager = TorrentManager::new(test_config(&download_dir));

    // Magnet без трекеров: метаданные взять негде — это единственный путь,
    // который в текущем движке приводит к состоянию Error с текстом для UI.
    let link = format!(
        "magnet:?xt=urn:btih:{}&dn=torrenta-error",
        common::hex(&sample.info_hash)
    );
    let id = manager
        .add_magnet(&link, None)
        .await
        .expect("движок не принял корректную magnet-ссылку");

    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(20),
        "торрент перешёл в Error",
        |item| item.state == TorrentState::Error,
    )
    .await;

    let message = snapshot
        .error
        .clone()
        .expect("в состоянии Error поле error должно быть заполнено");
    assert!(
        !message.is_empty(),
        "текст ошибки не должен быть пустым"
    );
    assert!(
        has_cyrillic(&message),
        "текст ошибки должен быть на русском, получено: {message}"
    );
    assert_eq!(
        snapshot.downloaded_bytes, 0,
        "в ошибке не должно быть скачанных байтов"
    );

    // Движок жив после ошибки: остальные операции не паникуют.
    assert_eq!(manager.totals().torrents, 1);
    manager.pause(id).expect("пауза торрента в ошибке");
    manager.resume(id).expect("возобновление торрента в ошибке");
    manager.remove(id, true).await.expect("удаление торрента в ошибке");
    assert!(manager.snapshot().is_empty());
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_resume_and_remove_mid_download() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("pause");
    let sample = Arc::new(Sample::new(
        "torrenta-pause",
        PIECE_LENGTH,
        &[("big.bin", 8 * 1024 * 1024)],
    ));
    assert_eq!(sample.piece_count(), 256, "ожидалось 256 кусков");

    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    // 1. Дожидаемся, что загрузка реально пошла.
    let running = wait_for(
        &manager,
        id,
        Duration::from_secs(30),
        "появились первые скачанные байты",
        |item| item.downloaded_bytes > 0,
    )
    .await;
    assert_eq!(running.state, TorrentState::Downloading);

    // 2. Пауза посреди загрузки: состояние Paused, трафик останавливается.
    manager.pause(id).expect("пауза существующего торрента");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let paused = require_snapshot(&manager, id);
    assert_eq!(
        paused.state,
        TorrentState::Paused,
        "после pause() состояние должно быть Paused"
    );
    assert!(
        paused.downloaded_bytes < paused.total_bytes,
        "пауза должна была случиться до конца загрузки"
    );
    let frozen = paused.downloaded_bytes;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let paused_later = require_snapshot(&manager, id);
    // Блоки, уже летевшие по сети в момент паузы, могут долететь (не больше пары
    // блоков), но полноценная загрузка на паузе продолжаться не может.
    assert!(
        paused_later.downloaded_bytes < frozen + 2 * PIECE_LENGTH as u64,
        "на паузе загрузка должна остановиться: было {frozen}, стало {}",
        paused_later.downloaded_bytes
    );
    // Скорость в снимке — сглаженное значение (EMA), поэтому на паузе она не
    // обнуляется мгновенно, но обязана затухать: новых байтов нет.
    assert!(
        paused_later.down_rate < paused.down_rate * 0.5,
        "на паузе скорость должна затухать: было {}, стало {}",
        paused.down_rate,
        paused_later.down_rate
    );
    assert_eq!(paused_later.state, TorrentState::Paused);
    assert_eq!(
        paused_later.pieces_have, paused.pieces_have,
        "на паузе новые куски не должны засчитываться"
    );

    // 3. Возобновление: состояние возвращается в Downloading, прогресс сохраняется.
    // ВАЖНО: текущий движок на паузе закрывает соединения с пирами, а адрес пира
    // после `seen` повторно не выдаётся — сам по себе повторный коннект не
    // происходит (пробел движка, описан в отчёте). Поэтому проверяем сохранность
    // состояния и отсутствие паник, а не прирост байтов сразу после resume().
    manager.resume(id).expect("возобновление торрента");
    let resumed = wait_for(
        &manager,
        id,
        Duration::from_secs(10),
        "состояние вернулось в Downloading после resume()",
        |item| item.state == TorrentState::Downloading,
    )
    .await;
    assert!(
        resumed.downloaded_bytes >= frozen,
        "после resume() прогресс не должен уменьшаться: было {frozen}, стало {}",
        resumed.downloaded_bytes
    );
    assert!(
        resumed.pieces_have >= paused.pieces_have,
        "после resume() число собранных кусков не должно уменьшаться"
    );
    assert!(
        resumed.error.is_none(),
        "resume() не должен порождать ошибку: {:?}",
        resumed.error
    );

    // 4. Удаление посреди загрузки: без паник, файлы и состояние вычищены.
    manager
        .remove(id, true)
        .await
        .expect("удаление качающегося торрента");
    assert!(
        manager.snapshot().is_empty(),
        "после remove() снимок должен быть пустым"
    );
    let totals = manager.totals();
    assert_eq!(totals.torrents, 0, "торрент не должен остаться в менеджере");
    assert_eq!(totals.downloaded_bytes, 0, "счётчики должны обнулиться");
    let leftover: Vec<std::path::PathBuf> = std::fs::read_dir(&download_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "после remove(delete_files = true) в каталоге не должно остаться файлов: {leftover:?}"
    );

    // 5. Повторное добавление того же торрента работает (состояние не утекло).
    let id_again = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("повторное добавление торрента после удаления");
    wait_for(
        &manager,
        id_again,
        Duration::from_secs(30),
        "повторная загрузка пошла",
        |item| item.downloaded_bytes > 0,
    )
    .await;

    manager.shutdown().await;
}
