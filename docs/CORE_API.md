# Контракт API движка `torrent-core`

Документ фиксирует **точные сигнатуры**, на которые опираются и Rust-ядро, и
Tauri-бэкенд. Менять сигнатуры можно только синхронно правя этот файл.

Крейт: `crates/torrent-core`, имя пакета `torrent-core`, подключение:
`use torrent_core::{...};`

## Карта модулей

| Модуль | Содержимое | Владелец |
|---|---|---|
| `bencode` | `Value`, `decode`, `decode_all`, `encode`, `BencodeError` | protocol |
| `metainfo` | `InfoHash`, `Info`, `FileEntry`, `Metainfo`, `MetainfoError` | protocol |
| `magnet` | `Magnet`, `parse`, `is_magnet`, `MagnetError`, `to_base32/from_base32` | protocol |
| `wire` | `PROTOCOL`, `Handshake`, `Message`, `decode`, `WireError` | protocol |
| `tracker` | `AnnounceEvent`, `AnnounceRequest`, `AnnounceResponse`, `PeerAddr`, `TrackerError`, `build_http_url`, `http_announce`, `udp_announce`, `parse_response`, `parse_compact_peers`, `announce` | protocol |
| `bitfield` | `Bitfield` | lead |
| `peerid` | `generate_peer_id`, `client_name` | lead |
| `storage` | `Storage` | lead |
| `picker` | `PiecePicker`, `PieceStatus` | lead |
| `engine` | `EngineConfig`, `TorrentManager`, `TorrentSnapshot`, ... | lead |

## `bencode`

```rust
pub enum Value { Int(i64), Bytes(Vec<u8>), List(Vec<Value>), Dict(BTreeMap<Vec<u8>, Value>) }
impl Value {
    pub fn int(v: i64) -> Value;
    pub fn bytes(b: impl Into<Vec<u8>>) -> Value;
    pub fn string(s: impl Into<String>) -> Value;
    pub fn as_int(&self) -> Option<i64>;
    pub fn as_bytes(&self) -> Option<&[u8]>;
    pub fn as_str(&self) -> Option<&str>;
    pub fn as_list(&self) -> Option<&[Value]>;
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>>;
    pub fn get(&self, key: &str) -> Option<&Value>;
}
pub fn decode(buf: &[u8]) -> Result<(Value, usize), BencodeError>;
pub fn decode_all(buf: &[u8]) -> Result<Value, BencodeError>;
pub fn encode(value: &Value) -> Vec<u8>;
```

Требования: строгий разбор (ведущие нули, `-0`, пустые целые — ошибка),
защита от переполнения длины, `encode` сортирует ключи словаря по байтам.

## `metainfo`

```rust
pub struct InfoHash(pub [u8; 20]);  // Copy, Eq, Hash, Serialize как hex
impl InfoHash {
    pub fn new(bytes: [u8; 20]) -> Self;
    pub fn from_slice(bytes: &[u8]) -> Option<Self>;
    pub fn as_bytes(&self) -> &[u8; 20];
    pub fn to_hex(&self) -> String;      // 40 символов, нижний регистр
    pub fn from_hex(s: &str) -> Option<Self>;
}
pub struct FileEntry { pub path: String, pub length: u64, pub offset: u64 }
pub struct Info {
    pub name: String, pub piece_length: u32, pub pieces: Vec<InfoHash>,
    pub files: Vec<FileEntry>, pub total_length: u64, pub private: bool,
}
impl Info { pub fn piece_count(&self)->usize; pub fn piece_size(&self,i:usize)->u32; pub fn piece_offset(&self,i:usize)->u64; }
pub struct Metainfo {
    pub announce: Option<String>, pub announce_list: Vec<String>,
    pub comment: Option<String>, pub created_by: Option<String>, pub creation_date: Option<i64>,
    pub info: Info, pub info_hash: InfoHash, pub raw_info: Vec<u8>,
}
impl Metainfo {
    pub fn parse(data: &[u8]) -> Result<Metainfo, MetainfoError>;
    pub fn from_raw_info(raw_info: &[u8], announce_list: Vec<String>, name_hint: Option<&str>) -> Result<Metainfo, MetainfoError>;
    pub fn trackers(&self) -> Vec<String>;
    pub fn total_length(&self) -> u64;
    pub fn piece_count(&self) -> usize;
    pub fn name(&self) -> &str;
}
```

