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
| `storage` | `Storage`, `release_writes`/`acquire_writes`/`writes_released`, общий режим доступа к файлам | lead |
| `picker` | `PiecePicker`, `PieceStatus`, `set_levels`/`level`/`reset_have` | lead |
| `selection` | `Selection`, `wanted_pieces`, `wanted_downloaded`, `effective_limit`, `piece_levels`/`level_of` (crate-private) | core |
| `dht` | `Dht`, `DhtConfig`, `DhtStats`, `DhtError`, `DEFAULT_BOOTSTRAP`, `announce`/`take_peers` (BEP 5) | core |
| `pex` | `PexMessage`, `EXT_PEX`, `encode`, `decode`, `diff` (BEP 11) | core |
| `port_map` | `MappingProtocol`, `PortMapping`, `PortMapError`, `map_port`, `unmap_port` (UPnP-IGD, NAT-PMP) | core |
| `proxy` | `ProxyKind`, `ProxyConfig`, `ProxyError`, `parse`, `connect` (SOCKS5 / HTTP CONNECT) | core |
| `web_seed` | `WebSeed`, `WebSeedConfig`, `WebSeedError`, `parse_url_list` (BEP 19) | core |
| `engine` | `EngineConfig`, `TorrentManager`, `TorrentSnapshot`, `AddOptions`, `TorrentPreview`, `FilePriority`, `FileView`, `PlanKind`/`plan` (crate-private) | core |

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
    pub url_list: Vec<String>,      // 1.6: `url-list` (BEP 19), только http(s)
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

`url_list` — ключ `url-list`: строка или список строк, каждая обрезается,
остаются только `http(s)` с хостом, дубликаты выбрасываются с сохранением
первого вхождения (разбор — `web_seed::parse_url_list`); отсутствие ключа —
пустой вектор.

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

## `dht`

```rust
pub const DEFAULT_BOOTSTRAP: &[&str];   // router.bittorrent.com:6881,
                                        // dht.transmissionbt.com:6881,
                                        // router.utorrent.com:6881
pub struct DhtConfig {
    pub port: u16,               // 0 = свободный UDP-порт
    pub bootstrap: Vec<String>,  // пусто = DEFAULT_BOOTSTRAP
    pub max_nodes: usize,        // по умолчанию 1024
}
impl Default for DhtConfig { ... }
pub struct DhtStats { pub running: bool, pub nodes: usize, pub peer_cache: usize, pub queries: u64, pub responses: u64 }
pub enum DhtError { Closed, Io, Timeout, Protocol(String), Remote(String) }
pub struct Dht { /* private */ }
impl Dht {
    pub fn new(config: DhtConfig) -> Self;                            // сеть не трогается
    pub fn node_id(&self) -> [u8; 20];
    pub async fn start(self: &Arc<Self>) -> std::io::Result<u16>;     // фактический порт
    pub async fn announce(&self, info_hash: InfoHash, port: u16) -> Result<(), DhtError>;
    pub fn take_peers(&self, info_hash: &InfoHash) -> Vec<SocketAddr>; // опустошает кэш
    pub fn stats(&self) -> DhtStats;
    pub async fn shutdown(&self);
}
```

Жизненный цикл: `new` → `start` → `announce`/`take_peers` → `shutdown`. `start`
идемпотентен (повторный вызов возвращает тот же порт), `shutdown` тоже; после
остановки `announce` возвращает `DhtError::Closed`, `port == 0` — `Protocol`,
пустая таблица маршрутизации или молчание всех узлов — `Timeout`. Паники на
сетевых сбоях нет: если bootstrap не ответил, узел живёт с пустой таблицей.
`take_peers` забирает накопленные адреса и чистит кэш по этому `info_hash`.

Ограничения (сознательные, чтобы модуль не выглядел полным BEP 5):

* один раунд `get_peers` по ближайшим известным узлам — без итеративного обхода
  `nodes`;
* `announce_peer` уходит fire-and-forget, без ожидания полного раунда;
* токены подписываются статическим секретом;
* таблица IPv4-only: компактные `nodes` — только IPv4, IPv6-пиры из `values`
  разбираются, но новых узлов из них не появляется;
* BEP 42 (secure node id) не поддерживается, защиты от Sybil нет;
* сообщения `port` (BEP 5 relay) не обрабатываются.

## `pex`

