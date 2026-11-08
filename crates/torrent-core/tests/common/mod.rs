//! Общие помощники для E2E-тестов движка.
//!
//! Здесь живёт всё, что нужно, чтобы движок реально скачивал данные по сети:
//!
//! * [`Sample`] — детерминированный тестовый торрент (файлы + SHA-1 кусков +
//!   готовые байты `.torrent`);
//! * [`MockTracker`] — HTTP-трекер на `127.0.0.1:0` с компактным списком пиров
//!   или с `failure reason`;
//! * [`MockSeeder`] — сидер на `127.0.0.1:0`, отдающий куски по peer wire
//!   protocol (умеет отдавать один испорченный кусок для негативных сценариев);
//! * [`MockMetadataPeer`] — пир с поддержкой BEP 9 (`ut_metadata`), отдающий
//!   `info`-словарь по расширенному протоколу (для magnet-ссылок).
//!
//! Никаких внешних адресов: все сокеты биндятся на `127.0.0.1:0`, интернет не
//! нужен, параллельные прогоны не конфликтуют по портам.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use torrent_core::bencode::{self, Value};
use torrent_core::metainfo::InfoHash;
use torrent_core::wire::{self, Handshake, Message, HANDSHAKE_LEN};
use torrent_core::{Bitfield, Metainfo, TorrentManager, TorrentSnapshot};

/// Сколько ждём одного сетевого события в mock-серверах.
const MOCK_IO_TIMEOUT: Duration = Duration::from_secs(20);

/// Размер куска по умолчанию для тестовых торрентов (32 КиБ).
pub const PIECE_LENGTH: u32 = 32 * 1024;

/// Размер куска метаданных в BEP 9.
pub const METADATA_PIECE: usize = 16 * 1024;

/// Наш id расширения `ut_metadata` (движок объявляет его в extended handshake).
pub const OUR_UT_METADATA: u8 = 1;

// ---------------------------------------------------------------------------
// Тестовый торрент
// ---------------------------------------------------------------------------

/// Детерминированный торрент: файлы, поток байт, SHA-1 кусков и байты `.torrent`.
pub struct Sample {
    /// Имя торрента (`info.name`).
    pub name: String,
    /// Размер куска.
    pub piece_length: u32,
    /// Файлы: `(относительный путь, содержимое)`.
    pub files: Vec<(String, Vec<u8>)>,
    /// Непрерывный поток данных торрента (конкатенация файлов).
    pub data: Vec<u8>,
    /// SHA-1 каждого куска.
    pub pieces: Vec<[u8; 20]>,
    /// Точные байты `info`-словаря (по ним считается info-hash).
    pub info_bytes: Vec<u8>,
    /// info-hash торрента (SHA-1 от `info_bytes`).
    pub info_hash: [u8; 20],
}

impl Sample {
    /// Собирает торрент из списка `(имя файла, размер)`.
    ///
    /// Содержимое детерминировано (линейный конгруэнтный генератор), поэтому
    /// повторный запуск теста даёт те же байты и тот же info-hash.
    pub fn new(name: &str, piece_length: u32, sizes: &[(&str, usize)]) -> Sample {
        let mut data: Vec<u8> = Vec::new();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut state: u32 = 0x1234_5678;
        for (file_name, size) in sizes {
            let mut content = Vec::with_capacity(*size);
            for _ in 0..*size {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                content.push((state >> 16) as u8);
            }
            data.extend_from_slice(&content);
            files.push(((*file_name).to_string(), content));
        }
        let pieces: Vec<[u8; 20]> = data
            .chunks(piece_length as usize)
            .map(sha1_bytes)
            .collect();
        let info_bytes = info_dict(name, piece_length, &pieces, &files);
        let info_hash = sha1_bytes(&info_bytes);
        Sample {
            name: name.to_string(),
            piece_length,
            files,
            data,
            pieces,
            info_bytes,
            info_hash,
        }
    }

    /// Количество кусков.
    pub fn piece_count(&self) -> usize {
        self.data.len().div_ceil(self.piece_length as usize)
    }

    /// Байты куска `index` (последний кусок может быть короче).
    pub fn piece(&self, index: usize) -> Option<&[u8]> {
        let start = index.checked_mul(self.piece_length as usize)?;
        if start >= self.data.len() {
            return None;
        }
        let end = (start + self.piece_length as usize).min(self.data.len());
        Some(&self.data[start..end])
    }

