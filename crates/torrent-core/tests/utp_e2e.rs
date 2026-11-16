//! E2E: uTP (BEP 29) как транспорт пира.
//!
//! Движок сначала пробует TCP; здесь TCP-порт закрыт, поэтому он обязан
//! перейти на uTP и поверх него отправить обычный BitTorrent handshake.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use torrent_core::utp::UtpListener;
use torrent_core::wire::Handshake;
use torrent_core::{AddOptions, EngineConfig, InfoHash, TorrentManager};

use common::{MockTracker, Sample, TrackerReply, PIECE_LENGTH};

#[tokio::test]
async fn engine_connects_over_utp_when_tcp_is_closed() {
    let temp = tempfile::tempdir().expect("временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new("utp", PIECE_LENGTH, &[("data.bin", 32)]));
    let expected_hash = sample.info_hash;

    // Наш «пир»: слушает только UDP/uTP на случайном порту, TCP там закрыт.
    let (addr, mut incoming) = UtpListener::start("127.0.0.1:0".parse().expect("адрес"))
        .await
        .expect("uTP-слушатель");
    let (hash_tx, hash_rx) = tokio::sync::oneshot::channel();
    let peer_task = tokio::spawn(async move {
        // Первое соединение в режиме «предпочтительно» несёт MSE-рукопожатие
        // (наш пир его не поддерживает) — движок обязан откатиться на открытое
        // и прийти по uTP ещё раз.
        for _ in 0..3 {
            let Some(mut stream) = incoming.recv().await else {
                return;
            };
            let mut head = [0u8; 20];
            if stream.read_exact(&mut head).await.is_err() {
                continue;
            }
            if head[0] != 19 {
                // Не BitTorrent handshake — это MSE, наш пир его не умеет.
                continue;
            }
            let mut buf = [0u8; 68];
            buf[..20].copy_from_slice(&head);
            if stream.read_exact(&mut buf[20..]).await.is_err() {
                continue;
            }
            let remote = Handshake::parse(&buf).expect("handshake разбирается");
            let _ = hash_tx.send(remote.info_hash);
            tokio::time::sleep(Duration::from_secs(1)).await;
            return;
        }
    });

    let tracker = MockTracker::start(TrackerReply::Peers(vec![addr])).await;
    let config = EngineConfig {
        download_dir,
        listen_port: 0,
        dht: false,

        ..EngineConfig::default()
    };
    let manager = TorrentManager::new(config);
    manager
        .add_file_with(sample.torrent_bytes(&tracker.url()), AddOptions::default())
        .await
        .expect("движок принял торрент");

    let received = tokio::time::timeout(Duration::from_secs(20), hash_rx)
        .await
        .expect("пир дождался handshake по uTP")
        .expect("канал не закрыт");
    assert_eq!(received, InfoHash(expected_hash));
    let _ = peer_task.await;
    manager.shutdown().await;
}