```rust
pub const EXT_PEX: u8 = 1;    // локальный id ut_pex; лимит — 50 адресов на поле
pub struct PexMessage { pub added: Vec<SocketAddr>, pub dropped: Vec<SocketAddr> }
impl PexMessage {
    pub fn encode(&self) -> Vec<u8>;
    pub fn decode(payload: &[u8]) -> Result<Self, PexError>;
    pub fn diff(known: &[SocketAddr], fresh: &[SocketAddr], dropped: &[SocketAddr]) -> PexMessage;
}
pub enum PexError {
    Bencode(String), NotDict, Malformed(&'static str), Truncated(&'static str),
    TooMany { field: &'static str, count: usize, limit: usize },
}
```

Адреса — compact по 6 байт (IPv4). `encode` не пишет пустые поля (пустое
сообщение — `de`), IPv6 молча отбрасывает, `added.f` заполняет нулями. `decode`
читает `added`/`dropped`, поля `added6`/`dropped6` игнорирует (IPv6-пиры из PEX
не попадают); больше 50 адресов в поле — `TooMany`.

`diff(known, fresh, dropped)` — что отправить соседу: дедупликация, не больше 50
адресов на поле, отбрасываются порт 0, `0.0.0.0`, широковещательные и
multicast-адреса, IPv6; адрес, попавший и в `fresh`, и в `dropped`, считается
живым (уходит только в `added`). Loopback и приватные адреса сохраняются: в
локальной сети они рабочие.

## `web_seed`

```rust
pub struct WebSeedConfig { pub connect_timeout: Duration, pub read_timeout: Duration, pub max_parallel: usize }
impl Default for WebSeedConfig { ... }   // 10 с / 30 с / 4
pub struct WebSeed { /* private */ }
impl WebSeed {
    pub fn new(url: &str, proxy: Option<&str>, config: WebSeedConfig) -> Result<Self, WebSeedError>;
    pub fn url(&self) -> &str;
    pub async fn fetch_range(&self, offset: u64, len: u32) -> Result<Vec<u8>, WebSeedError>;
}
pub fn parse_url_list(value: &bencode::Value) -> Vec<String>;   // значение `url-list`
pub enum WebSeedError {
    BadUrl(String), Http(String), Status(u16), ShortBody,
    RangeMismatch { want: u64, got: u64 }, BadRange { offset: u64, len: u32 }, Timeout,
}
```

`new` проверяет схему и хост (`http`/`https`), собирает клиент с `.no_proxy()`
(прокси передаётся аргументом), не более 5 редиректов.

`fetch_range` запрашивает `Range: bytes=<offset>-<offset+len-1>`:

* `206` — начало `Content-Range` сверяется с `offset` (расхождение —
  `RangeMismatch`), отсутствующий заголовок означает «доверяем запросу», тело
  короче `len` — `ShortBody`;
* `200` — тело читается потоком с пропуском `offset` (буфер 64 КиБ,
  `read_timeout` на каждое чтение), короткое тело — `ShortBody`;
* `416` и любой другой статус — `Status(code)`, отдельной ветки для 416 нет;
* `len == 0` — пустой `Vec` без запроса; нулевой/некорректный диапазон —
  `BadRange`.

`parse_url_list` принимает строку или список: обрезает, оставляет только
`http(s)` с хостом, выбрасывает нестроковые элементы и дубликаты (порядок первого
вхождения сохраняется).

## `proxy`

```rust
pub enum ProxyKind { Socks5, Http }
impl ProxyKind { pub fn as_scheme(self) -> &'static str }   // "socks5" | "http"
pub struct ProxyConfig {
    pub kind: ProxyKind, pub host: String, pub port: u16,
    pub username: Option<String>, pub password: Option<String>,
}
impl ProxyConfig {
    pub fn parse(spec: &str) -> Result<Self, ProxyError>;
    pub fn to_url(&self) -> String;
}
pub enum ProxyError { BadScheme(String), BadHost(String), BadPort(String) }
pub async fn connect(config: &ProxyConfig, target: SocketAddr) -> std::io::Result<TcpStream>;
```

`parse`: `socks5://` и `socks5h://` → `Socks5`, `http://` и `https://` → `Http`;
порт по умолчанию 1080/8080; пустая спецификация — `BadScheme`, без хоста —
`BadHost`, порт 0 — `BadPort`; путь и query игнорируются, пароль без логина
отбрасывается. `to_url` percent-кодирует userinfo и всегда пишет схему из
`ProxyKind`, поэтому `https://`-спецификация сериализуется как `http://`.

