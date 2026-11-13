//! Регресс-тесты на баги движка, исправленные после первого прогона E2E.
//!
//! Покрываются четыре исправления:
//! 1. отказ трекера (`failure reason`) снова виден пользователю: `TorrentState::Error`,
//!    русский текст в снимке и в `EngineEvent::Error`, а успешное объявление
//!    возвращает торрент в `Downloading` и стирает текст ошибки;
//! 2. `resume()` действительно продолжает загрузку — без ожидания следующего
//!    объявления трекера (интервал мока 600 с), то есть по списку уже известных
//!    адресов;
//! 3. `TorrentManager::shutdown()` освобождает порт прослушки;
//! 4. постоянная порча одного куска не блокирует остальные: они скачиваются,
//!    прогресс растёт, а принятые байты не раздувают счётчик.
//!
//! Сеть полностью локальная: моки биндятся на `127.0.0.1:0`, движок слушает
//! `listen_port: 0`. Внешних адресов нет, интернет не нужен.

mod common;

use std::net::TcpListener;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::{
    require_snapshot, wait_for, Corruption, MockSeeder, MockTracker, Sample, TrackerReply,
    PIECE_LENGTH,
};
use torrent_core::engine::EngineEvent;
use torrent_core::{EngineConfig, TorrentManager, TorrentState};

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

/// Читает скачанный файл и сверяет его с исходником.
fn assert_file_matches(root: &std::path::Path, name: &str, expected: &[u8]) {
    let path = root.join(name);
    let actual = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("не удалось прочитать {}: {err}", path.display()));
    assert_eq!(
        actual.len(),
        expected.len(),
        "размер файла {name} не совпал с исходником"
    );
    assert!(
        actual == expected,
        "файл {name} скачан с расхождениями в содержимом"
    );
}