    /// Байты `.torrent`-файла с указанным адресом трекера.
    ///
    /// `info` вставляется вложенным словарём (именно так его ожидает
    /// `Metainfo::parse`: он ищет границы `info` в исходном буфере). Кодирование
    /// bencode канонично, поэтому байты `info` совпадают с [`Sample::info_bytes`],
    /// а info-hash — с [`Sample::info_hash`].
    pub fn torrent_bytes(&self, announce: &str) -> Vec<u8> {
        let info = bencode::decode_all(&self.info_bytes)
            .expect("info-словарь тестового торрента должен разбираться bencode");
        assert_eq!(
            bencode::encode(&info),
            self.info_bytes,
            "info-словарь должен кодироваться канонически, иначе info-hash не совпадёт"
        );
        let mut outer = BTreeMap::new();
        outer.insert(b"announce".to_vec(), Value::string(announce));
        outer.insert(b"info".to_vec(), info);
        let bytes = bencode::encode(&Value::Dict(outer));
        assert_eq!(
            Metainfo::parse(&bytes)
                .map(|meta| meta.info_hash)
                .unwrap_or_else(|err| panic!("тестовый .torrent не разобрался: {err}")),
            InfoHash::new(self.info_hash),
            "info-hash собранного .torrent должен совпадать с ожидаемым"
        );
        bytes
    }

    /// Magnet-ссылка на этот торрент с одним трекером.
    pub fn magnet_link(&self, announce: &str) -> String {
        format!(
            "magnet:?xt=urn:btih:{}&dn={}&tr={}",
            hex(&self.info_hash),
            self.name,
            encode_query_value(announce)
        )
    }
}

/// Кодирует `info`-словарь (multi-file раскладка: `files` + `path`).
fn info_dict(
    name: &str,
    piece_length: u32,
    pieces: &[[u8; 20]],
    files: &[(String, Vec<u8>)],
) -> Vec<u8> {
    let mut hashes = Vec::with_capacity(pieces.len() * 20);
    for digest in pieces {
        hashes.extend_from_slice(digest);
    }
    let entries: Vec<Value> = files
        .iter()
        .map(|(path, content)| {
            let mut file = BTreeMap::new();
            file.insert(b"length".to_vec(), Value::int(content.len() as i64));
            file.insert(
                b"path".to_vec(),
                Value::List(vec![Value::string(path.as_str())]),
            );
            Value::Dict(file)
        })
        .collect();
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), Value::string(name));
    info.insert(
        b"piece length".to_vec(),
        Value::int(piece_length as i64),
    );
    info.insert(b"pieces".to_vec(), Value::bytes(hashes));
    info.insert(b"files".to_vec(), Value::List(entries));
    bencode::encode(&Value::Dict(info))
}

// ---------------------------------------------------------------------------
// Вспомогательные функции
// ---------------------------------------------------------------------------

/// SHA-1 от среза.
pub fn sha1_bytes(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest);
    out
}

/// Нижний hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Percent-encoding для значения query-строки magnet-ссылки.
///
/// Достаточно экранировать разделители query: `:` и `/` (и `%` на всякий случай).
fn encode_query_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            ':' => out.push_str("%3A"),
            '/' => out.push_str("%2F"),
            '%' => out.push_str("%25"),
            other => out.push(other),
        }
    }
    out
}