`connect` — TCP до прокси (12 с) и рукопожатие (15 с), затем `set_nodelay`:

* SOCKS5 (RFC 1928/1929): авторизация по логину/паролю предлагается только если
  логин задан; ответы 1–8 отображаются в `Other`/`PermissionDenied`/
  `NotConnected`/`AddrNotAvailable`/`ConnectionRefused`/`TimedOut`/`Unsupported`;
* HTTP CONNECT: `CONNECT host:port HTTP/1.1`, `Host`, при логине и пароле —
  `Proxy-Authorization: Basic`, `User-Agent: Torrenta`, `Proxy-Connection: Keep-Alive`;
  не-`200` — `PermissionDenied` только для 407, остальные — `Other`.

Ограничения: только TCP `CONNECT` (BIND и UDP ASSOCIATE не нужны — DHT и
UDP-трекеры идут напрямую); цель обязана быть уже разрешённым `SocketAddr`
(ATYP=3 не используется); `socks5h` трактуется как `socks5` — DNS разрешается
локально; для HTTP — только Basic-авторизация.

## `port_map`

```rust
pub enum MappingProtocol { Tcp, Udp }
impl MappingProtocol { pub fn as_str(self) -> &'static str }   // "TCP" | "UDP"
pub struct PortMapping { pub gateway: IpAddr, pub external_port: u16, pub protocol: MappingProtocol, pub lifetime: Duration }
pub enum PortMapError { Network(String), Parse(String), Soap { code: String, message: String } }
pub async fn map_port(internal_port: u16, protocol: MappingProtocol, lifetime_secs: u32) -> Option<PortMapping>;
pub async fn unmap_port(mapping: &PortMapping);
```

`map_port(0, ...)` → `None`; `lifetime_secs` зажимается в 60…604 800. Порядок
попыток — сначала UPnP-IGD (SSDP-поиск → описание устройства → `AddPortMapping`),
затем NAT-PMP (RFC 6886); внешний порт равен внутреннему. Ошибки наружу не
выходят: не получилось — вернулся `None`, движок продолжает без проброса.
`unmap_port` пробует оба протокола и ошибки игнорирует (каждый вызов под
таймаутом 6 с).

Ограничения: только IPv4; шлюз для NAT-PMP ищется эвристикой «локальный адрес с
последним октетом 1» (чтение таблицы маршрутов потребовало бы новых
зависимостей), поэтому в нестандартной сети NAT-PMP может не найтись, и UPnP
остаётся запасным путём.

## `engine` (контракт для Tauri-бэкенда)

