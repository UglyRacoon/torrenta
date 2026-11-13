//! Быстрое возобновление: карта проверенных кусков без чтения диска.
//!
//! Реальный сценарий — перезапуск приложения: Tauri-слой сохраняет карту в
//! `session.json` вместе с отпечатками файлов и, если файлы не менялись,
//! применяет её вместо полной проверки хэшами. Здесь проверяется сам движок:
//! применить карту, получить её обратно и не перейти в загрузку заново.

mod common;

use std::path::Path;

use common::{Sample, PIECE_LENGTH};
use torrent_core::{EngineConfig, EngineError, ResumeData, TorrentManager, TorrentState};

fn test_config(download_dir: &Path) -> EngineConfig {
    EngineConfig {
        download_dir: download_dir.to_path_buf(),
        listen_port: 0,
        // Сеть не нужна: карта применяется без обмена с пирами.
        dht: false,
        pex: false,
        web_seeds: false,
        port_mapping: false,
        ..EngineConfig::default()
    }
}

/// Трекер-заглушка: адрес заведомо недоступен, поэтому announce просто не удаётся.
fn dead_tracker() -> String {
    "http://127.0.0.1:1/announce".to_string()
}

/// Битовая карта, где помечены куски `first..first + count`.
fn map_of(piece_count: usize, marked: &[usize]) -> ResumeData {
    let mut pieces = vec![0u8; piece_count.div_ceil(8)];
    for index in marked {
        pieces[index / 8] |= 0x80 >> (index % 8);
    }
    ResumeData {
        piece_count: piece_count as u32,
        piece_length: PIECE_LENGTH,
        pieces,
    }
}

async fn add_sample(manager: &TorrentManager, sample: &Sample) -> u64 {
    manager
        .add_file(sample.torrent_bytes(&dead_tracker()), None)
        .await
        .expect("движок не принял корректный .torrent")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_map_marks_torrent_complete_without_downloading() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Sample::new(
        "resume-full",
        PIECE_LENGTH,
        &[("full.bin", 8 * PIECE_LENGTH as usize)],
    );
    let manager = TorrentManager::new(test_config(&download_dir));
    let id = add_sample(&manager, &sample).await;

    // Ни одного байта не скачано: пиров нет вообще.
    let before = common::require_snapshot(&manager, id);
    assert_eq!(before.pieces_have, 0, "до применения карты кусков нет");

    let all: Vec<usize> = (0..sample.piece_count()).collect();
    manager
        .apply_resume(id, &map_of(sample.piece_count(), &all))
        .expect("движок не принял карту возобновления");

    let after = common::require_snapshot(&manager, id);
    assert_eq!(
        after.pieces_have,
        sample.piece_count() as u32,
        "карта должна пометить все куски проверенными"
    );
    assert_eq!(after.progress, 1.0, "прогресс считается по карте");
    assert_eq!(after.remaining_bytes, 0, "докачивать нечего");
    assert_eq!(
        after.state,
        TorrentState::Seeding,
        "готовый торрент раздаётся сразу, без хэширования"
    );

    // Обратный путь: сохранённая карта совпадает с применённой.
    let stored = manager.resume_data(id).expect("карта должна читаться");
    assert_eq!(stored.piece_count, sample.piece_count() as u32);
    assert_eq!(stored.piece_length, PIECE_LENGTH);
    assert_eq!(stored, map_of(sample.piece_count(), &all));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_map_resumes_download_from_saved_point() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Sample::new(
        "resume-partial",
        PIECE_LENGTH,
        &[("partial.bin", 8 * PIECE_LENGTH as usize)],
    );
    let manager = TorrentManager::new(test_config(&download_dir));
    let id = add_sample(&manager, &sample).await;

    manager
        .apply_resume(id, &map_of(sample.piece_count(), &[0, 1, 2, 3]))
        .expect("движок не принял карту возобновления");

    let snapshot = common::require_snapshot(&manager, id);
    assert_eq!(snapshot.pieces_have, 4);
    assert_eq!(snapshot.progress, 0.5, "прогресс 4 из 8 кусков — половина");
    assert_eq!(
        snapshot.state,
        TorrentState::Downloading,
        "недостающие куски докачиваются, а не считаются готовыми"
    );
    assert!(
        snapshot.remaining_bytes > 0,
        "остаток должен быть больше нуля"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_map_from_another_torrent() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Sample::new(
        "resume-reject",
        PIECE_LENGTH,
        &[("reject.bin", 8 * PIECE_LENGTH as usize)],
    );
    let manager = TorrentManager::new(test_config(&download_dir));
    let id = add_sample(&manager, &sample).await;

    // Другое число кусков — данные от чужого торрента.
    let wrong_count = ResumeData {
        piece_count: sample.piece_count() as u32 + 1,
        piece_length: PIECE_LENGTH,
        pieces: vec![0u8; sample.piece_count().div_ceil(8) + 1],
    };
    assert!(matches!(
        manager.apply_resume(id, &wrong_count),
        Err(EngineError::InvalidInput(_))
    ));

    // Правильное число кусков, но карта короче нужного (8 кусков — это 1 байт,
    // пустая карта заведомо неполная).
    let short = ResumeData {
        piece_count: sample.piece_count() as u32,
        piece_length: PIECE_LENGTH,
        pieces: Vec::new(),
    };
    assert!(matches!(
        manager.apply_resume(id, &short),
        Err(EngineError::InvalidInput(_))
    ));

    // Отказ не должен портить состояние: кусков по-прежнему ноль.
    let snapshot = common::require_snapshot(&manager, id);
    assert_eq!(snapshot.pieces_have, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn map_is_empty_before_metadata() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Sample::new(
        "resume-magnet",
        PIECE_LENGTH,
        &[("magnet.bin", 4 * PIECE_LENGTH as usize)],
    );
    let manager = TorrentManager::new(test_config(&download_dir));
    // Magnet без доступного трекера: метаданных нет, карту сохранять нечего.
    let id = manager
        .add_magnet(&sample.magnet_link(&dead_tracker()), None)
        .await
        .expect("движок не принял magnet");
    let _ = id;

    assert!(
        matches!(
            manager.resume_data(id),
            Err(EngineError::NotReady) | Err(EngineError::NotFound(_))
        ),
        "без метаданных карты возобновления нет"
    );
}