/// Ждёт, пока снимок торрента удовлетворит условию.
///
/// Всё ожидание завёрнуто в `tokio::time::timeout`: тест не может зависнуть
/// навсегда, при таймауте печатается последний снимок.
pub async fn wait_for<F>(
    manager: &Arc<TorrentManager>,
    id: u64,
    limit: Duration,
    what: &str,
    mut done: F,
) -> TorrentSnapshot
where
    F: FnMut(&TorrentSnapshot) -> bool,
{
    let deadline = tokio::time::Instant::now() + limit;
    tokio::time::timeout(limit + Duration::from_secs(5), async {
        loop {
            if let Some(snapshot) = snapshot_of(manager, id) {
                if done(&snapshot) {
                    return snapshot;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let last = snapshot_of(manager, id);
                panic!("не дождались события «{what}», последний снимок: {last:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("внешний таймаут ожидания состояния движка")
}

/// Текущий снимок торрента по id.
pub fn snapshot_of(manager: &Arc<TorrentManager>, id: u64) -> Option<TorrentSnapshot> {
    manager.snapshot().into_iter().find(|item| item.id == id)
}

/// Снимок торрента по id, паникует, если торрент пропал.
pub fn require_snapshot(manager: &Arc<TorrentManager>, id: u64) -> TorrentSnapshot {
    snapshot_of(manager, id).expect("торрент неожиданно исчез из менеджера")
}

/// Читает один peer wire frame (4 байта длины + payload) с таймаутом.
///
/// `Ok(None)` — соединение закрылось или истёк таймаут; пустой `Vec` — keep-alive.
async fn read_frame<R>(reader: &mut R, limit: Duration) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut head = [0u8; 4];
    match tokio::time::timeout(limit, reader.read_exact(&mut head)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => return Err(err),
        Err(_) => return Ok(None),
    }
    let len = u32::from_be_bytes(head) as usize;
    if len == 0 {
        return Ok(Some(Vec::new()));
    }
    if len > 4 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "сообщение пира слишком большое",
        ));
    }
    let mut payload = vec![0u8; len];
    match tokio::time::timeout(limit, reader.read_exact(&mut payload)).await {
        Ok(Ok(_)) => Ok(Some(payload)),
        Ok(Err(err)) => Err(err),
        Err(_) => Ok(None),
    }
}

/// Читает ровно `buf.len()` байт с таймаутом.
async fn read_full<R>(reader: &mut R, buf: &mut [u8], limit: Duration) -> io::Result<bool>
where
    R: AsyncRead + Unpin,
{
    match tokio::time::timeout(limit, reader.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(err)) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Ok(Err(err)) => Err(err),
        Err(_) => Ok(false),
    }
}

/// Пишет байты и сбрасывает буфер.
async fn write_all_flush<W>(writer: &mut W, bytes: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(bytes).await?;
    writer.flush().await
}

// ---------------------------------------------------------------------------
// Mock HTTP-трекер
// ---------------------------------------------------------------------------

/// Что отвечает mock-трекер.
pub enum TrackerReply {
    /// Успешный announce с компактным списком пиров.
    Peers(Vec<SocketAddr>),
    /// Отказ: `failure reason` (движок должен пережить его без падения).
    Failure(String),
}

/// HTTP-трекер на `127.0.0.1:0`.
pub struct MockTracker {
    /// Адрес трекера.
    pub addr: SocketAddr,
    /// Сколько announce реально дошло.
    pub announces: Arc<AtomicU32>,
    /// Параметры дошедших announce (в порядке прихода).
    pub log: Arc<Mutex<Vec<TrackerAnnounce>>>,
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

/// Параметры одного объявления, которые запоминает mock-трекер.
#[derive(Clone, Debug, Default)]
pub struct TrackerAnnounce {
    /// Сколько байт осталось скачать по мнению движка.
    pub left: u64,
    /// Отдано за сессию.
    pub uploaded: u64,
    /// Скачано за сессию.
    pub downloaded: u64,
    /// Событие (`started`, `completed`, …), если было.
    pub event: Option<String>,
}

/// Достаёт параметры announce из query-строки HTTP-запроса.
fn parse_announce_query(request: &[u8]) -> TrackerAnnounce {
    let text = String::from_utf8_lossy(request);
    let mut announce = TrackerAnnounce::default();
    let Some(line) = text.lines().next() else {
        return announce;
    };
    let Some((_path, query)) = line.split_once('?') else {
        return announce;
    };
    let query = query.split_whitespace().next().unwrap_or(query);
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        match name {
            "left" => announce.left = value.parse().unwrap_or(0),
            "uploaded" => announce.uploaded = value.parse().unwrap_or(0),
            "downloaded" => announce.downloaded = value.parse().unwrap_or(0),
            "event" => announce.event = Some(value.to_string()),
            _ => {}
        }
    }
    announce
}

impl MockTracker {
    /// Поднимает трекер с одним и тем же ответом и `interval` 30 с.
    pub async fn start(reply: TrackerReply) -> MockTracker {
        MockTracker::start_with_interval(reply, 30).await
    }

    /// Поднимает трекер с фиксированным ответом и своим `interval` (секунды).
    ///
    /// Большой интервал (например 600) нужен регресс-тестам, которые доказывают,
    /// что движок обходится без повторного объявления.
    pub async fn start_with_interval(reply: TrackerReply, interval: u32) -> MockTracker {
        MockTracker::start_script(vec![reply], interval).await
    }

    /// Поднимает трекер, который отвечает по очереди: `replies[0]` на первый
    /// announce, `replies[1]` на второй и так далее, последний ответ повторяется.
    pub async fn start_script(replies: Vec<TrackerReply>, interval: u32) -> MockTracker {
        assert!(
            !replies.is_empty(),
            "mock-трекер: нужен хотя бы один ответ"
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock-трекер: не удалось занять 127.0.0.1:0");
        let addr = listener
            .local_addr()
            .expect("mock-трекер: не удалось узнать локальный адрес");
        let announces = Arc::new(AtomicU32::new(0));
        let log: Arc<Mutex<Vec<TrackerAnnounce>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let bodies: Arc<Vec<Vec<u8>>> = Arc::new(
            replies
                .iter()
                .map(|reply| tracker_body(reply, interval))
                .collect(),
        );
        let counter = announces.clone();
        let stopping = stop.clone();
        let recorded = log.clone();
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
                let seen = counter.fetch_add(1, Ordering::Relaxed) as usize;
                let position = seen.min(bodies.len().saturating_sub(1));
                let body = bodies[position].clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_tracker(stream, body, recorded).await {
                        eprintln!("mock-трекер: {err}");
                    }
                });
            }
        });
        MockTracker {
            addr,
            announces,
            log,
            stop,
            task,
        }
    }

    /// URL announce для `.torrent`-файла.
    pub fn url(&self) -> String {
        format!("http://{}/announce", self.addr)
    }

    /// Сколько announce пришло.
    pub fn announce_count(&self) -> u32 {
        self.announces.load(Ordering::Relaxed)
    }

    /// Копия журнала announce: что именно присылал движок.
    pub fn log(&self) -> Vec<TrackerAnnounce> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for MockTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