```rust
pub struct EngineConfig {
    pub download_dir: PathBuf,
    pub max_peers_per_torrent: usize,   // по умолчанию 50
    pub download_limit_kib: u32,        // 0 = без лимита
    pub upload_limit_kib: u32,          // 0 = без лимита
    pub listen_port: u16,               // 0 = свободный порт
    pub seed_after_complete: bool,      // по умолчанию true
    pub max_active_downloads: usize,    // 1.3: 0 = без ограничения
    pub max_active_seeds: usize,        // 1.3: 0 = без ограничения
    // 1.6
    pub dht: bool,                      // по умолчанию true: искать пиров через DHT (BEP 5)
    pub dht_port: u16,                  // 0 = свободный UDP-порт узла DHT
    pub pex: bool,                      // по умолчанию true: обмен пирами (BEP 11)
    pub web_seeds: bool,                // по умолчанию true: качать с веб-сидов (BEP 19)
    pub proxy: Option<String>,          // None = напрямую; например "socks5://user:pass@host:port"
    pub port_mapping: bool,             // по умолчанию true: проброс порта (best effort)
    pub seed_ratio_limit: f64,          // 0.0 = без ограничения
    pub seed_time_limit_min: u32,       // 0 = без ограничения
}
impl Default for EngineConfig { ... }

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TorrentState { Queued, Metadata, Checking, Downloading, Seeding, Paused, Completed, Error }

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FilePriority { Skip, Low, #[default] Normal, High }

#[derive(Serialize, Clone)] pub struct FileView { pub path: String, pub length: u64, pub downloaded: u64, pub selected: bool, pub priority: FilePriority }
#[derive(Serialize, Clone)] pub struct PeerView { pub addr: String, pub client: String, pub down_rate: f64, pub up_rate: f64, pub progress: f64, pub is_seed: bool }
#[derive(Serialize, Clone)] pub struct TorrentSnapshot {
    pub id: u64,
    pub name: String,
    pub info_hash: String,          // hex
    pub state: TorrentState,
    pub error: Option<String>,
    pub total_bytes: u64,           // полный размер торрента
    pub wanted_bytes: u64,          // объём выбранных файлов
    pub downloaded_bytes: u64,      // за сессию (полезные данные)
    pub uploaded_bytes: u64,
    pub progress: f64,              // 0.0..=1.0 по выбранным файлам
    pub piece_count: u32,
    pub piece_length: u32,
    pub pieces_have: u32,           // по всему торренту
    pub peers_connected: u32,
    pub peers_known: u32,
    pub down_rate: f64,             // байт/с
    pub up_rate: f64,
    pub eta_seconds: Option<u64>,
    pub seed_ratio: f64,            // 1.6: отдано/скачано; 0.0, пока скачано 0
    pub seed_limit_reached: bool,   // 1.6: лимит раздачи сработал, ждём ручного resume
    pub down_limit_kib: u32,        // действующий лимит: min(глобальный, торрентный)
    pub up_limit_kib: u32,
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
/// Настройки добавления: папка и индексы выбранных файлов (`None` = все).
#[derive(Debug, Clone, Default)]
pub struct AddOptions { pub save_path: Option<PathBuf>, pub files: Option<Vec<usize>> }
/// Файл в предпросмотре торрента до его добавления.
#[derive(Debug, Clone, Serialize)] pub struct PreviewFile { pub index: usize, pub path: String, pub length: u64 }
#[derive(Debug, Clone, Serialize)] pub struct TorrentPreview {
    pub name: String,
    pub info_hash: String,          // hex
    pub total_bytes: u64,
    pub piece_length: u32,
    pub piece_count: u32,
    pub files: Vec<PreviewFile>,
    pub trackers: Vec<String>,
}
#[derive(Debug, thiserror::Error)] pub enum EngineError {
    #[error("некорректный .torrent: {0}")] Metainfo(String),
    #[error("некорректная magnet-ссылка: {0}")] Magnet(String),
    #[error("ошибка ввода-вывода: {0}")] Io(String),
    #[error("торрент {0} не найден")] NotFound(u64),
    #[error("торрент уже добавлен: {0}")] Duplicate(String),
    #[error("не выбран ни один файл")] Selection,
    #[error("торрент ещё не готов к перепроверке")] NotReady,
}

pub struct TorrentManager { /* private */ }
impl TorrentManager {
    pub fn new(config: EngineConfig) -> Arc<TorrentManager>;
    pub fn config(&self) -> EngineConfig;
    pub fn set_config(&self, config: EngineConfig);
    pub fn listen_port(&self) -> u16;
    pub fn preview_file(data: &[u8]) -> Result<TorrentPreview, EngineError>;
    pub async fn preview_magnet(&self, link: &str, timeout: Duration) -> Result<TorrentPreview, EngineError>;
    pub async fn add_file(&self, data: Vec<u8>, save_path: Option<PathBuf>) -> Result<u64, EngineError>;
    pub async fn add_file_with(&self, data: Vec<u8>, options: AddOptions) -> Result<u64, EngineError>;
    pub async fn add_magnet(&self, link: &str, save_path: Option<PathBuf>) -> Result<u64, EngineError>;
    pub async fn add_magnet_with(&self, link: &str, options: AddOptions) -> Result<u64, EngineError>;
    pub fn set_file_selection(&self, id: u64, files: Vec<usize>) -> Result<(), EngineError>;
    pub fn set_file_priorities(&self, id: u64, priorities: Vec<FilePriority>) -> Result<(), EngineError>;
    pub fn recheck(&self, id: u64) -> Result<(), EngineError>;
    pub fn set_limits(&self, id: u64, down_kib: u32, up_kib: u32) -> Result<(), EngineError>;
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

**Настройки и `sanitized()`.** `TorrentManager::new` и `set_config` прогоняют
`EngineConfig::sanitized()`: пустой `download_dir` заменяется каталогом по
умолчанию, `max_peers_per_torrent` зажимается в 1…200, `max_active_*` — не больше
`MAX_ACTIVE_LIMIT = 500`; `proxy` обрезается от пробелов, пустая строка или
неразбираемая спецификация превращаются в `None` (движок молча работает
напрямую); нечисловой (`NaN`, ∞) или отрицательный `seed_ratio_limit` становится
`0.0`. В `EngineConfig` включён `#[serde(default)]`, поэтому старые настройки
Tauri без новых полей читаются без миграции.

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
  Ручная пауза запоминается (`flags.user_paused`): такой торрент очередь не
  запускает, а `resume` лишь снимает флаг — окончательное решение снова за
  `schedule`, поэтому при заполненном лимите он может уйти в `Queued`.
