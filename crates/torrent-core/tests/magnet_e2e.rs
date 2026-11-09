//! E2E: получение метаданных по magnet-ссылке (BEP 9 `ut_metadata`, BEP 10).
//!
//! Сеть локальная: mock-трекер и mock-пир на `127.0.0.1:0`. Проверяется, что
//! движок реально вытягивает `info`-словарь у пира по расширенному протоколу,
//! сверяет info-hash и переходит из `Metadata` в `Downloading` с готовым
//! набором файлов.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{wait_for, MockMetadataPeer, MockTracker, Sample, TrackerReply, PIECE_LENGTH};
use torrent_core::{EngineConfig, TorrentManager, TorrentState};

fn test_config(download_dir: &std::path::Path) -> EngineConfig {
    EngineConfig {
        download_dir: download_dir.to_path_buf(),
        listen_port: 0,
        // DHT ходит в настоящую сеть — в тестах он не нужен и только шумит.
        dht: false,
        ..EngineConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_metadata_is_fetched_from_mock_peer() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("magnet");
    let sample = Arc::new(Sample::new(
        "torrenta-magnet",
        PIECE_LENGTH,
        &[("m.bin", 96_000)],
    ));
    assert_eq!(sample.piece_count(), 3, "ожидалось 3 куска");

    // Пир отдаёт только метаданные: этого достаточно, чтобы движок вышел из Metadata.
    let peer = MockMetadataPeer::start(sample.info_bytes.clone(), sample.info_hash).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![peer.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let link = sample.magnet_link(&tracker.url());
    let id = manager
        .add_magnet(&link, None)
        .await
        .expect("движок не принял корректную magnet-ссылку");

    // Сразу после добавления метаданных ещё нет.
    let initial = common::require_snapshot(&manager, id);
    assert_eq!(
        initial.state,
        TorrentState::Metadata,
        "magnet-торрент начинается в состоянии Metadata"
    );
    assert_eq!(initial.piece_count, 0, "до метаданных кусков ещё нет");

    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(45),
        "метаданные получены и торрент вышел из Metadata",
        |item| item.piece_count > 0 && item.state != TorrentState::Metadata,
    )
    .await;

    assert_eq!(snapshot.name, "torrenta-magnet", "имя из info-словаря");
    assert_eq!(
        snapshot.info_hash,
        common::hex(&sample.info_hash),
        "info-hash из метаданных должен совпасть с ожидаемым"
    );
    assert_eq!(snapshot.piece_count as usize, sample.piece_count());
    assert_eq!(snapshot.piece_length, PIECE_LENGTH);
    assert_eq!(
        snapshot.total_bytes,
        sample.data.len() as u64,
        "полный размер из метаданных"
    );
    assert_eq!(snapshot.error, None, "ошибок быть не должно");
    assert!(
        matches!(
            snapshot.state,
            TorrentState::Downloading | TorrentState::Seeding
        ),
        "после метаданных ожидался Downloading, получено {:?}",
        snapshot.state
    );
    assert_eq!(
        snapshot.files.len(),
        1,
        "в метаданных должен быть один файл"
    );
    assert_eq!(snapshot.files[0].path, "m.bin");

    manager
        .remove(id, false)
        .await
        .expect("удаление magnet-торрента");
    assert!(manager.snapshot().is_empty());
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_without_peers_stays_in_metadata_and_engine_survives() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("magnet-empty");
    let sample = Arc::new(Sample::new(
        "torrenta-magnet-empty",
        PIECE_LENGTH,
        &[("e.bin", 20_000)],
    ));

    // Трекер отвечает корректно, но список пиров пуст.
    let tracker = MockTracker::start(TrackerReply::Peers(Vec::new())).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_magnet(&sample.magnet_link(&tracker.url()), None)
        .await
        .expect("движок не принял корректную magnet-ссылку");

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let snapshot = common::require_snapshot(&manager, id);
    assert_eq!(
        snapshot.state,
        TorrentState::Metadata,
        "без пиров метаданные получить негде — состояние остаётся Metadata"
    );
    assert_eq!(snapshot.piece_count, 0);
    assert_eq!(snapshot.downloaded_bytes, 0);
    assert!(
        tracker.announce_count() >= 1,
        "движок должен был объявиться трекеру за метаданными"
    );

    // Движок жив: снимок читается, операции не паникуют.
    assert_eq!(manager.totals().torrents, 1);
    manager
        .remove(id, false)
        .await
        .expect("удаление magnet-торрента");
    assert!(manager.snapshot().is_empty());
    manager.shutdown().await;
}