/// Тело ответа трекера в bencode.
fn tracker_body(reply: &TrackerReply, interval: u32) -> Vec<u8> {
    match reply {
        TrackerReply::Peers(peers) => {
            let mut compact = Vec::with_capacity(peers.len() * 6);
            for peer in peers {
                match peer.ip() {
                    std::net::IpAddr::V4(ip) => compact.extend_from_slice(&ip.octets()),
                    std::net::IpAddr::V6(_) => continue,
                }
                compact.extend_from_slice(&peer.port().to_be_bytes());
            }
            let mut dict = BTreeMap::new();
            dict.insert(b"interval".to_vec(), Value::int(interval as i64));
            dict.insert(b"peers".to_vec(), Value::bytes(compact));
            bencode::encode(&Value::Dict(dict))
        }
        TrackerReply::Failure(reason) => {
            let mut dict = BTreeMap::new();
            dict.insert(b"failure reason".to_vec(), Value::string(reason.as_str()));
            bencode::encode(&Value::Dict(dict))
        }
    }
}

/// Отвечает на один HTTP-запрос трекера.
async fn serve_tracker(
    mut stream: TcpStream,
    body: Vec<u8>,
    log: Arc<Mutex<Vec<TrackerAnnounce>>>,
) -> io::Result<()> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = match tokio::time::timeout(MOCK_IO_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(count)) => count,
            Ok(Err(err)) => return Err(err),
            Err(_) => break,
        };
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() > 16 * 1024 {
            break;
        }
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    let announce = parse_announce_query(&request);
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(announce);
    Ok(())
}

// ---------------------------------------------------------------------------
// Mock-сидер (обычная раздача кусков)
// ---------------------------------------------------------------------------

/// Как mock-сидер портит отдаваемые данные.
#[derive(Clone, Copy, Debug)]
pub enum Corruption {
    /// Данные всегда верные.
    None,
    /// Первые `times` ответов на блок 0 куска `index` испорчены, дальше — верные
    /// (движок должен отбраковать кусок и перекачать его).
    Transient {
        /// Индекс портимого куска.
        index: usize,
        /// Сколько испорченных ответов отдать, прежде чем начать отдавать верные.
        times: u32,
    },
    /// Блок 0 куска `index` испорчен всегда.
    Permanent {
        /// Индекс портимого куска.
        index: usize,
    },
}

/// Состояние одного сеанса mock-сидера.
struct SeederState {
    sample: Arc<Sample>,
    corruption: Corruption,
    /// Сколько испорченных блоков уже отдано (нужно для [`Corruption::Transient`]).
    corrupt_hits: Arc<AtomicU32>,
    requests: Arc<AtomicU32>,
    connections: Arc<AtomicU32>,
}