* Ограничения `max_active_downloads`/`max_active_seeds` применяются внутри
  движка: `schedule` вызывается после каждого изменения и раз в тик, лишние
  торренты паркуются в `Queued`, освободившийся слот достаётся самому раннему по
  `added_at`/`id`. Загрузки и раздачи считаются отдельно, `0` — без ограничения.
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

## Выборочная загрузка, лимиты и предпросмотр (1.1)

**Выбор файлов.** `AddOptions.files` — индексы файлов в порядке `Metainfo`;
`None` или пустой список означает «все файлы». Индексы вне диапазона и пустой
итоговый выбор отклоняются `EngineError::Selection`. `set_file_selection(id,
files)` меняет выбор на лету:

* «нужный» кусок — тот, что пересекается хотя бы с одним выбранным файлом
  (`selection::wanted_pieces`); планировщик выбирает только нужные куски;
* снятые куски, которые ещё не скачаны, немедленно освобождаются: буферы
  выбрасываются, запросы у пиров снимаются, куски возвращаются в раздачу;
* уже скачанные данные не удаляются и остаются «на диске»; после расширения
  выбора они не перекачиваются (кусок считается готовым по битфилду);
* `progress`, `eta_seconds` и `wanted_bytes` считаются по выбранным файлам;
  `total_bytes` остаётся полным размером торрента, `pieces_have`/`pieces_total` —
  по всему торренту. `FileView.downloaded` пока всегда `0`: прогресс отдаётся
  только суммарно по торренту, отдельного учёта скачанного по файлам нет;
* завершение наступает, когда скачаны все **нужные** куски: состояние
  `Seeding`/`Completed`, даже если `pieces_have < piece_count`;
* `Selection` и его хелперы (`wanted_pieces`, `wanted_downloaded`,
  `effective_limit`) — crate-private, снаружи виден только результат через
  `TorrentSnapshot`/`FileView`.

**Объявление трекеру.** В `left` уходит остаток только по выбранным файлам
(`Runtime::wanted_left()`), в `downloaded` — фактические байты. Если завершение
произошло из-за сужения выбора, цикл трекера будится сразу (`wake_announcer`) и
отправляет `event=completed`, не дожидаясь интервала объявления. Побудка — флаг
`announce_pending`, который цикл проверяет не реже раза в 500 мс
(`wait_wake`): в отличие от `Notify::notify_waiters()` сигнал не теряется, если
в этот момент ответ трекера ещё в пути (до 25 с). Побудка ставится и при смене
выбора на уже готовом торренте, поэтому новый `left` уходит сразу.

**Лимиты.** `set_limits(id, down_kib, up_kib)` задаёт лимит торрента, `0` — без
своего потолка. Действующая скорость = `min(глобальный лимит, лимит торрента)`
(`selection::effective_limit`), это же значение отдаётся в
`snapshot.down_limit_kib`/`up_limit_kib`. На сессию существует один общий
лимитер (`SharedLimiters` в `Inner`), поэтому N торрентов при глобальных
1 МиБ/с вместе дают не больше 1 МиБ/с, а не N × 1 МиБ/с. Изменения настроек
подхватываются на следующем запросе без перезапуска.

**Предпросмотр.** `preview_file(&[u8])` разбирает `.torrent` без добавления;
`preview_magnet(link, timeout)` идёт за метаданными по BEP 9 с ограничением по
времени: до 5 трекеров по очереди, не более 64 кандидатов, проверка шанса
совпадения `info_hash`; ручка в менеджере не регистрируется и фоновых задач не
остаётся. Ошибка по таймауту: «не удалось получить метаданные за отведённое
время». Ограничение именно предпросмотра: `preview_magnet` не использует DHT и
PEX — метаданные ищутся только у пиров с трекеров,
а у magnet-ссылки без списка файлов `add_magnet_with` получает полный выбор.

