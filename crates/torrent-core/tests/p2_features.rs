//! E2E: P2-возможности 1.6.0 — PEX (BEP 11) и веб-сиды (BEP 19).
//!
//! Оба сценария полностью локальные: mock-пиры и mock-HTTP-сервер биндятся на
//! `127.0.0.1:0`, движок слушает `listen_port: 0`. Интернет не нужен.
//!
//! Что проверяется:
//! * PEX: движок узнаёт о сидере только из `ut_pex`-сообщения (трекер его не
//!   выдавал) и всё равно докачивает торрент — то есть адреса из PEX реально
//!   попадают в очередь подключений;
//! * веб-сид: торрент без единого пира скачивается по HTTP Range-запросам, а
//!   данные на диске байт-в-байт совпадают с исходными.

mod common;

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use common::{wait_for, MockSeeder, MockTracker, Sample, TrackerReply, PIECE_LENGTH};
use torrent_core::bencode::{self, Value};
use torrent_core::wire::{Handshake, Message, HANDSHAKE_LEN};
use torrent_core::{EngineConfig, TorrentManager, TorrentState};

/// Наш id расширения `ut_pex` — движок объявляет его в extended handshake,
/// поэтому входящие PEX-сообщения приходят с этим `ext_id`.
const OUR_UT_PEX: u8 = 2;
/// Сколько ждём одного сетевого события в mock-серверах.
const MOCK_IO_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Читает скачанные файлы и сверяет их с исходником.
fn assert_files_match(root: &Path, expected: &[(String, Vec<u8>)]) {
    for (name, content) in expected {
        let path = root.join(name);
        let actual = std::fs::read(&path)
            .unwrap_or_else(|err| panic!("не удалось прочитать {}: {err}", path.display()));
        assert!(
            actual == *content,
            "файл {name} скачан с расхождениями в содержимом"
        );
    }
}

// ---------------------------------------------------------------------------
// Mock-пир с поддержкой BEP 11 (ut_pex)
// ---------------------------------------------------------------------------

/// Пир, который сразу после рукопожатия присылает PEX-сообщение со списком
/// «живых» адресов и больше ничего не отдаёт.
struct MockPexPeer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl MockPexPeer {
    /// Поднимает пира, который прорекламирует `peers` через `ut_pex`.
    async fn start(info_hash: [u8; 20], peers: Vec<SocketAddr>) -> MockPexPeer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock-пир PEX: не удалось занять 127.0.0.1:0");
        let addr = listener
            .local_addr()
            .expect("mock-пир PEX: не удалось узнать локальный адрес");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let peers = Arc::new(peers);
        let task = tokio::spawn(async move {
            loop {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                let accepted =
                    tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
                let Ok(Ok((stream, _peer))) = accepted else {
                    continue;
                };
                let peers = peers.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_pex_peer(stream, info_hash, peers).await {
                        if err.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("mock-пир PEX: {err}");
                        }
                    }
                });
            }
        });
        MockPexPeer { addr, stop, task }
    }
}

impl Drop for MockPexPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

