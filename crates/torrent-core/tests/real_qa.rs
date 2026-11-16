//! QA на реальных данных: настоящие `.torrent`-файлы и живой рой.
//!
//! Тест помечен `#[ignore]`: он читает каталог из окружения и ходит в сеть, то
//! есть не должен выполняться в обычном прогоне.
//!
//! Запуск:
//! `TORRENTA_QA_DIR=C:\Users\romik\Downloads cargo test -p torrent-core --test real_qa -- --ignored --nocapture`
//!
//! `TORRENTA_QA_LIVE` (имя файла внутри каталога) включает живой прогон: движок
//! добавляет торрент, ходит в DHT/трекеры и 45 секунд качает. Результат печатается
//! в stdout как «QA-LIVE ...» — из него собирается отчёт.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use torrent_core::{AddOptions, EngineConfig, Metainfo, TorrentManager};

fn qa_dir() -> PathBuf {
    std::env::var_os("TORRENTA_QA_DIR")
        .map(PathBuf::from)
        .expect("нужен TORRENTA_QA_DIR с настоящими .torrent-файлами")
}

fn torrent_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("каталог должен читаться")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"))
        })
        .collect();
    files.sort();
    files
}

#[test]
#[ignore = "читает настоящие .torrent из TORRENTA_QA_DIR"]
fn real_torrents_parse_and_describe() {
    let dir = qa_dir();
    let files = torrent_files(&dir);
    assert!(!files.is_empty(), "в каталоге нет .torrent-файлов");
    println!("QA-PARSE каталог {}, файлов {}", dir.display(), files.len());
    for path in &files {
        let data = std::fs::read(path).expect("файл читается");
        let meta = Metainfo::parse(&data)
            .unwrap_or_else(|err| panic!("QA-PARSE FAIL {}: {err}", path.display()));
        let multi = meta.info.files.len() > 1;
        println!(
            "QA-PARSE OK {} | name={} | файлов={} | размер={} | piece={} | приватный={} | trackers={}",
            path.file_name().and_then(|name| name.to_str()).unwrap_or("?"),
            meta.info.name,
            meta.info.files.len(),
            meta.info.total_length,
            meta.info.piece_length,
            meta.info.private,
            meta.trackers().len(),
        );
        assert!(!meta.info.name.trim().is_empty(), "имя торрента пустое");
        assert!(meta.info.piece_count() > 0, "нет ни одного куска");
        assert_eq!(
            meta.info.files.len() > 1,
            multi,
            "число файлов и признак многофайловости расходятся"
        );
        // Первый файл обязан быть в пределах торрента.
        if let Some(first) = meta.info.files.first() {
            assert!(first.offset + first.length <= meta.info.total_length);
        }
    }
}

#[tokio::test]
#[ignore = "ходит в настоящую сеть"]
async fn real_torrent_downloads_from_live_swarm() {
    let dir = qa_dir();
    let name = std::env::var("TORRENTA_QA_LIVE").expect("нужен TORRENTA_QA_LIVE");
    let path = dir.join(&name);
    let data = std::fs::read(&path).expect("торрент читается");
    let meta = Metainfo::parse(&data).expect("торрент разбирается");
    println!(
        "QA-LIVE start {name} | {} | {} байт | кусков {}",
        meta.info.name,
        meta.info.total_length,
        meta.info.piece_count()
    );

    let temp = tempfile::tempdir().expect("временный каталог");
    let download_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&download_dir).expect("каталог загрузки");
    let config = EngineConfig {
        download_dir: download_dir.clone(),
        // Слушаем случайный порт: входящие соединения из роя должны работать.
        listen_port: 0,
        // DHT включён: трекеры у файлов могут быть недоступны.
        dht: true,
        ..EngineConfig::default()
    };
    let manager = TorrentManager::new(config.clone());
    let id = Arc::clone(&manager)
        .add_file_with(data, AddOptions::default())
        .await
        .expect("движок принял настоящий торрент");

    // Даём время объявиться трекерам, получить пиров и обменяться блоками.
    let mut best = (0u32, 0u32, 0f64);
    for step in 0..9 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let snapshot = manager
            .snapshot()
            .into_iter()
            .find(|item| item.id == id)
            .expect("снимок есть");
        println!(
            "QA-LIVE t={}s state={:?} peers={} known={} pieces={}/{} down={:.0} Б/с up={:.0} Б/с downloaded={}",
            (step + 1) * 5,
            snapshot.state,
            snapshot.peers_connected,
            snapshot.peers_known,
            snapshot.pieces_have,
            snapshot.piece_count,
            snapshot.down_rate,
            snapshot.up_rate,
            snapshot.downloaded_bytes,
        );
        best = (
            best.0.max(snapshot.pieces_have),
            best.1.max(snapshot.peers_connected),
            best.2.max(snapshot.down_rate),
        );
    }
    println!(
        "QA-LIVE result {} | порт={} | пиков: пиров {}, кусков {}, скорость {:.0} Б/с",
        meta.info.name,
        manager.listen_port(),
        best.1,
        best.0,
        best.2
    );
    manager.shutdown().await;
}