## Приоритеты, очередь и перепроверка (1.3)

**Приоритеты файлов.** `FilePriority` (`Skip`, `Low`, `Normal`, `High`) — сериализуется
в нижнем регистре (`"skip"`, `"low"`, `"normal"`, `"high"`), по умолчанию `Normal`.
`set_file_priorities(id, priorities)` принимает вектор **длиной ровно в число файлов**
торрента (иначе `EngineError::Selection`, неизвестный id — `NotFound`) и меняет
набор нужных кусков вместе с порядком их выдачи:

* `Skip` — файл не скачивается (то же, что снять выбор); `selected` в `FileView`
  всегда согласован: `priority != Skip`;
* `Low`/`Normal`/`High` превращаются в уровни кусков `0/1/2`
  (`selection::level_of`), общий кусок нескольких файлов получает **максимальный**
  уровень; `picker` перебирает уровни от старшего к младшему, внутри уровня
  сохраняется rarest-first;
* приоритет меняет только порядок загрузки, а не скорость: лимиты и общий пул
  аренд прежние, скачанное не перекачивается и не удаляется;
* смена приоритетов пересчитывает `wanted_bytes`/`progress`/`eta` и отправляет
  трекеру свежий `left` так же, как смена выбора;
* для magnet до получения метаданных приоритеты откладываются
  (`pending_priorities`) и применяются автоматически, когда `Metainfo` готов;
* `AddOptions` приоритеты не принимает: при добавлении задаётся только выбор
  файлов, приоритеты — отдельным вызовом после добавления.

**Очередь и лимиты активности.** `EngineConfig.max_active_downloads` и
`max_active_seeds` (`0` — без ограничения, значения ограничены сверху
`MAX_ACTIVE_LIMIT = 500`, в настройках Tauri — 100). `schedule` строит план
(`PlanKind`: `Download`/`Seed`/`Idle`) по состояниям и лимитам, считая загрузки и
раздачи отдельно:

* кандидаты сортируются по `added_at`, затем `id`; активные получают слот,
  остальные паркуются — состояние `Queued`, пауза движка, файлы только на чтение;
* `Idle` (ручная пауза, `Checking`, `Error`, удаление, завершённый при выключенной
  раздаче) слот не занимает и в очередь не ставится;
* завершившийся торрент переезжает из слота загрузки в слот раздачи; освободившееся
  место тут же достаётся следующему, потому что `schedule` вызывается и на тике;
* `recheck` слот не занимает: пока идёт проверка, место может занять другой торрент.

**Перепроверка.** `recheck(id)`:

* magnet без метаданных → `EngineError::NotReady`; повторный вызов во время
  проверки — идемпотентный `Ok(())`;
* состояние → `Checking`, `pieces_have` обнуляется и растёт по мере проверки,
  `eta_seconds = 0`, обмен останавливается, запись на диск отпускается;
* `verify_all` читает каждый кусок из `Storage`, считает SHA-1 и сравнивает с
  эталоном, восстанавливая битовую карту; при остановке менеджера проверка
  прерывается;
* итоговое состояние — `Seeding`/`Completed`, `Downloading` (если нужны ещё куски)
  или `Paused`, если пользователь поставил паузу до проверки;
* если торрент продолжает качать, запись забирается обратно (`acquire_writes`);
* при завершении ставится побудка `wake_announcer` с `event=completed`; пиры не
  переподключаются, известные адреса сохраняются.

**Доступ к файлам (Windows).** `Storage` открывает файлы с общим доступом
`read | write | delete`, поэтому дескриптор торрента не блокирует запуск `.exe`.
Запись держится только пока идёт загрузка; на паузе, в `Queued`, после завершения
и во время проверки файлы переоткрываются только на чтение
(`release_writes`), а перед записью — возвращаются в режим записи
(`acquire_writes`). Запись в файл после `release_writes` возвращает
`PermissionDenied` — это ожидаемое поведение, а не сбой. Проверки при добавлении
торрента по-прежнему нет: диск не хэшируется, пока пользователь явно не вызовет
`recheck`.

## DHT, PEX, веб-сиды, прокси, проброс и лимиты раздачи (1.6)