Требования: single-file (`length`) раскладывается в один `FileEntry` с
`path = name`; multi-file (`files`) — путь через `/`, `offset` считается
последовательно; `pieces` обязана делиться на 20; `private = info.private == 1`;
`info_hash = sha1(raw_info)`.

## `magnet`

```rust
pub struct Magnet { pub info_hash: InfoHash, pub display_name: Option<String>, pub trackers: Vec<String> }
pub fn is_magnet(link: &str) -> bool;
pub fn parse(link: &str) -> Result<Magnet, MagnetError>;
```
Поддержка hex (40) и base32 (32) xt, несколько `tr`, `dn`; `+` → пробел в
percent-decoding; мусорные `tr` (без схемы) отбрасывать.

## `wire`

```rust
pub const PROTOCOL: &[u8] = b"BitTorrent protocol";
pub struct Handshake { pub info_hash: InfoHash, pub peer_id: [u8; 20], pub reserved: [u8; 8] }
impl Handshake {
    pub fn new(info_hash: InfoHash, peer_id: [u8; 20]) -> Handshake;
    pub fn with_extensions(info_hash: InfoHash, peer_id: [u8; 20]) -> Handshake;
    pub fn supports_extended(&self) -> bool;
    pub fn encode(&self) -> Vec<u8>;                       // 68 байт
    pub fn parse(buf: &[u8]) -> Result<Handshake, WireError>;
}
pub enum Message {
    KeepAlive, Choke, Unchoke, Interested, NotInterested,
    Have(u32), Bitfield(Bitfield),
    Request { index: u32, begin: u32, length: u32 },
    Piece { index: u32, begin: u32, data: Vec<u8> },
    Cancel { index: u32, begin: u32, length: u32 },
    Port(u16),
    Extended { ext_id: u8, payload: Vec<u8> },
}
impl Message { pub fn encode(&self) -> Vec<u8>; pub fn kind(&self) -> &'static str; }
pub fn decode(payload: &[u8]) -> Result<Message, WireError>;   // payload без длины

pub enum WireError {
    Short,                     // "буфер короче ожидаемого"
    BadProtocol,               // "неверная строка протокола в handshake"
    Malformed(&'static str),   // "некорректное поле сообщения: {0}"
    UnknownId(u8),             // "неизвестный id сообщения: {0}"
}
```

`Message::encode` включает 4-байтовый префикс длины (`KeepAlive` → `00 00 00 00`).
`wire::decode` принимает payload **без** префикса; пустой payload → `KeepAlive`.
`Handshake::parse` допускает лишние байты после 68-го (чтение из потока с буфером).

## `tracker`

```rust
pub enum AnnounceEvent { Started, Completed, Stopped }
impl AnnounceEvent { pub fn as_str(self)->&'static str; pub fn as_udp_code(self)->u32; }
pub struct AnnounceRequest {
    pub info_hash: InfoHash, pub peer_id: [u8; 20], pub port: u16,
    pub uploaded: u64, pub downloaded: u64, pub left: u64,
    pub event: Option<AnnounceEvent>, pub num_want: Option<u32>, pub key: Option<u32>,
}
pub struct PeerAddr { pub ip: IpAddr, pub port: u16 }   // Display = "ip:port"
pub struct AnnounceResponse {
    pub interval: u64, pub min_interval: Option<u64>, pub peers: Vec<PeerAddr>,
    pub seeds: Option<u32>, pub leechers: Option<u32>, pub warning: Option<String>,
}
pub fn build_http_url(base: &str, req: &AnnounceRequest) -> Result<String, TrackerError>;
pub fn parse_response(bytes: &[u8]) -> Result<AnnounceResponse, TrackerError>;
pub fn parse_compact_peers(data: &[u8]) -> Result<Vec<PeerAddr>, TrackerError>;
pub async fn http_announce(client: &reqwest::Client, base: &str, req: &AnnounceRequest, timeout: Duration) -> Result<AnnounceResponse, TrackerError>;
pub async fn udp_announce(base: &str, req: &AnnounceRequest, timeout: Duration) -> Result<AnnounceResponse, TrackerError>;
pub async fn announce(client: &reqwest::Client, base: &str, req: &AnnounceRequest, timeout: Duration) -> Result<AnnounceResponse, TrackerError>;
```