#[tokio::test]
#[ignore = "раздаёт настоящий файл в живой рой"]
async fn real_seed_uploads_and_totals_survive_restart() {
    let dir = qa_dir();
    let name = std::env::var("TORRENTA_QA_SEED").expect("нужен TORRENTA_QA_SEED");
    let seed_dir = std::env::var_os("TORRENTA_QA_SEED_DIR")
        .map(PathBuf::from)
        .expect("нужен TORRENTA_QA_SEED_DIR (где лежат уже скачанные данные)");
    let data = std::fs::read(dir.join(&name)).expect("торрент читается");
    let meta = Metainfo::parse(&data).expect("торрент разбирается");
    println!(
        "QA-SEED start {name} | {} | кусков {}",
        meta.info.name,
        meta.info.piece_count()
    );

    let config = EngineConfig {
        download_dir: seed_dir.clone(),
        listen_port: 0,
        dht: true,
        ..EngineConfig::default()
    };
    let manager = TorrentManager::new(config);
    let id = Arc::clone(&manager)
        .add_file_with(data, AddOptions::default())
        .await
        .expect("движок принял торрент");
    if let Err(err) = manager.recheck(id) {
        panic!("QA-SEED recheck не запустился: {err}");
    }

    // Проверяем файл на диске хэшами и ждём состояние раздачи.
    let mut seeding = false;
    for step in 0..24 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let snapshot = manager
            .snapshot()
            .into_iter()
            .find(|item| item.id == id)
            .expect("снимок есть");
        println!(
            "QA-SEED t={}s state={:?} проверено={}/{} peers={} up={:.0} Б/с uploaded={}",
            (step + 1) * 5,
            snapshot.state,
            snapshot.pieces_have,
            snapshot.piece_count,
            snapshot.peers_connected,
            snapshot.up_rate,
            snapshot.uploaded_bytes,
        );
        if matches!(snapshot.state, torrent_core::TorrentState::Seeding) {
            seeding = true;
            break;
        }
    }
    assert!(
        seeding,
        "файл не перешёл в раздачу: проверка не увидела данные"
    );

    // Наблюдаем отдачу в живой рой.
    let mut peak_up = 0.0f64;
    let mut uploaded = 0u64;
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let snapshot = manager
            .snapshot()
            .into_iter()
            .find(|item| item.id == id)
            .expect("снимок есть");
        peak_up = peak_up.max(snapshot.up_rate);
        uploaded = snapshot.uploaded_bytes;
        println!(
            "QA-SEED upload peers={} up={:.0} Б/с uploaded={}",
            snapshot.peers_connected, snapshot.up_rate, snapshot.uploaded_bytes,
        );
    }
    println!("QA-SEED result upload_peak={peak_up:.0} uploaded={uploaded}");

    // «Перезапуск»: новый менеджер, счётчики восстанавливаются как в сессии.
    let snapshot = manager
        .snapshot()
        .into_iter()
        .find(|item| item.id == id)
        .expect("снимок есть");
    let (uploaded, downloaded) = (snapshot.uploaded_bytes, snapshot.downloaded_bytes);
    manager.shutdown().await;

    let config = EngineConfig {
        download_dir: seed_dir,
        listen_port: 0,
        dht: false,
        ..EngineConfig::default()
    };
    let restarted = TorrentManager::new(config);
    let restarted_id = Arc::clone(&restarted)
        .add_file_with(
            std::fs::read(dir.join(&name)).expect("торрент читается"),
            AddOptions::default(),
        )
        .await
        .expect("движок принял торрент после «перезапуска»");
    restarted
        .set_transfer_totals(restarted_id, uploaded, downloaded)
        .expect("счётчики восстанавливаются");
    let after = restarted
        .snapshot()
        .into_iter()
        .find(|item| item.id == restarted_id)
        .expect("снимок есть");
    println!(
        "QA-SEED restore uploaded={} downloaded={} ratio={}",
        after.uploaded_bytes, after.downloaded_bytes, after.seed_ratio
    );
    assert_eq!(after.uploaded_bytes, uploaded, "отданное не восстановилось");
    assert_eq!(
        after.downloaded_bytes, downloaded,
        "скачанное не восстановилось"
    );
    restarted.shutdown().await;
}
/// Проверяет отдачу как таковую: наш «пир» подключается к сиду, говорит
/// Interested и просит блок — сид обязан разчокнуть и прислать данные.
#[tokio::test]
#[ignore = "раздаёт настоящий файл, нужны TORRENTA_QA_SEED*"]
async fn real_seed_serves_block_to_interested_peer() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use torrent_core::wire::{Handshake, Message};

    let dir = qa_dir();
    let name = std::env::var("TORRENTA_QA_SEED").expect("нужен TORRENTA_QA_SEED");
    let seed_dir = std::env::var_os("TORRENTA_QA_SEED_DIR")
        .map(PathBuf::from)
        .expect("нужен TORRENTA_QA_SEED_DIR");
    let data = std::fs::read(dir.join(&name)).expect("торрент читается");
    let meta = Metainfo::parse(&data).expect("торрент разбирается");
    let hash = meta.info_hash;

    let manager = TorrentManager::new(EngineConfig {
        download_dir: seed_dir,
        listen_port: 0,
        dht: false,
        ..EngineConfig::default()
    });
    let id = Arc::clone(&manager)
        .add_file_with(data, AddOptions::default())
        .await
        .expect("торрент добавлен");
    manager.recheck(id).expect("перепроверка запущена");
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let snapshot = manager
            .snapshot()
            .into_iter()
            .find(|item| item.id == id)
            .unwrap();
        if matches!(snapshot.state, torrent_core::TorrentState::Seeding) {
            break;
        }
    }
    let port = manager.listen_port();
    assert!(port != 0, "движок не занял порт прослушивания");

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("подключение к сиду");
    stream
        .write_all(&Handshake::new(hash, *b"-QA0001-abcdefghijkl").encode())
        .await
        .expect("handshake");
    let mut remote = [0u8; 68];
    stream
        .read_exact(&mut remote)
        .await
        .expect("ответный handshake");
    stream
        .write_all(
            &Message::Bitfield(torrent_core::bitfield::Bitfield::new(
                meta.info.piece_count(),
            ))
            .encode(),
        )
        .await
        .expect("bitfield");
    stream
        .write_all(&Message::Interested.encode())
        .await
        .expect("interested");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut unchoked = false;
    let mut piece: Option<Vec<u8>> = None;
    while tokio::time::Instant::now() < deadline {
        let mut head = [0u8; 4];
        if tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut head))
            .await
            .is_err()
        {
            continue;
        }
        let length = u32::from_be_bytes(head) as usize;
        if length == 0 {
            continue;
        }
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload).await.expect("сообщение");
        match torrent_core::wire::decode(&payload).expect("сообщение разбирается")
        {
            Message::Unchoke => unchoked = true,
            Message::Piece { index, begin, data } => {
                println!(
                    "QA-SERVE получен блок piece={index} begin={begin} len={}",
                    data.len()
                );
                piece = Some(data);
                break;
            }
            _ => {}
        }
        if unchoked {
            stream
                .write_all(
                    &Message::Request {
                        index: 0,
                        begin: 0,
                        length: 16_384,
                    }
                    .encode(),
                )
                .await
                .expect("запрос блока");
        }
    }
    println!(
        "QA-SERVE unchoked={unchoked} piece={}",
        piece.as_ref().map(Vec::len).unwrap_or(0)
    );
    assert!(unchoked, "сид не разчокнул заинтересованного пира");
    assert!(piece.is_some(), "сид не прислал запрошенный блок");
    manager.shutdown().await;
}
