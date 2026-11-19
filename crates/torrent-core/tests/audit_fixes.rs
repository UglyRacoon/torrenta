//! Регрессии по аудиту 1.9.2: проверка на добавлении (D22) и всплеск
//! скорости после перезапуска (D25).

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use torrent_core::{AddOptions, ContentLayout, EngineConfig, TorrentManager, TorrentSnapshot};

use common::{MockTracker, Sample, TrackerReply, PIECE_LENGTH};

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
        assert!(Instant::now() < deadline, "условие не наступило: {snap:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// D22: готовые данные на диске проверяются при добавлении, без ручной
/// «Перепроверки». Раньше торрент начинал «качать» то, что уже есть.
#[tokio::test]
async fn existing_data_are_verified_on_add() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let dir = temp.path().join("downloads");
    std::fs::create_dir_all(&dir).expect("каталог загрузки");
    let sample = Sample::new("auto-check", PIECE_LENGTH, &[("data.bin", 64 * 1024)]);
    write_data(&sample, &dir);

    let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let manager = TorrentManager::new(EngineConfig {
        download_dir: dir,
        listen_port: 0,
        dht: false,
        content_layout: ContentLayout::NoSubfolder,
        seed_after_complete: false,
        ..EngineConfig::default()
    });
    let id = manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("торрент добавлен");
    // recheck НЕ вызываем: проверка должна стартовать сама.
    let checked = wait_for(&manager, id, |snap| {
        snap.pieces_have == snap.piece_count && snap.piece_count > 0
    })
    .await;
    assert_eq!(
        checked.downloaded_bytes, 0,
        "готовые байты не скачиваются заново"
    );
    let completed = wait_for(&manager, id, |snap| {
        matches!(snap.state, torrent_core::TorrentState::Completed)
    })
    .await;
    assert!(completed.progress >= 1.0);
    manager.shutdown().await;
}

/// D25: после восстановления счётчиков (перезапуск) скорость не должна
/// показывать десятки гигабайт в секунду.
#[tokio::test]
async fn restored_counters_do_not_spike_rate() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let sample = Sample::new("rate-baseline", PIECE_LENGTH, &[("data.bin", 32 * 1024)]);
    let tracker = MockTracker::start(TrackerReply::Peers(vec![])).await;
    let manager = TorrentManager::new(EngineConfig {
        download_dir: temp.path().join("downloads"),
        listen_port: 0,
        dht: false,
        content_layout: ContentLayout::NoSubfolder,
        ..EngineConfig::default()
    });
    let id = manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("торрент добавлен");
    // Имитация перезапуска: 5 ГиБ отдано, 1 МиБ скачано.
    manager
        .set_transfer_totals(id, 5 * 1024 * 1024 * 1024, 1024 * 1024)
        .expect("счётчики восстановлены");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let snap = snapshot(&manager, id);
    assert!(
        snap.up_rate < 1_000_000.0,
        "скорость после восстановления: {} Б/с (ожидали около нуля)",
        snap.up_rate
    );
    assert_eq!(snap.uploaded_bytes, 5 * 1024 * 1024 * 1024);
    manager.shutdown().await;
}