/// Проверяет, что в тексте ошибки есть кириллица (сообщения движка — на русском).
fn assert_russian(text: &str, what: &str) {
    assert!(
        text.chars()
            .any(|ch| ('\u{0400}'..='\u{04FF}').contains(&ch)),
        "{what}: ожидался русский текст ошибки, получено «{text}»"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tracker_refusal_sets_error_and_successful_announce_recovers() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "torrenta-tracker-recovery",
        PIECE_LENGTH,
        &[("recover.bin", 20_000)],
    ));
    assert_eq!(sample.piece_count(), 1, "ожидался один кусок");

    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    // Первый announce — отказ, второй (через ~30 с бэкоффа) — уже пиры.
    let tracker = MockTracker::start_script(
        vec![
            TrackerReply::Failure("торрент не зарегистрирован".to_string()),
            TrackerReply::Peers(vec![seeder.addr]),
        ],
        30,
    )
    .await;
    let manager = TorrentManager::new(test_config(&download_dir));
    let mut events = manager.subscribe();

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    let failed = wait_for(
        &manager,
        id,
        Duration::from_secs(20),
        "трекер отказал и торрент ушёл в Error",
        |item| item.state == TorrentState::Error,
    )
    .await;

    let message = failed
        .error
        .clone()
        .expect("в состоянии Error снимок обязан содержать текст ошибки");
    assert!(
        message.contains("трекер"),
        "текст ошибки должен объяснять причину, получено «{message}»"
    );
    assert!(
        message.contains("не зарегистрирован"),
        "в тексте ошибки должен быть отказ трекера, получено «{message}»"
    );
    assert_russian(&message, "отказ трекера");
    assert_eq!(
        failed.downloaded_bytes, 0,
        "пока трекер отказывает, качать нечего — счётчик должен быть нулевым"
    );

    // Тот же отказ должен быть продублирован событием для UI.
    let mut reported = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EngineEvent::Error {
            id: event_id,
            message: event_message,
        } = event
        {
            if event_id == id {
                reported.push(event_message);
            }
        }
    }
    assert!(
        reported.iter().any(|item| item == &message),
        "ожидалось EngineEvent::Error с текстом «{message}», получено {reported:?}"
    );

    // Успешное объявление обязано снять ошибку и вернуть торрент в загрузку.
    let recovered = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "после успешного объявления ошибка снята и загрузка пошла",
        |item| item.state != TorrentState::Error && item.piece_count > 0,
    )
    .await;
    assert!(
        recovered.error.is_none(),
        "после успешного объявления текст ошибки должен исчезнуть, получено {:?}",
        recovered.error
    );
    assert!(
        matches!(
            recovered.state,
            TorrentState::Downloading | TorrentState::Seeding | TorrentState::Completed
        ),
        "после восстановления ожидалось Downloading/Seeding/Completed, получено {:?}",
        recovered.state
    );

    let done = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "торрент докачался после восстановления трекера",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;
    assert!(
        (done.progress - 1.0).abs() < 1e-9,
        "после восстановления прогресс должен быть 1.0, получено {}",
        done.progress
    );
    assert_eq!(
        done.downloaded_bytes, done.total_bytes,
        "скачано должно быть ровно total_bytes"
    );
    assert_file_matches(&download_dir, "recover.bin", &sample.data);
    assert!(
        tracker.announce_count() >= 2,
        "ожидалось как минимум два объявления (отказ и успешное), получено {}",
        tracker.announce_count()
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_continue_download_without_new_announce() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "torrenta-pause-resume",
        PIECE_LENGTH,
        &[("resume.bin", 8 * PIECE_LENGTH as usize)],
    ));
    assert_eq!(sample.piece_count(), 8, "ожидалось 8 кусков");

    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    // Огромный интервал объявления: доказательство, что переподключение идёт
    // по уже известным адресам, а не по свежему ответу трекера.
    let tracker =
        MockTracker::start_with_interval(TrackerReply::Peers(vec![seeder.addr]), 600).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    // С 1.8.0 конвейер запросов заполняется сразу после каждого блока, и на
    // loopback 256 КиБ прилетают быстрее, чем тест успевает увидеть середину
    // загрузки (проверка «есть 2..7 кусков» проскакивала в «готово»). Ограничение
    // скорости делает паузу посреди загрузки воспроизводимой и не меняет суть
    // проверки: пауза, простой и продолжение по уже известным адресам.
    manager
        .set_limits(id, 64, 0)
        .expect("движок не принял лимит скорости для теста");

    // Пауза строго посреди загрузки: часть кусков уже есть, часть — нет.
    let middle = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "скачана половина кусков (пауза посреди загрузки)",
        |item| item.pieces_have >= 2 && item.pieces_have < item.piece_count,
    )
    .await;
    assert!(
        middle.pieces_have < middle.piece_count,
        "пауза должна приходиться на незавершённую загрузку (есть {}, всего {})",
        middle.pieces_have,
        middle.piece_count
    );
    assert_eq!(
        tracker.announce_count(),
        1,
        "до паузы движок обязан был объявиться ровно один раз"
    );

    manager
        .pause(id)
        .expect("движок не смог поставить торрент на паузу");
    let paused = wait_for(
        &manager,
        id,
        Duration::from_secs(10),
        "торрент встал на паузу",
        |item| item.state == TorrentState::Paused,
    )
    .await;

    // Пауза должна продержаться дольше тика сеанса (250 мс), иначе движок
    // не успеет закрыть соединение и проверка переподключения ничего не докажет.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let frozen = require_snapshot(&manager, id);
    assert_eq!(
        frozen.state,
        TorrentState::Paused,
        "торрент обязан оставаться на паузе"
    );
    let requests_on_pause = seeder.requests.load(Ordering::Relaxed);
    let connections_on_pause = seeder.connections.load(Ordering::Relaxed);

    // На паузе прогресс стоять на месте: новых проверенных кусков быть не может.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let during_pause = require_snapshot(&manager, id);
    assert_eq!(
        during_pause.state,
        TorrentState::Paused,
        "торрент не должен самовольно выходить с паузы"
    );
    assert_eq!(
        during_pause.pieces_have, frozen.pieces_have,
        "на паузе прогресс не должен расти (было {}, стало {})",
        frozen.pieces_have, during_pause.pieces_have
    );
    assert!(
        during_pause.error.is_none(),
        "пауза не должна добавлять ошибок: {:?}",
        during_pause.error
    );

    manager
        .resume(id)
        .expect("движок не смог снять торрент с паузы");
    wait_for(
        &manager,
        id,
        Duration::from_secs(15),
        "торрент вернулся в загрузку после resume",
        |item| item.state == TorrentState::Downloading,
    )
    .await;

    let done = wait_for(
        &manager,
        id,
        Duration::from_secs(90),
        "торрент докачался после resume",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;

    assert!(
        done.error.is_none(),
        "после resume загрузка не должна уходить в ошибку: {:?}",
        done.error
    );
    assert!(
        (done.progress - 1.0).abs() < 1e-9,
        "после resume прогресс должен быть 1.0, получено {}",
        done.progress
    );
    assert_eq!(
        done.pieces_have, done.piece_count,
        "после resume должны быть все куски"
    );
    assert_file_matches(&download_dir, "resume.bin", &sample.data);

    // Ключевое доказательство: торрент докачался без нового объявления трекера
    // (интервал мока 600 с покрывает весь тест) и на новом соединении.
    assert_eq!(
        tracker.announce_count(),
        1,
        "загрузка не должна требовать нового объявления трекера, получено объявлений: {}",
        tracker.announce_count()
    );
    // Переподключение к сидеру после resume: движок обязан взять адрес из своего
    // списка известных, а не ждать ответа трекера (интервал мока — 600 с).
    let mut reconnected = false;
    for _ in 0..150 {
        if seeder.connections.load(Ordering::Relaxed) > connections_on_pause {
            reconnected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        reconnected,
        "после resume движок должен переподключиться к сидеру (было {}, стало {})",
        connections_on_pause,
        seeder.connections.load(Ordering::Relaxed)
    );
    assert!(
        seeder.requests.load(Ordering::Relaxed) > requests_on_pause,
        "после resume сидер должен получить новые запросы блоков"
    );
    assert!(
        paused.pieces_have <= done.pieces_have,
        "прогресс не должен откатываться: на паузе {}, в конце {}",
        paused.pieces_have,
        done.pieces_have
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_releases_listen_port() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "torrenta-port",
        PIECE_LENGTH,
        &[("port.bin", 8_192)],
    ));
    // Пиров нет: движок живёт, слушает порт, но соединений не принимает.
    let tracker = MockTracker::start(TrackerReply::Peers(Vec::new())).await;

    let manager = TorrentManager::new(test_config(&download_dir));
    let port = manager.listen_port();
    assert_ne!(
        port, 0,
        "движок должен был занять конкретный порт прослушки"
    );

    let _id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");
    let mut announced = false;
    for _ in 0..100 {
        if tracker.announce_count() >= 1 {
            announced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        announced,
        "движок должен был объявиться трекеру после добавления торрента"
    );

    // Пока движок работает, порт занят — иначе проверка ниже ничего не доказывает.
    assert!(
        TcpListener::bind(("0.0.0.0", port)).is_err(),
        "пока движок работает, порт {port} должен быть занят"
    );

    manager.shutdown().await;

    let mut bound = None;
    for _ in 0..50 {
        match TcpListener::bind(("0.0.0.0", port)) {
            Ok(listener) => {
                bound = Some(listener);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let freed = bound.expect("после shutdown() порт должен освобождаться");
    drop(freed);

    // И новый менеджер обязан занять именно этот порт, а не «какой-нибудь».
    let mut config = test_config(&download_dir);
    config.listen_port = port;
    let second = TorrentManager::new(config);
    assert_eq!(
        second.listen_port(),
        port,
        "новый менеджер должен занять освобождённый порт {port}"
    );
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanently_corrupt_piece_does_not_starve_the_rest() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let total = (4 * PIECE_LENGTH + 464) as usize;
    let sample = Arc::new(Sample::new(
        "torrenta-degraded",
        PIECE_LENGTH,
        &[("degraded.bin", total)],
    ));
    assert_eq!(sample.piece_count(), 5, "ожидалось 5 кусков");

    let seeder = MockSeeder::start(sample.clone(), Corruption::Permanent { index: 2 }).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    // Куски 0, 1, 3 и 4 обязаны скачаться, несмотря на вечно битый кусок 2.
    let progress = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "все остальные куски скачались, несмотря на битый кусок",
        |item| item.pieces_have >= 4,
    )
    .await;

    assert!(
        progress.pieces_have < progress.piece_count,
        "битый кусок не должен засчитываться (есть {}, всего {})",
        progress.pieces_have,
        progress.piece_count
    );
    assert!(
        progress.progress > 0.5,
        "прогресс должен перевалить за половину, получено {}",
        progress.progress
    );
    assert_eq!(
        progress.pieces_have, 4,
        "должны быть скачаны все куски, кроме вечно битого"
    );
    assert!(
        progress.error.is_none(),
        "порча данных — не повод уводить торрент в ошибку: {:?}",
        progress.error
    );
    assert!(
        matches!(
            progress.state,
            TorrentState::Downloading | TorrentState::Error
        ),
        "ожидалось Downloading (или Error), получено {:?}",
        progress.state
    );

    // Счётчик принятых байтов учитывает только проверенные куски.
    let limit = progress.total_bytes + PIECE_LENGTH as u64;
    assert!(
        progress.downloaded_bytes <= limit,
        "downloaded_bytes {} превышает total_bytes {} более чем на кусок",
        progress.downloaded_bytes,
        progress.total_bytes
    );
    let totals = manager.totals();
    assert!(
        totals.downloaded_bytes <= limit,
        "глобальный счётчик {} превышает total_bytes {} более чем на кусок",
        totals.downloaded_bytes,
        progress.total_bytes
    );

    // Движок обязан продолжать попытки (а не замереть) и не падать: кулдаун
    // битого куска растёт, поэтому ждём запросы с запасом.
    let before = seeder.requests.load(Ordering::Relaxed);
    let mut retried = false;
    for _ in 0..150 {
        if seeder.requests.load(Ordering::Relaxed) > before {
            retried = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        retried,
        "движок должен повторять запросы к битому куску, а не замирать"
    );
    assert_eq!(
        manager.snapshot().len(),
        1,
        "менеджер должен продолжать держать торрент"
    );
    assert_eq!(manager.totals().torrents, 1, "должен остаться один торрент");

    // Пауза/возобновление/удаление на «застрявшем» торренте не должны паниковать.
    manager.pause(id).expect("пауза сломанного торрента упала");
    manager.resume(id).expect("resume сломанного торрента упал");
    manager
        .remove(id, true)
        .await
        .expect("удаление сломанного торрента упало");
    assert!(
        manager.snapshot().is_empty(),
        "после remove снимок должен быть пустым"
    );

    manager.shutdown().await;
}