/// Handshake + extended handshake + PEX-сообщение, дальше просто держим соединение.
async fn serve_pex_peer(
    stream: TcpStream,
    info_hash: [u8; 20],
    peers: Arc<Vec<SocketAddr>>,
) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let mut handshake = [0u8; HANDSHAKE_LEN];
    if !read_full(&mut reader, &mut handshake).await? {
        return Ok(());
    }
    let remote = Handshake::parse(&handshake)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    if remote.info_hash.as_bytes() != &info_hash {
        return Ok(());
    }
    let ours = Handshake::with_extensions(
        torrent_core::metainfo::InfoHash::new(info_hash),
        *b"TorrentaPexPeer00001",
    );
    write_all_flush(&mut writer, &ours.encode()).await?;

    // Расширенный handshake: объявляем ut_metadata и ut_pex. Номер `ut_pex`
    // здесь — наш собственный id для входящих сообщений от этого пира.
    let mut extensions = BTreeMap::new();
    extensions.insert(b"ut_metadata".to_vec(), Value::int(1));
    extensions.insert(b"ut_pex".to_vec(), Value::int(1));
    let mut hello = BTreeMap::new();
    hello.insert(b"m".to_vec(), Value::Dict(extensions));
    hello.insert(b"v".to_vec(), Value::string("TorrentaPexPeer 1.0"));
    let hello = Message::Extended {
        ext_id: 0,
        payload: bencode::encode(&Value::Dict(hello)),
    };
    write_all_flush(&mut writer, &hello.encode()).await?;

    // PEX-сообщение отправляется с *нашим* (движка) id `ut_pex`.
    let message = torrent_core::pex::PexMessage {
        added: (*peers).clone(),
        dropped: Vec::new(),
    };
    let pex = Message::Extended {
        ext_id: OUR_UT_PEX,
        payload: message.encode(),
    };
    write_all_flush(&mut writer, &pex.encode()).await?;

    // Держим соединение живым: движок может прислать interested/keep-alive.
    while let Some(payload) = read_frame(&mut reader).await? {
        if payload.is_empty() {
            continue;
        }
        // Содержимое не интересует: куски отдаёт настоящий сидер.
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mock веб-сид (BEP 19): HTTP-сервер с поддержкой Range
// ---------------------------------------------------------------------------

/// HTTP-сервер, отдающий `data` по запросам с `Range: bytes=a-b`.
struct MockWebSeed {
    url: String,
    requests: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl MockWebSeed {
    /// Поднимает сервер для указанного файла.
    async fn start(data: Arc<Vec<u8>>) -> MockWebSeed {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock веб-сид: не удалось занять 127.0.0.1:0");
        let addr = listener
            .local_addr()
            .expect("mock веб-сид: не удалось узнать локальный адрес");
        let requests = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let counter = requests.clone();
        let stopping = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                let accepted =
                    tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
                let Ok(Ok((stream, _peer))) = accepted else {
                    continue;
                };
                let data = data.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_web_seed(stream, data, counter).await {
                        if err.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("mock веб-сид: {err}");
                        }
                    }
                });
            }
        });
        MockWebSeed {
            url: format!("http://127.0.0.1:{}/files/data.bin", addr.port()),
            requests,
            stop,
            task,
        }
    }
}

impl Drop for MockWebSeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

/// Отдаёт один Range-запрос и закрывает соединение.
async fn serve_web_seed(
    mut stream: TcpStream,
    data: Arc<Vec<u8>>,
    requests: Arc<AtomicU32>,
) -> io::Result<()> {
    let mut buffer = vec![0u8; 8192];
    let mut request = Vec::new();
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    requests.fetch_add(1, Ordering::Relaxed);
    let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
    let total = data.len() as u64;
    let Some((start, end)) = head
        .lines()
        .find_map(|line| line.strip_prefix("range: bytes="))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, end)| {
            let start = start.trim().parse::<u64>().ok()?;
            let end = end.trim().parse::<u64>().ok()?;
            Some((start, end))
        })
    else {
        // Без Range отдаём файл целиком: движок умеет вырезать кусок сам.
        let head =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
        write_all_flush(&mut stream, head.as_bytes()).await?;
        write_all_flush(&mut stream, &data).await?;
        return Ok(());
    };
    let start = start.min(total);
    let end = end.min(total.saturating_sub(1));
    let body = &data[start as usize..=end as usize];
    let head = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    write_all_flush(&mut stream, head.as_bytes()).await?;
    write_all_flush(&mut stream, body).await
}

// ---------------------------------------------------------------------------
// Общие помощники для mock-серверов
// ---------------------------------------------------------------------------

/// Читает один peer wire frame (4 байта длины + payload) с таймаутом.
async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut head = [0u8; 4];
    match tokio::time::timeout(MOCK_IO_TIMEOUT, reader.read_exact(&mut head)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => return Err(err),
        Err(_) => return Ok(None),
    }
    let len = u32::from_be_bytes(head) as usize;
    if len == 0 {
        return Ok(Some(Vec::new()));
    }
    let mut payload = vec![0u8; len];
    match tokio::time::timeout(MOCK_IO_TIMEOUT, reader.read_exact(&mut payload)).await {
        Ok(Ok(_)) => Ok(Some(payload)),
        Ok(Err(err)) => Err(err),
        Err(_) => Ok(None),
    }
}