impl SeederState {
    /// Нужно ли испортить ответ на блок `begin == 0` куска `index`.
    fn should_corrupt(&self, index: usize) -> bool {
        match self.corruption {
            Corruption::None => false,
            Corruption::Permanent { index: target } => index == target,
            Corruption::Transient { index: target, times } => {
                index == target && self.corrupt_hits.fetch_add(1, Ordering::Relaxed) < times
            }
        }
    }
}

/// Сидер на `127.0.0.1:0`, отдающий куски из [`Sample`].
pub struct MockSeeder {
    /// Адрес сидера.
    pub addr: SocketAddr,
    /// Сколько TCP-соединений принято.
    pub connections: Arc<AtomicU32>,
    /// Сколько запросов кусков пришло.
    pub requests: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl MockSeeder {
    /// Поднимает сидера с указанным режимом порчи данных.
    pub async fn start(sample: Arc<Sample>, corruption: Corruption) -> MockSeeder {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock-сидер: не удалось занять 127.0.0.1:0");
        let addr = listener
            .local_addr()
            .expect("mock-сидер: не удалось узнать локальный адрес");
        let connections = Arc::new(AtomicU32::new(0));
        let requests = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(SeederState {
            sample,
            corruption,
            corrupt_hits: Arc::new(AtomicU32::new(0)),
            requests: requests.clone(),
            connections: connections.clone(),
        });
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
                state.connections.fetch_add(1, Ordering::Relaxed);
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_seeder(stream, state).await {
                        if err.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("mock-сидер: {err}");
                        }
                    }
                });
            }
        });
        MockSeeder {
            addr,
            connections,
            requests,
            stop,
            task,
        }
    }
}

impl Drop for MockSeeder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

/// Handshake + Bitfield + Unchoke, затем обслуживание запросов кусков.
async fn serve_seeder(stream: TcpStream, state: Arc<SeederState>) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let mut handshake = [0u8; HANDSHAKE_LEN];
    if !read_full(&mut reader, &mut handshake, MOCK_IO_TIMEOUT).await? {
        return Ok(());
    }
    let remote = Handshake::parse(&handshake)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    if remote.info_hash.as_bytes() != &state.sample.info_hash {
        return Ok(());
    }
    let ours = Handshake::new(
        InfoHash::new(state.sample.info_hash),
        *b"TorrentaMockSeeder01",
    );
    write_all_flush(&mut writer, &ours.encode()).await?;
    let bitfield = Bitfield::all_set(state.sample.piece_count());
    write_all_flush(&mut writer, &Message::Bitfield(bitfield).encode()).await?;
    write_all_flush(&mut writer, &Message::Unchoke.encode()).await?;

    while let Some(payload) = read_frame(&mut reader, MOCK_IO_TIMEOUT).await? {
        if payload.is_empty() {
            continue;
        }
        let Ok(message) = wire::decode(&payload) else {
            break;
        };
        match message {
            Message::Request {
                index,
                begin,
                length,
            } => {
                state.requests.fetch_add(1, Ordering::Relaxed);
                let (Ok(index), Ok(begin), Ok(length)) = (
                    usize::try_from(index),
                    usize::try_from(begin),
                    usize::try_from(length),
                ) else {
                    break;
                };
                let Some(mut data) = block_of(&state.sample, index, begin, length) else {
                    break;
                };
                if begin == 0 && !data.is_empty() && state.should_corrupt(index) {
                    data[0] ^= 0x5A;
                }
                let piece = Message::Piece {
                    index: index as u32,
                    begin: begin as u32,
                    data,
                };
                write_all_flush(&mut writer, &piece.encode()).await?;
            }
            Message::KeepAlive
            | Message::Interested
            | Message::NotInterested
            | Message::Unchoke
            | Message::Choke
            | Message::Have(_) => {}
            _ => {}
        }
    }
    Ok(())
}

/// Вырезает блок `begin..begin+length` из куска.
fn block_of(sample: &Sample, index: usize, begin: usize, length: usize) -> Option<Vec<u8>> {
    let piece = sample.piece(index)?;
    let end = begin.checked_add(length)?;
    if end > piece.len() {
        return None;
    }
    Some(piece[begin..end].to_vec())
}