```rust
pub enum TrackerError {
    Unsupported(String),   // "неподдерживаемый трекер: {0}"
    Network(String),       // "сетевая ошибка: {0}"
    Parse(String),         // "ошибка разбора ответа трекера: {0}"
    Failure(String),       // "трекер вернул ошибку: {0}"
}
```

Критично: `info_hash` и `peer_id` в HTTP URL кодируются как **сырые байты**
(все не-alnum → `%XX`), а не как UTF-8 строка. UDP: connect (action 0) →
announce (action 1), transaction id случайный, таймаут и один повтор.

Поведение, зафиксированное тестами:
- `key` в HTTP — 8 hex-цифр (`format!("{:08x}")`), в UDP — сырой `u32`;
- `numwant` по умолчанию 50 (для UDP — `-1`, «сколько есть»);
- compact-пиры с `port == 0` или `0.0.0.0`/`::` отбрасываются как бесполезные;
- `peers6` (18 байт на пир) разбирается, хотя в старом BEP не упоминался;
- хвост UDP-ответа, не кратный 6, молча обрезается;
- `failure reason` важнее кривого тела; хвостовые `\n`/пробелы у HTTP-ответа терпим.

## `engine` (контракт для Tauri-бэкенда)

```rust
pub struct EngineConfig {
    pub download_dir: PathBuf,
    pub max_peers_per_torrent: usize,   // по умолчанию 50
    pub download_limit_kib: u32,        // 0 = без лимита
    pub upload_limit_kib: u32,          // 0 = без лимита
    pub listen_port: u16,               // 0 = свободный порт
    pub seed_after_complete: bool,      // по умолчанию true
}
impl Default for EngineConfig { ... }

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TorrentState { Queued, Metadata, Downloading, Seeding, Paused, Completed, Error }

#[derive(Serialize, Clone)] pub struct FileView { pub path: String, pub length: u64, pub downloaded: u64 }
#[derive(Serialize, Clone)] pub struct PeerView { pub addr: String, pub client: String, pub down_rate: f64, pub up_rate: f64, pub progress: f64, pub is_seed: bool }
#[derive(Serialize, Clone)] pub struct TorrentSnapshot {
    pub id: u64,
    pub name: String,
    pub info_hash: String,          // hex
    pub state: TorrentState,
    pub error: Option<String>,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,      // за сессию (полезные данные)
    pub uploaded_bytes: u64,
    pub progress: f64,              // 0.0..=1.0
    pub piece_count: u32,
    pub piece_length: u32,
    pub pieces_have: u32,
    pub peers_connected: u32,
    pub peers_known: u32,
    pub down_rate: f64,             // байт/с
    pub up_rate: f64,
    pub eta_seconds: Option<u64>,
    pub save_path: String,
    pub added_at: i64,              // unix seconds
    pub files: Vec<FileView>,
    pub trackers: Vec<String>,
    pub peers: Vec<PeerView>,
}
#[derive(Serialize, Clone, Default)] pub struct GlobalTotals {
    pub torrents: u32, pub active: u32,
    pub down_rate: f64, pub up_rate: f64,
    pub downloaded_bytes: u64, pub uploaded_bytes: u64,
}
#[derive(Debug, Clone)] pub enum AddSource { File(Vec<u8>), Magnet(String) }
#[derive(Debug, thiserror::Error)] pub enum EngineError {
    #[error("некорректный .torrent: {0}")] Metainfo(String),
    #[error("некорректная magnet-ссылка: {0}")] Magnet(String),
    #[error("ошибка ввода-вывода: {0}")] Io(String),
    #[error("торрент {0} не найден")] NotFound(u64),
    #[error("торрент уже добавлен: {0}")] Duplicate(String),
}

pub struct TorrentManager { /* private */ }
impl TorrentManager {
    pub fn new(config: EngineConfig) -> Arc<TorrentManager>;
    pub fn config(&self) -> EngineConfig;
    pub fn set_config(&self, config: EngineConfig);
    pub fn listen_port(&self) -> u16;
    pub async fn add_file(&self, data: Vec<u8>, save_path: Option<PathBuf>) -> Result<u64, EngineError>;
    pub async fn add_magnet(&self, link: &str, save_path: Option<PathBuf>) -> Result<u64, EngineError>;
    pub fn pause(&self, id: u64) -> Result<(), EngineError>;
    pub fn resume(&self, id: u64) -> Result<(), EngineError>;
    pub fn pause_all(&self);
    pub fn resume_all(&self);
    pub async fn remove(&self, id: u64, delete_files: bool) -> Result<(), EngineError>;
    pub fn snapshot(&self) -> Vec<TorrentSnapshot>;
    pub fn totals(&self) -> GlobalTotals;
    pub async fn shutdown(&self);
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent>;
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineEvent {
    Added { id: u64, name: String },
    StateChanged { id: u64, state: TorrentState },
    Completed { id: u64, name: String },
    Error { id: u64, message: String },
    Removed { id: u64 },
}
```