/// Читает ровно `buf.len()` байт с таймаутом.
async fn read_full<R>(reader: &mut R, buf: &mut [u8]) -> io::Result<bool>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match tokio::time::timeout(MOCK_IO_TIMEOUT, reader.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(err)) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Ok(Err(err)) => Err(err),
        Err(_) => Ok(false),
    }
}

/// Пишет байты и сбрасывает буфер.
async fn write_all_flush<W>(writer: &mut W, bytes: &[u8]) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    writer.write_all(bytes).await?;
    writer.flush().await
}

/// Собирает `.torrent` с одним трекером и полем `url-list` (BEP 19).
fn torrent_with_web_seed(sample: &Sample, announce: &str, url: &str) -> Vec<u8> {
    let info = bencode::decode_all(&sample.info_bytes)
        .expect("info-словарь тестового торрента должен разбираться bencode");
    let mut outer = BTreeMap::new();
    outer.insert(b"announce".to_vec(), Value::string(announce));
    outer.insert(b"info".to_vec(), info);
    outer.insert(b"url-list".to_vec(), Value::string(url));
    bencode::encode(&Value::Dict(outer))
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_peers_from_pex_without_tracker_help() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "torrenta-pex",
        PIECE_LENGTH,
        &[("data.bin", 70_000)],
    ));

    // Настоящий сидер: трекер о нём не знает, адрес придёт только через PEX.
    let seeder = MockSeeder::start(sample.clone(), common::Corruption::None).await;
    let pex_peer = MockPexPeer::start(sample.info_hash, vec![seeder.addr]).await;
    // Трекер выдаёт только PEX-пира.
    let tracker = MockTracker::start(TrackerReply::Peers(vec![pex_peer.addr])).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let id = manager
        .add_file(sample.torrent_bytes(&tracker.url()), None)
        .await
        .expect("движок не принял корректный .torrent");

    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "загрузка через пиров из PEX завершилась",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;

    assert_eq!(snapshot.error, None, "ошибок быть не должно");
    assert_eq!(
        snapshot.downloaded_bytes,
        sample.data.len() as u64,
        "весь торрент должен быть скачан у сидера, найденного через PEX"
    );
    assert!(
        seeder.requests.load(Ordering::Relaxed) > 0,
        "сидер из PEX не получил ни одного запроса куска"
    );
    assert_files_match(&download_dir, &sample.files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_from_web_seed_without_any_peers() {
    let temp = tempfile::tempdir().expect("не удалось создать временный каталог");
    let download_dir = temp.path().join("downloads");
    let sample = Arc::new(Sample::new(
        "torrenta-web-seed",
        PIECE_LENGTH,
        &[("data.bin", 70_000)],
    ));
    let web_seed = MockWebSeed::start(Arc::new(sample.data.clone())).await;
    // Трекер отдаёт пустой список пиров: качать можно только с веб-сида.
    let tracker = MockTracker::start(TrackerReply::Peers(Vec::new())).await;
    let manager = TorrentManager::new(test_config(&download_dir));

    let bytes = torrent_with_web_seed(&sample, &tracker.url(), &web_seed.url);
    let id = manager
        .add_file(bytes, None)
        .await
        .expect("движок не принял корректный .torrent");

    let snapshot = wait_for(
        &manager,
        id,
        Duration::from_secs(60),
        "загрузка с веб-сида завершилась",
        |item| matches!(item.state, TorrentState::Seeding | TorrentState::Completed),
    )
    .await;

    assert_eq!(snapshot.error, None, "ошибок быть не должно");
    assert_eq!(
        snapshot.downloaded_bytes,
        sample.data.len() as u64,
        "весь торрент должен быть скачан с веб-сида"
    );
    assert_eq!(snapshot.pieces_have, snapshot.piece_count);
    assert!(
        web_seed.requests.load(Ordering::Relaxed) > 0,
        "веб-сид не получил ни одного запроса"
    );
    assert_files_match(&download_dir, &sample.files);
}