// ---------------------------------------------------------------------------
// Mock-пир с поддержкой BEP 9 (ut_metadata)
// ---------------------------------------------------------------------------

/// Пир, отдающий `info`-словарь через расширенный протокол (BEP 9/BEP 10).
///
/// Отвечает на extended handshake и запросы `ut_metadata`, куски данных не
/// отдаёт: этого достаточно, чтобы движок получил метаданные magnet-ссылки.
pub struct MockMetadataPeer {
    /// Адрес пира.
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl MockMetadataPeer {
    /// Поднимает пира, который отдаст ровно `info` и подтвердит `info_hash`.
    pub async fn start(info: Vec<u8>, info_hash: [u8; 20]) -> MockMetadataPeer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock-пир метаданных: не удалось занять 127.0.0.1:0");
        let addr = listener
            .local_addr()
            .expect("mock-пир метаданных: не удалось узнать локальный адрес");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let info = Arc::new(info);
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
                let info = info.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_metadata_peer(stream, info, info_hash).await {
                        if err.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("mock-пир метаданных: {err}");
                        }
                    }
                });
            }
        });
        MockMetadataPeer { addr, stop, task }
    }
}

impl Drop for MockMetadataPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

async fn serve_metadata_peer(
    stream: TcpStream,
    info: Arc<Vec<u8>>,
    info_hash: [u8; 20],
) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let mut handshake = [0u8; HANDSHAKE_LEN];
    if !read_full(&mut reader, &mut handshake, MOCK_IO_TIMEOUT).await? {
        return Ok(());
    }
    let remote = Handshake::parse(&handshake)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    if remote.info_hash.as_bytes() != &info_hash {
        return Ok(());
    }
    let ours = Handshake::with_extensions(InfoHash::new(info_hash), *b"TorrentaMetaPeer0001");
    write_all_flush(&mut writer, &ours.encode()).await?;

    while let Some(payload) = read_frame(&mut reader, MOCK_IO_TIMEOUT).await? {
        if payload.is_empty() {
            continue;
        }
        let Ok(message) = wire::decode(&payload) else {
            break;
        };
        let Message::Extended { ext_id, payload } = message else {
            // Интерес и запросы кусков игнорируем: пир отдаёт только метаданные.
            continue;
        };
        if ext_id == 0 {
            let mut extensions = BTreeMap::new();
            extensions.insert(b"ut_metadata".to_vec(), Value::int(2));
            extensions.insert(b"v".to_vec(), Value::string("TorrentaMetaPeer 1.0"));
            let mut hello = BTreeMap::new();
            hello.insert(b"m".to_vec(), Value::Dict(extensions));
            hello.insert(
                b"metadata_size".to_vec(),
                Value::int(info.len() as i64),
            );
            let hello = Message::Extended {
                ext_id: 0,
                payload: bencode::encode(&Value::Dict(hello)),
            };
            write_all_flush(&mut writer, &hello.encode()).await?;
            continue;
        }
        if ext_id != 2 {
            continue;
        }
        // Запрос куска метаданных: {"msg_type": 0, "piece": N}.
        let Ok((value, _consumed)) = bencode::decode(&payload) else {
            continue;
        };
        let Some(dict) = value.as_dict() else {
            continue;
        };
        let msg_type = dict.get(b"msg_type".as_slice()).and_then(Value::as_int);
        if msg_type != Some(0) {
            continue;
        }
        let Some(index) = dict
            .get(b"piece".as_slice())
            .and_then(Value::as_int)
            .and_then(|value| usize::try_from(value).ok())
        else {
            continue;
        };
        let start = index * METADATA_PIECE;
        if start >= info.len() {
            continue;
        }
        let end = (start + METADATA_PIECE).min(info.len());
        let mut header = BTreeMap::new();
        header.insert(b"msg_type".to_vec(), Value::int(1));
        header.insert(b"piece".to_vec(), Value::int(index as i64));
        header.insert(b"total_size".to_vec(), Value::int(info.len() as i64));
        let mut body = bencode::encode(&Value::Dict(header));
        body.extend_from_slice(&info[start..end]);
        let response = Message::Extended {
            ext_id: OUR_UT_METADATA,
            payload: body,
        };
        write_all_flush(&mut writer, &response.encode()).await?;
    }
    Ok(())
}

/// Корректный IPv4-адрес для тестов (никогда не уходит в сеть).
pub const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