Поведение:

* `TorrentManager::new` вызывается уже внутри tokio-рантайма Tauri
  (`tauri::async_runtime`) — создаёт HTTP-клиент, слушающий TCP-сокет и тикер
  статистики (обновляет `down_rate`/`up_rate`/ETA каждые 500 мс).
* `snapshot()` — синхронный, дешёвый: только атомики и короткие мьютексы.
* `downloaded_bytes` в снапшоте и `GlobalTotals` учитывает только **проверенные
  SHA-1 куски**, поэтому у завершённого торрента он равен `total_bytes`
  (перекачки из-за повторных запросов не раздувают счётчик).
* `pause` останавливает сетевые задачи, состояние → `Paused`; `resume` перезапускает
  announce и подключение к пирам. Дополнительно `resume` возвращает в очередь
  подключения уже известные адреса (`Runtime::requeue_peers`), поэтому загрузка
  возобновляется сразу, не дожидаясь следующего объявления трекера (30–1800 с).
* Испорченный кусок (несовпадение SHA-1) не блокирует торрент: он освобождается
  и откладывается на `1500 мс × число неудач` (не более 10 с), а `next_requests`
  выбирает другие куски. Когда отложенных не осталось, берутся и «остывающие» —
  торрент из одного куска не замирает.
* Отказ трекера (`failure reason`) и повторные сетевые ошибки объявления переводят
  торрент в `Error` с русским текстом в `error`/`EngineEvent::Error`, но только
  пока нет ни скачанных байт, ни подключённых пиров; первое успешное объявление
  снимает ошибку и возвращает `Downloading`/`Completed`/`Seeding`.
* `remove(delete_files = true)` удаляет файлы торрента внутри `save_path`.
* magnet: состояние `Metadata`, пока идёт обмен метаданными по BEP 9/10;
  после проверки SHA-1 → обычная загрузка.
* `shutdown()` останавливает задачи, сбрасывает буферы на диск и освобождает
  слушающий сокет: цикл `accept` проверяет флаг остановки раз в 100 мс, поэтому
  после `shutdown()` (ожидание ~250 мс) порт можно занимать снова — например,
  новым `TorrentManager` в этом же процессе (используется в тестах).
