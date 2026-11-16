//! E2E: шифрование соединений с пирами (MSE/PE).
//!
//! Проверяется со стороны движка, а не только модуля `mse`: менеджер с
//! включённым шифрованием должен сам выполнить рукопожатие и поверх
//! зашифрованного потока отправить обычный BitTorrent handshake. Вторая
//! проверка — откат «предпочтительного» режима на открытое соединение, если пир
//! шифрование не поддерживает.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Corruption, MockSeeder, MockTracker, Sample, TrackerReply, PIECE_LENGTH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use torrent_core::mse::{self, PROVIDE_PLAINTEXT, PROVIDE_RC4};
use torrent_core::wire::Handshake;
use torrent_core::{
    AddOptions, EncryptionMode, EngineConfig, InfoHash, TorrentManager, TorrentState,
};

/// Конфиг с заданным режимом шифрования.
fn config_with(download_dir: &std::path::Path, encryption: EncryptionMode) -> EngineConfig {
    EngineConfig {
        download_dir: download_dir.to_path_buf(),
        listen_port: 0,
        dht: false,
        encryption,
        ..EngineConfig::default()
    }
}

/// Наш ответный handshake: тот же info-hash, произвольный peer id.
fn our_handshake(hash: [u8; 20]) -> Vec<u8> {
    Handshake::new(InfoHash(hash), *b"-TT0001-abcdefghijkl").encode()
}

#[tokio::test]
async fn required_encryption_completes_mse_handshake() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new("mse", PIECE_LENGTH, &[("data.bin", 32)]));
    let expected_hash = sample.info_hash;

    // Наш «пир»: слушает TCP, выполняет входящее рукопожатие MSE и проверяет,
    // что дальше пришёл настоящий BitTorrent handshake уже расшифрованным.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("слушатель");
    let peer_addr = listener.local_addr().expect("адрес слушателя");
    let (info_tx, info_rx) = tokio::sync::oneshot::channel();
    let peer_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("принять соединение");
        let handshake = mse::accept(
            stream,
            &[InfoHash(expected_hash)],
            PROVIDE_RC4 | PROVIDE_PLAINTEXT,
            true,
        )
        .await
        .expect("MSE-рукопожатие должно пройти");
        assert!(handshake.rc4, "обязательно должен выбраться RC4");
        let mut stream = handshake.stream;
        let mut buf = [0u8; 68];
        stream
            .read_exact(&mut buf)
            .await
            .expect("BitTorrent handshake поверх шифрования");
        let remote = Handshake::parse(&buf).expect("handshake разбирается");
        let _ = info_tx.send(remote.info_hash);
        stream
            .write_all(&our_handshake(expected_hash))
            .await
            .expect("ответный handshake");
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let tracker = MockTracker::start(TrackerReply::Peers(vec![peer_addr])).await;
    let manager = TorrentManager::new(config_with(&download_dir, EncryptionMode::Required));
    manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("движок принял торрент");

    let received = tokio::time::timeout(Duration::from_secs(20), info_rx)
        .await
        .expect("пир дождался расшифрованного handshake")
        .expect("канал не закрыт");
    assert_eq!(received, InfoHash(expected_hash));
    let _ = peer_task.await;
    manager.shutdown().await;
}

#[tokio::test]
async fn preferred_encryption_falls_back_to_plaintext() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new("fallback", PIECE_LENGTH, &[("data.bin", 32)]));
    let expected_hash = sample.info_hash;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("слушатель");
    let peer_addr = listener.local_addr().expect("адрес слушателя");
    let (plain_tx, plain_rx) = tokio::sync::oneshot::channel();
    let peer_task = tokio::spawn(async move {
        // Первое соединение: шифрованное рукопожатие мы не поддерживаем —
        // просто закрываем сокет.
        let (first, _) = listener.accept().await.expect("первое соединение");
        drop(first);
        // Второе: движок в режиме «предпочтительно» обязан прийти с открытым
        // BitTorrent handshake.
        let (mut second, _) = listener.accept().await.expect("второе соединение");
        let mut buf = [0u8; 68];
        second
            .read_exact(&mut buf)
            .await
            .expect("открытый handshake после отката");
        let remote = Handshake::parse(&buf).expect("handshake разбирается");
        assert_eq!(buf[0], 19, "откат должен давать открытый протокол");
        let _ = plain_tx.send(remote.info_hash);
        second
            .write_all(&our_handshake(expected_hash))
            .await
            .expect("ответ на handshake");
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let tracker = MockTracker::start(TrackerReply::Peers(vec![peer_addr])).await;
    let manager = TorrentManager::new(config_with(&download_dir, EncryptionMode::Preferred));
    manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("движок принял торрент");

    let received = tokio::time::timeout(Duration::from_secs(25), plain_rx)
        .await
        .expect("движок откатился на открытое соединение")
        .expect("канал не закрыт");
    assert_eq!(received, InfoHash(expected_hash));
    let _ = peer_task.await;
    manager.shutdown().await;
}

#[tokio::test]
async fn required_encryption_rejects_plaintext_peer() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new("strict", PIECE_LENGTH, &[("data.bin", 32)]));
    // Обычный сидер без шифрования: в строгом режиме соединение не состоится.
    let seeder = MockSeeder::start(sample.clone(), Corruption::None).await;
    let tracker = MockTracker::start(TrackerReply::Peers(vec![seeder.addr])).await;
    let manager = TorrentManager::new(config_with(&download_dir, EncryptionMode::Required));
    let id = manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("движок принял торрент");
    tokio::time::sleep(Duration::from_secs(4)).await;
    let snapshot = manager
        .snapshot()
        .into_iter()
        .find(|item| item.id == id)
        .expect("снимок есть");
    assert_eq!(
        snapshot.peers_connected, 0,
        "без шифрования пир не подключается"
    );
    assert_eq!(snapshot.pieces_have, 0);
    assert!(
        matches!(
            snapshot.state,
            TorrentState::Downloading | TorrentState::Queued
        ),
        "торрент остаётся в ожидании: {:?}",
        snapshot.state
    );
    manager.shutdown().await;
}