**DHT.** Узел один на сессию (`Inner.dht`): поднимается и гасится вместе с
настройкой `dht` при старте, в `set_config` и в `shutdown` (`refresh_dht`). Порт —
`dht_port`, `0` = свободный; если UDP-порт занят, узел просто не поднимается, а
торренты продолжают работать через трекеры и PEX. Каждый торрент крутит
`Runtime::dht_loop`: раз в 30 с (`DHT_POLL`) забирает `take_peers` и отдаёт
адреса кондуктору; `announce` уходит сразу после старта и затем раз в 300 с
(`DHT_ANNOUNCE`) отдельной задачей, чтобы сетевой раунд не задерживал опрос уже
известных пиров. При `listen_port == 0` анонс не отправляется (входящих
соединений нет), но цикл продолжает искать пиров в кэш.

**PEX.** `our_extended_handshake` объявляет `ut_metadata: 1` и `ut_pex: 2`;
входящий extended-`0` запоминает `ut_pex` пира, после чего каждое сообщение
`ut_pex` разбирается в `PexMessage`, а его адреса становятся кандидатами.
Исходящий дифф строится раз в 60 с (`PEX_INTERVAL`), не больше 50 адресов
(`PEX_MAX_ADDRESSES`), и отправляется только тем, кто объявил `ut_pex`. Номер в
сообщении берётся из handshake пира, а `pex::EXT_PEX = 1` — внутреннее соглашение
модуля, на провод оно не влияет.

**Веб-сиды.** `web_seed_loop` запускается, только если `Metainfo.url_list`
непуст, и качает по одному куску за раз: занимает кусок у общего `picker`
(`Bitfield::all_set`), берёт окно через `piece_window`, качает `fetch_range` и
сдаёт `complete_piece`. Пересечься с пирами куски не могут — планировщик один.
Перед попыткой списывается бюджет общего лимитера (`grant_download` на
`piece_blocks`), пустое ведро — пауза 5 с. Отказ или короткое тело: `picker.release`, ротация на следующий веб-сид и пауза
5 с (`WEB_SEED_RETRY`); несовпадение SHA-1 откладывает сам планировщик, сид тоже
меняется. На один запрос отводится 90 с (`WEB_SEED_DEADLINE`). Выключение
`web_seeds` цикл не убивает — он спит по 5 с и проверяет настройку снова; смена
`proxy` пересобирает клиентов на ходу. Бюджет списывается куском целиком
(`piece_length`), поэтому при очень узком лимите возможно превышение на один
кусок. `WebSeedConfig` берётся по умолчанию (10 с / 30 с / 4); если все URL в
`url-list` битые, клиентов не остаётся и цикл выходит, как и у докачанного
торрента.

**Прокси.** При заданном `proxy` TCP-соединения с пирами идут через
`proxy::connect`, а HTTP-клиент сессии (`HttpClient` в `Inner`, `build_http`)
собирается с тем же прокси — через него работают HTTP-трекеры, веб-сиды и
запросы за метаданными magnet. `set_config` пересоздаёт клиент сразу, не
дожидаясь перезапуска торрентов. UDP-трекеры и DHT прокси не используют:
UDP-туннеля нет, это ограничение протокола, а не недоделка.

**Проброс портов.** При старте и в `set_config` поднимается TCP-проброс
`listen_port` на 3600 с (`PORT_MAP_LIFETIME_SECS`); при `shutdown` и при
выключении `port_mapping` проброс снимается. Порт `0` не пробрасывается. UDP-порт
DHT не пробрасывается: входящие соединения пиров приходят только по TCP, а DHT
работает исходящими запросами. Всё best effort — ошибок и событий в UI нет,
неудача ничего не ломает (UPnP → NAT-PMP).

**Лимиты раздачи.** `seed_ratio_limit` (отдано/скачано) и `seed_time_limit_min`
(минуты в состоянии `Seeding`) проверяются на тике (`tick_seed_limit`).
`seed_ratio` в снапшоте — `uploaded / downloaded`, `0.0`, пока ничего не скачано;
порог срабатывает включительно (`>=`), а время считается только пока торрент
действительно раздаёт (`Seeding`). Лимит сработал → `seed_limit_reached = true`,
`plan_kind()` возвращает `Idle`: слот раздачи освобождается, торрент стоит до
ручного `resume`, который обнуляет счётчик (`SeedMeter::reset`) и начинает отрезок
заново. `0` в любой из настроек — без ограничения; проверка идёт по статистике и
докачке не мешает.
