//! E2E: раскладка файлов внутри каталога загрузки (1.9, как в qBittorrent).
//!
//! Проверяется не только снимок движка, но и реальное положение файлов на
//! диске: раскладка применяется до создания хранилища, поэтому «как в раздаче»
//! обязано создать папку с именем торрента, а «без папки» — не создавать.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{wait_for, Corruption, MockSeeder, MockTracker, Sample, TrackerReply, PIECE_LENGTH};
use torrent_core::{AddOptions, ContentLayout, EngineConfig, TorrentManager, TorrentState};

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

/// Скачивает торрент с заданной раскладкой и возвращает снимок движка.
async fn download_with(
    dir: &Path,
    sample: Arc<Sample>,
    layout: ContentLayout,
) -> Arc<TorrentManager> {
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(test_config(dir));
    let id = manager
        .add_file_with(
            sample.torrent_bytes(&tracker.url()),
            AddOptions {
                content_layout: Some(layout),
                ..AddOptions::default()
            },
        )
        .await
        .expect("движок не принял корректный .torrent");
    wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "торрент скачался",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;
    manager
}

fn assert_file(root: &Path, name: &str, expected: &[u8]) {
    let path = root.join(name);
    let actual = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("не удалось прочитать {}: {err}", path.display()));
    assert_eq!(actual, expected, "содержимое {} не совпало", path.display());
}

#[tokio::test]
async fn original_layout_creates_torrent_folder() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "Раскладка",
        PIECE_LENGTH,
        &[
            ("first.bin", PIECE_LENGTH as usize),
            ("sub/second.bin", PIECE_LENGTH as usize),
        ],
    ));
    let manager = download_with(&download_dir, sample.clone(), ContentLayout::Original).await;

    // «Как в раздаче»: файлы внутри папки с именем торрента.
    let root = download_dir.join("Раскладка");
    for (name, content) in &sample.files {
        assert_file(&root, name, content);
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn no_subfolder_layout_keeps_files_in_root() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "Без папки",
        PIECE_LENGTH,
        &[
            ("first.bin", PIECE_LENGTH as usize),
            ("second.bin", PIECE_LENGTH as usize),
        ],
    ));
    let manager = download_with(&download_dir, sample.clone(), ContentLayout::NoSubfolder).await;

    for (name, content) in &sample.files {
        assert_file(&download_dir, name, content);
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn subfolder_layout_wraps_single_file_in_stem_folder() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "movie.mkv",
        PIECE_LENGTH,
        &[("movie.mkv", PIECE_LENGTH as usize)],
    ));
    let manager = download_with(&download_dir, sample.clone(), ContentLayout::Subfolder).await;

    // У однофайлового торрента папка называется по имени файла без расширения.
    let root = download_dir.join("movie");
    assert_file(&root, "movie.mkv", &sample.files[0].1);
    manager.shutdown().await;
}
