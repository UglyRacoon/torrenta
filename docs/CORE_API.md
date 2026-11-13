# Контракт API движка `torrent-core`

Документ фиксирует **точные сигнатуры**, на которые опираются и Rust-ядро, и
Tauri-бэкенд. Менять сигнатуры можно только синхронно правя этот файл.

Крейт: `crates/torrent-core`, имя пакета `torrent-core`, подключение:
`use torrent_core::{...};`

Версия контракта: **1.8**. Подсистемы: bencode и метаинформация
(`announce` + `announce-list`), magnet-ссылки с `ut_metadata` (BEP 9),
wire-протокол, HTTP- и UDP-трекеры, DHT (BEP 5), расширения (BEP 10), PEX
(BEP 11), веб-сиды (BEP 19), SOCKS5 (RFC 1928/1929) и HTTP CONNECT, проброс
портов (UPnP-IGD, NAT-PMP — RFC 6886). В 1.7 добавлены последовательная
загрузка (`picker::PickOrder`), расписание ограничений, ручное управление
трекерами и `TorrentManager::diagnostics()`. В 1.8 изменена подсистема обмена с
пирами и метрики: адаптивное окно запросов, отдача по tit-for-tat, устойчивый
разбор ответов трекера, сглаженные скорость и ETA, остаток по выбранным файлам и
размер роя из ответа трекера; подробности — в разделе «Скорость, остаток и рой
(1.8)» в конце документа.

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
| `picker` | `PiecePicker`, `PieceStatus`, `PickOrder`, `set_levels`/`level`/`reset_have`, `order`/`set_order` | lead |
| `selection` | `Selection`, `wanted_pieces`, `wanted_downloaded`, `effective_limit`, `piece_levels`/`level_of` (crate-private) | core |
| `dht` | `Dht`, `DhtConfig`, `DhtStats`, `DhtError`, `DEFAULT_BOOTSTRAP`, `announce`/`take_peers` (BEP 5) | core |
| `pex` | `PexMessage`, `EXT_PEX`, `encode`, `decode`, `diff` (BEP 11) | core |
| `port_map` | `MappingProtocol`, `PortMapping`, `PortMapError`, `map_port`, `unmap_port` (UPnP-IGD, NAT-PMP) | core |
| `proxy` | `ProxyKind`, `ProxyConfig`, `ProxyError`, `parse`, `connect` (SOCKS5 / HTTP CONNECT) | core |
| `web_seed` | `WebSeed`, `WebSeedConfig`, `WebSeedError`, `parse_url_list` (BEP 19) | core |
| `engine` | `EngineConfig`, `TorrentManager`, `TorrentSnapshot`, `Diagnostics`, `EngineError`, `EngineEvent`, `AddOptions`, `TorrentPreview`, `FilePriority`, `FileView`, `PlanKind`/`plan` (crate-private) | core |

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
announce (action 1), transaction id случайный, таймаут и до трёх попыток
(`DEFAULT_NUM_WANT`, см. ниже).

Поведение, зафиксированное тестами:
- `key` в HTTP — 8 hex-цифр (`format!("{:08x}")`), в UDP — сырой `u32`;
- `numwant` по умолчанию `DEFAULT_NUM_WANT = 200` (для UDP — `-1`, «сколько
  есть»). До 1.8 было 50: трекер отдаёт не всех пиров роя, а случайную выборку
  размером `numwant`, поэтому маленькое значение напрямую уменьшало пул пиров;
- в UDP-запросе до трёх попыток: одна потеря пакета на перегруженном трекере
  больше не выводит трекер из ротации;
- compact-пиры с `port == 0` и заведомо нерабочими адресами (`0.0.0.0/8`,
  широковещательный, multicast, link-local) отбрасываются; приватные, CGNAT и
  loopback-адреса **остаются** — в локальной сети и за общим NAT это рабочие
  собеседники;
- `peers6` (18 байт на пир) разбирается, хотя в старом BEP не упоминался;
- неполный хвост списка (UDP-ответ или компактный HTTP-ответ) больше не роняет
  announce: разбираются все полные записи, остаток игнорируется. До 1.8 такая
  мелочь теряла весь список пиров;
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

* `announce` делает до `ANNOUNCE_ROUNDS = 3` раундов `get_peers`: каждый
  следующий идёт по узлам, которых ещё не спрашивали, и останавливается, если
  раунд не принёс ответов или пиров уже достаточно. Полного обхода до схождения
  (как в Kademlia) по-прежнему нет — раундов ровно столько, чтобы уложиться в
  `ANNOUNCE_TIMEOUT`;
* `announce_peer` уходит fire-and-forget, без ожидания полного раунда;
* токены подписываются статическим секретом;
* таблица IPv4-only: компактные `nodes` — только IPv4, IPv6-пиры из `values`
  разбираются, но новых узлов из них не появляется;
* BEP 42 (secure node id) не поддерживается, защиты от Sybil нет;
* сообщения `port` (BEP 5 relay) не обрабатываются.

## `pex`

```rust
pub const EXT_PEX: u8 = 1;    // локальный id ut_pex; лимит — 200 адресов на поле
pub struct PexMessage { pub added: Vec<SocketAddr>, pub dropped: Vec<SocketAddr> }
impl PexMessage {
    pub fn encode(&self) -> Vec<u8>;
    pub fn decode(payload: &[u8]) -> Result<Self, PexError>;
    pub fn diff(known: &[SocketAddr], fresh: &[SocketAddr], dropped: &[SocketAddr]) -> PexMessage;
}
pub enum PexError {
    Bencode(String), NotDict, Malformed(&'static str), Truncated(&'static str),
}
```

Адреса — compact по 6 байт (IPv4). `encode` не пишет пустые поля (пустое
сообщение — `de`), IPv6 молча отбрасывает, `added.f` заполняет нулями. `decode`
читает `added`/`dropped`, поля `added6`/`dropped6` игнорирует (IPv6-пиры из PEX
не попадают).

`MAX_ADDRESSES = 200`: BEP 11 рекомендует 50, но libtorrent и qBittorrent кладут
в поле до 200, поэтому длинный список **обрезается** до предела, а не считается
ошибкой. До 1.8 (`limit = 50`, вариант ошибки `TooMany`) валидное сообщение с
двумя сотнями живых пиров отбрасывалось целиком.

`diff(known, fresh, dropped)` — что отправить соседу: дедупликация, не больше
`MAX_ADDRESSES` адресов на поле, отбрасываются порт 0, `0.0.0.0`,
широковещательные и multicast-адреса, IPv6; адрес, попавший и в `fresh`, и в
`dropped`, считается живым (уходит только в `added`). Loopback и приватные
адреса сохраняются: в локальной сети они рабочие.

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
    pub max_peers_per_torrent: usize,   // по умолчанию 200 (1.8; было 50), верхняя граница 500
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
    // 1.7
    pub sequential: bool,               // по умолчанию false: новые торренты качаются по порядку кусков
    pub schedule_enabled: bool,         // по умолчанию false: расписание ограничений скорости
    pub schedule_from_min: u32,         // начало окна, минуты от полуночи (0..=1439)
    pub schedule_to_min: u32,           // конец окна; from > to — окно через полночь (23:00 → 07:00)
    pub schedule_download_limit_kib: u32, // лимит загрузки внутри окна, КиБ/с (0 = без ограничения)
    pub schedule_upload_limit_kib: u32, // лимит отдачи внутри окна, КиБ/с (0 = без ограничения)
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
    pub downloaded_bytes: u64,      // за сессию (полезные данные, включая невыбранные файлы)
    pub remaining_bytes: u64,       // 1.8: остаток по выбранным файлам (0 = готово)
    pub uploaded_bytes: u64,
    pub progress: f64,              // 0.0..=1.0 по выбранным файлам
    pub piece_count: u32,
    pub piece_length: u32,
    pub pieces_have: u32,           // по всему торренту
    pub peers_connected: u32,
    pub peers_known: u32,
    pub swarm_seeds: u32,           // 1.8: сиды во всём рое по данным трекера (0 = трекер не ответил)
    pub swarm_leechers: u32,        // 1.8: личеры во всём рое по данным трекера
    pub down_rate: f64,             // байт/с, среднее за ~8 с по проводным байтам (1.8)
    pub up_rate: f64,               // байт/с, то же окно
    pub eta_seconds: Option<u64>,   // 1.8: по окну ~30 с, обновляется раз в секунду; None без оценки
    pub seed_ratio: f64,            // 1.6: отдано/скачано; 0.0, пока скачано 0
    pub seed_limit_reached: bool,   // 1.6: лимит раздачи сработал, ждём ручного resume
    pub sequential: bool,           // 1.7: этот торрент качается по порядку кусков
    pub down_limit_kib: u32,        // действующий лимит: min(эффективный глобальный, лимит торрента)
    pub up_limit_kib: u32,          // «эффективный глобальный» уже с учётом окна расписания (1.7)
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    pub listen_port: u16,            // 0 — не слушаем входящие
    pub dht: bool,                   // включён в настройках
    pub dht_port: u16,               // настроенный порт (0 = автоматически), не фактически занятый
    pub dht_nodes: usize,            // узлов в таблице DHT сейчас (0, если DHT выключен)
    pub pex: bool,
    pub web_seeds: bool,
    pub proxy: Option<String>,       // None — напрямую
    pub port_mapping: bool,
    pub mapped_external: Option<String>, // "192.168.1.1:5678" — шлюз и внешний порт, не внешний IP
    pub schedule_enabled: bool,
    pub schedule_from_min: u32,
    pub schedule_to_min: u32,
    pub schedule_active: bool,       // окно расписания активно прямо сейчас
    pub download_limit_kib: u32,     // действующий лимит с учётом расписания (0 = без ограничения)
    pub upload_limit_kib: u32,
    pub sequential: bool,            // значение из настроек для новых торрентов
    pub torrents: usize,
    pub active: usize,               // загрузка, раздача, ожидание метаданных
    pub peers: usize,                // подключённых пиров по всем торрентам
    pub download_bps: u64,           // байт/с
    pub upload_bps: u64,
    pub uptime_secs: u64,            // сколько живёт менеджер
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
    #[error("{0}")] InvalidInput(String),   // 1.7: значение ввода не принято (список трекеров)
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
    pub fn set_sequential(&self, id: u64, on: bool) -> Result<(), EngineError>;
    pub fn trackers(&self, id: u64) -> Result<Vec<String>, EngineError>;
    pub fn set_trackers(&self, id: u64, urls: Vec<String>) -> Result<(), EngineError>;
    pub fn reannounce(&self, id: u64) -> Result<(), EngineError>;
    pub fn diagnostics(&self) -> Diagnostics;
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
`0.0`. Расписание (1.7) нормализуется там же: `schedule_from_min`/`schedule_to_min`
приводятся к суткам (`% 1440`), лимиты окна зажимаются сверху
`MAX_SCHEDULE_LIMIT_KIB = 1 048 576` КиБ/с (осмысленный предел: `u32` в КиБ/с
легко переполняется), а окно нулевой длины (`schedule_from_min == schedule_to_min`)
выключает расписание — `schedule_enabled` становится `false`. В `EngineConfig`
включён `#[serde(default)]`, поэтому старые настройки
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
`snapshot.down_limit_kib`/`up_limit_kib`. «Глобальный» здесь — **эффективный**
лимит сессии (1.7): вне окна расписания это `download_limit_kib`/`upload_limit_kib`
из настроек, внутри окна — `schedule_download_limit_kib`/`schedule_upload_limit_kib`;
пересчёт идёт на каждом тике (500 мс), подробности в разделе 1.7. На сессию
существует один общий лимитер (`SharedLimiters` в `Inner`), поэтому N торрентов
при глобальных 1 МиБ/с вместе дают не больше 1 МиБ/с, а не N × 1 МиБ/с.
Изменения настроек подхватываются на следующем запросе без перезапуска.

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
Исходящий дифф строится раз в 60 с (`PEX_INTERVAL`), не больше `MAX_ADDRESSES` = 200 адресов (с 1.8; было 50)
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

## Последовательная загрузка, расписание, трекеры и диагностика (1.7)

**Порядок кусков.** За порядок выдачи кусков отвечает `picker::PickOrder`,
публичный (реэкспортируется из корня крейта вместе с `PiecePicker`):

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PickOrder {
    #[default] Rarest,
    Sequential,
}
impl PiecePicker {
    pub fn order(&self) -> PickOrder;
    pub fn set_order(&self, order: PickOrder);
}
```

* `Rarest` (по умолчанию) — уровни приоритета от старшего к младшему, внутри
  уровня rarest-first, а на старте (пока нет кусков в работе) — уклон к началу
  файлов, чтобы скачанное было полезно сразу;
* `Sequential` — уровни приоритета **не учитываются вовсе**: берётся кусок с
  наименьшим индексом среди нужных, имеющихся у пира и ещё не запрошенных.
  Выбор файлов при этом действует: куски пропущенных файлов в `wanted` не
  попадают и в поиске не участвуют — режим меняет только очерёдность внутри
  нужного.

Включается двумя независимыми путями:

* `EngineConfig.sequential` — значение по умолчанию для **новых** торрентов: при
  создании ручки оно копируется в её собственный флаг `sequential`. Уже
  добавленные торренты `set_config` не трогает — их флаг живёт отдельно, поэтому
  глобальную настройку можно менять, не переключая существующие загрузки;
* `TorrentManager::set_sequential(id, on)` — точечно для одного торрента.
  Неизвестный id — `EngineError::NotFound(id)`. Флаг пишется сразу, а если
  рантайм уже создан, порядок меняется в тот же вызов; для magnet без
  метаданных флаг только запоминается и применяется в `install()` уже после
  разбора метаинформации. Текущее состояние отдаётся в
  `TorrentSnapshot.sequential`.

Переключение меняет только **следующие** решения планировщика: уже выданные
запросам куски докачиваются, ничего не отменяется и не удаляется; при
`Sequential` приоритеты файлов перестают влиять на порядок и остаются полезны
только как `Skip`/не `Skip`, а лимиты скорости и очередь не меняются.

**Расписание ограничений.** Шесть полей `EngineConfig`:

| Поле | Тип | Смысл |
|---|---|---|
| `schedule_enabled` | `bool` | расписание включено; по умолчанию `false` |
| `schedule_from_min` | `u32` | начало окна, минуты от полуночи (`0..=1439`; больше — приводится `% 1440`) |
| `schedule_to_min` | `u32` | конец окна, **не включается**; `from > to` — окно через полночь |
| `schedule_download_limit_kib` | `u32` | лимит загрузки внутри окна, КиБ/с; `0` — без ограничения |
| `schedule_upload_limit_kib` | `u32` | лимит отдачи внутри окна, КиБ/с; `0` — без ограничения |

Правила:

* окно — полуинтервал `[from, to)`: `23:00 → 07:00` это `1380`/`420`, внутри
  активны минуты `1380..=1439` и `0..=419`, а `420` — уже нет; при `from == to`
  окно пустое, и `sanitized()` выключает расписание;
* внутри окна действующие **глобальные** лимиты берутся из `schedule_*`
  целиком, а не «не больше базовых»: ночью потолок можно и поднять. Вне окна —
  `download_limit_kib`/`upload_limit_kib` из настроек;
* лимит торрента действует поверх: `snapshot.down_limit_kib` =
  `selection::effective_limit(действующий глобальный, лимит торрента)`, где `0`
  с любой стороны значит «без потолка с этой стороны»;
* значения пересчитываются на каждом тике статистики (`STATS_TICK = 500 мс`) и
  сразу при `set_config` — смена настроек видна без перезапуска, а граница окна
  подхватывается с точностью до одного тика. Запись в общие атомики идёт только
  при реальном изменении (`EffectiveLimits::set`), лишних записей нет;
* время суток берётся локальное, но по-разному: на Windows — через
  `GetLocalTime` (FFI, без новых зависимостей), на остальных платформах
  часового пояса нет (`chrono`/`time` в зависимостях отсутствуют), поэтому там
  окно считается по **UTC**. На практике ядро сейчас используется на Windows,
  но факт надо держать в голове при портировании.

```rust
// Ночью без ограничения, днём — 256/64 КиБ/с.
manager.set_config(EngineConfig {
    schedule_enabled: true,
    schedule_from_min: 23 * 60,
    schedule_to_min: 7 * 60,
    schedule_download_limit_kib: 0,
    schedule_upload_limit_kib: 0,
    download_limit_kib: 256,
    upload_limit_kib: 64,
    ..manager.config()
});
```

**Трекеры.** `trackers(id)` отдаёт копию сохранённого списка: сначала адреса из
метаинформации, затем добавленные вручную. На чтении список не
нормализуется и не дедуплицируется — возвращается то, что лежит в состоянии.
Неизвестный id — `NotFound`; фоновые задачи ручка не поднимает (в отличие от
большинства методов менеджера).

`set_trackers(id, urls)` **полностью заменяет** список. Каждый адрес проходит
`normalize_trackers`:

* пробелы по краям обрезаются, пустые строки пропускаются;
* не разбирается как URL → `EngineError::InvalidInput("некорректный адрес трекера: {url}")`;
* схема не `http`/`https`/`udp` → `InvalidInput("неподдерживаемая схема трекера: {url}")`;
* нет хоста → `InvalidInput("в адресе трекера нет хоста: {url}")`;
* дубликаты отбрасываются без учёта регистра (сравнивается весь URL), поэтому
  `HTTP://Tracker.Example/announce` и `http://tracker.example/announce` — один
  адрес;
* после 100 принятых адресов (`MAX_TRACKERS`) цикл прерывается: остаток списка
  молча теряется и уже не проверяется. Пустой итоговый список допустим — пиры
  ищутся через DHT и PEX.

После замены вызывается `Runtime::refresh_trackers`: для новых адресов
поднимаются циклы объявлений, и трекеры опрашиваются немедленно
(`wake_announcer`), а цикл, чей адрес убрали из списка, выходит сам. У одного
торрента не больше 16 одновременных циклов объявления (`MAX_TRACKER_LOOPS`):
адреса сверх этого хранятся и видны в `trackers()`, но опрашиваться не
начинают, пока не завершится чужой цикл, а завершается он при остановке
торрента. Практически это значит: опрашиваются первые 16 адресов.
Для magnet до метаданных замена списка меняет только хранимое значение: задача
получения метаданных читает список один раз на старте, а когда метаданные
придут, `Runtime::start` поставит трекеры из метаинформации вперёд и добавит
недостающие ручные — «окончательным» список на этой стадии считать нельзя.

`reannounce(id)` — побудка немедленного объявления, не дожидаясь интервала
(трекер сам ограничивает частоту ответом `interval`, 30–1800 с). Пустой список
— `Err(EngineError::InvalidInput("нет трекеров"))`; на паузе цикл объявления не
отправляет (он спит и раз в 700 мс проверяет флаг), так что объявление уйдёт
после `resume`; у magnet без метаданных объявлять ещё нечего — вернётся
`Ok(())` без сетевого действия. Как и `set_trackers`, ручка ничего не ждёт:
ответа трекера она не возвращает.

```rust
let current = manager.trackers(id)?;                 // что сейчас в списке
manager.set_trackers(id, vec![
    "http://tracker.example/announce".to_string(),
    "udp://tracker.example:1337".to_string(),
])?;                                                 // замена + немедленное объявление
manager.reannounce(id)?;                             // только объявление
manager.set_sequential(id, true)?;                   // этот торрент — по порядку кусков
```

**Диагностика.** `TorrentManager::diagnostics()` — синхронный снимок состояния
движка (никаких `.await`), его можно звать прямо из Tauri-команды. Поля:

| Поле | Тип | Смысл |
|---|---|---|
| `listen_port` | `u16` | фактический порт входящих (`0` — входящие не слушаются) |
| `dht` | `bool` | DHT включён в настройках, а не «узел поднялся» |
| `dht_port` | `u16` | **настроенный** UDP-порт DHT (`0` — автоматически); фактически занятый порт наружу не выводится |
| `dht_nodes` | `usize` | узлов в таблице DHT сейчас; `0`, если DHT выключен или узел не поднялся |
| `pex` | `bool` | обмен пирами (BEP 11) включён |
| `web_seeds` | `bool` | веб-сиды (BEP 19) включены |
| `proxy` | `Option<String>` | строка прокси как в настройках, `None` — напрямую; пароль не скрывается |
| `port_mapping` | `bool` | проброс порта включён в настройках |
| `mapped_external` | `Option<String>` | `"шлюз:внешний порт"` первого правила (`192.168.1.1:5678`); `None` — правила нет, либо его прямо сейчас обновляют, либо данных нет. Это не внешний IP: локально его узнать неоткуда |
| `schedule_enabled` | `bool` | расписание включено в настройках |
| `schedule_from_min` | `u32` | начало окна (минуты, уже нормализованные) |
| `schedule_to_min` | `u32` | конец окна |
| `schedule_active` | `bool` | окно активно **сейчас** (действующее значение, не пересчёт на месте) |
| `download_limit_kib` | `u32` | действующий глобальный лимит загрузки уже с учётом окна (`0` — без ограничения) |
| `upload_limit_kib` | `u32` | то же для отдачи |
| `sequential` | `bool` | `EngineConfig.sequential` — значение для **новых** торрентов; сколько торрентов реально качается по порядку, видно по `TorrentSnapshot.sequential` |
| `torrents` | `usize` | всего торрентов в менеджере |
| `active` | `usize` | в состоянии `Downloading`, `Seeding` или `Metadata` |
| `peers` | `usize` | сумма `peers_connected` по всем торрентам |
| `download_bps` | `u64` | сглаженная скорость загрузки, как в `totals()`, байт/с |
| `upload_bps` | `u64` | то же для отдачи |
| `uptime_secs` | `u64` | секунд с создания менеджера |

`Diagnostics` — `Serialize`/`Deserialize` без переименования, поля в JSON
остаются `snake_case`, как и у остальных DTO. Внутри `diagnostics()` вызывает
`totals()`, а тот поднимает фоновые задачи через `ensure_background()`, поэтому
самый первый вызов может их запустить. Замок проброса берётся через `try_lock`:
если проброс как раз обновляется, `mapped_external` окажется `None` — лучше
показать «нет данных», чем заблокировать поток интерфейса. `dht_nodes` читается
только у живого узла, поэтому при занятом UDP-порте (`dht: true`) там будет `0`.

**`EngineError::InvalidInput`.** Новый вариант (`#[error("{0}")]` — сообщение
уходит как есть, без префикса) для значений, которые нельзя принять. Возвращается
из `set_trackers` (три вида проверки адреса выше) и `reannounce` (пустой список);
неизвестный id по-прежнему `NotFound`. Бэкенду при обновлении важно, что
перечисление выросло: исчерпывающие `match` по `EngineError` и маппинг в
Tauri-ошибки нужно дополнить веткой, а текст показывать пользователю без
изменений.

**Ограничения 1.7.**

* расписание на не-Windows считается по UTC: часового пояса в зависимостях нет;
* граница окна применяется на ближайшем тике, поэтому `schedule_active` и
  действующие лимиты могут отставать от календаря до 500 мс; смена настроек
  через `set_config` — сразу;
* расписание задаёт только глобальные лимиты сессии; лимит торрента
  по-прежнему действует поверх (`min`), и «поднять» торрент выше торрентного
  потолка окно не может;
* `EngineConfig.sequential` влияет только на новые торренты — для существующих
  нужен `set_sequential`;
* `Sequential` игнорирует уровни приоритетов: `Low`/`Normal`/`High` перестают
  влиять на порядок (полезен остаётся только `Skip`); уже запрошенные куски при
  переключении не отменяются;
* список `set_trackers` обрезается до 100 адресов молча, а объявления идут
  только по первым 16 (`MAX_TRACKER_LOOPS`); лишние адреса видны в UI, но не
  опрашиваются;
* `set_trackers` на magnet до метаданных не окончателен: после появления
  метаинформации её трекеры встают в начало списка, а сравнение идёт по точному
  совпадению строки, поэтому адрес, отличающийся регистром, может остаться
  дважды;
* `reannounce` — только побудка: ответа не ждёт, на паузе объявление уйдёт после
  `resume`, для magnet без метаданных это `Ok(())` без сетевого действия;
* `dht_port` — настроенное значение; фактически занятый порт не выводится, а
  если UDP-порт занят и узел не поднялся, `dht: true` останется, а `dht_nodes`
  будет `0`;
* `mapped_external` — шлюз, а не внешний IP; при обновлении проброса поле
  временно `None`, а UDP-порт DHT не пробрасывается (остаётся TCP-only);
* `Diagnostics.sequential` — настройка, а не количество торрентов в
  последовательном режиме; `diagnostics()` может поднять фоновые задачи при
  первом вызове;
* `InvalidInput` — новый вариант ошибки, а `EngineError` не помечен
  `#[non_exhaustive]`, так что исчерпывающие `match` по нему в бэкенде придётся
  обновить.

**Проверки.** `cargo test --workspace` — 266 тестов пройдено, 0 провалов, 1
пропущен (`port_map::tests::map_port_on_real_gateway` — нужен живой роутер).
По 1.7 добавлено: `engine::tests::config_sanitizes_schedule` (нормализация минут
и лимитов окна, выключение окна нулевой длины), `engine::tests::schedule_window_bounds_and_crossing_midnight`
(полуинтервал, полночь, значения вне суток), `engine::tests::tracker_list_is_normalized`
(обрезка пробелов, дубли, схемы, отсутствие хоста, предел 100),
`engine::tests::diagnostics_of_empty_manager`, `engine::tests::torrent_controls_report_missing_torrent`
(`NotFound` у всех новых ручек), `picker::tests::sequential_order_takes_smallest_index`,
`sequential_order_skips_missing_and_excluded`, `sequential_order_ignores_piece_levels`
(порядок, выбор файлов и игнор уровней). Гейты: `cargo clippy --workspace
--all-targets -- -D warnings` и `cargo fmt --all -- --check` — чисто.

## Скорость, остаток и рой (1.8)

Релиз меняет то, как считаются метрики и как устроен обмен с пирами. Контракт
остался аддитивным: новые поля снимка, новые константы движка, но ни одно
существующее поле не переименовано и не удалено.

### Метрики скорости и ETA

`Stats` (внутри `TorrentHandle`, crate-private) получил счётчик проводных байт и
окна отсчётов:

```rust
pub(crate) wire_down: AtomicU64;      // байты, пришедшие из сети (пиры + веб-сиды)
struct RateWindow { /* VecDeque<(f64, u64)>, сумма байт, сумма секунд */ }
struct EtaState { value: Option<u64>, updated: Option<Instant> }

const RATE_WINDOW: usize = 16;        // 16 × 500 мс = 8 с — то, что видно в UI
const ETA_WINDOW: usize = 60;         // 30 с — только для прогноза
const ETA_REFRESH: Duration = Duration::from_secs(1);
const ETA_MIN_RATE: f64 = 2.0 * 1024.0;
const STALE_SAMPLE: f64 = 5.0;        // разрыв между тиками больше — «процесс стоял»
```

* `down_rate` / `up_rate` — среднее по окну `сумма байт / сумма времени`, а не
  EMA по двум отсчётам. Считаются по `wire_down`, поэтому на крупных кусках
  больше нет провалов в ноль между проверками.
* `eta_seconds`: скорость берётся из 30-секундного окна (или из короткого, пока
  оно не наполнилось наполовину), оценка смешивается `0.7 * prev + 0.3 * raw`,
  пересчёт не чаще раза в секунду. `None` — если остаток `0` или скорость ниже
  `ETA_MIN_RATE`; в этот момент кэш сбрасывается, чтобы после возобновления
  загрузки не показать забытую цифру.
* Разрыв между тиками больше `STALE_SAMPLE` считается паузой процесса: отсчёт
  отбрасывается, окно начинается заново (иначе сон машины просаживал среднюю на
  8 секунд вперёд).

### Остаток и рой в снимке

* `remaining_bytes` — `wanted_bytes - wanted_have`, где `wanted_have` считается
  только по выбранным файлам. `downloaded_bytes` для этого не годится: в нём
  есть и байты файлов с приоритетом `Skip`.
* `swarm_seeds` / `swarm_leechers` заполняются из `AnnounceResponse.seeds` /
  `.leechers` на каждом ответе трекера. `0` = трекер ещё не ответил (или
  сообщил `null`): это не «рой пуст». Собственные `peers_connected` /
  `peers_known` ограничены лимитом подключений и о размере роя ничего не говорят.

### Обмен с пирами

Конвейер запросов на соединение (`engine/peer.rs`, `engine/torrent.rs`):

```rust
pub(crate) const REQUEST_WINDOW: usize = 32;      // стартовое окно, блоков
pub(crate) const REQUEST_WINDOW_MIN: usize = 8;
pub(crate) const REQUEST_WINDOW_MAX: usize = 512;
const BLOCK_TIMEOUT: Duration = Duration::from_secs(8);
const UNCHOKE_SLOTS: usize = 8;
```

* окно хранится в `PeerRecord.window` и адаптируется: `+1` на каждый принятый
  блок, `÷2` при истечении `BLOCK_TIMEOUT` (с нижней границей `MIN`);
* `Runtime::peer_window` заодно снимает просроченные запросы из
  `PeerRecord.inflight` и `PieceBuffer.inflight` (оба стали
  `HashMap<u32, Instant>`), иначе «призрак» потерянного блока занимал бы место
  в окне вечно;
* `pump_requests` вызывается после каждого блока и после `Unchoke`, а не только
  по тику 250 мс;
* `Runtime::on_cancel` освобождает блок по `Cancel` от пира;
* `release_piece` при `Choke`/разрыве не выбрасывает частично скачанный кусок:
  `buffer.owner = 0`, и его подхватит следующий пир (`next_requests` умеет
  «усыновлять» такой кусок);
* отдача — tit-for-tat: `PeerRecord` начинает с `am_choking = true`,
  `should_unchoke`/`should_choke`/`set_am_choking` разчокивают
  `UNCHOKE_SLOTS` лучших по текущей скорости отдачи нам заинтересованных
  пиров (`peer_interested` теперь отдельный флаг).

Кондуктор подключений: `CONNECT_BURST = 32` (было 8), `PENDING_LIMIT = 1024`
(было 256, выбрасывается хвост, а не голова); если слотов нет, адрес
возвращается в начало очереди, а не помечается известным навсегда.

Планировщик: `PiecePicker::max_in_progress = (pieces / 8).clamp(8, 64)` — было
`(pieces / 40).clamp(4, 64)`, из-за чего на торренте 700 МБ одновременно
работало 4–8 пиров.

### Трекеры, PEX, DHT

* `DEFAULT_NUM_WANT = 200` (`tracker.rs`), `ANNOUNCE_NUM_WANT = 200` в движке —
  было 50, то есть ровно столько же, сколько разрешалось подключить;
* `parse_compact_peers` / `parse_compact_peers6` разбирают все полные записи и
  игнорируют неполный хвост вместо ошибки на весь announce;
* `is_usable_peer` отбрасывает только `0.0.0.0/8`, широковещательный,
  multicast, link-local и порт `0`; приватные, CGNAT (`100.64/10`) и loopback
  адреса остаются рабочими (локальные раздачи и пиры за общим NAT);
* UDP-обмен — до трёх попыток вместо двух;
* PEX: `MAX_ADDRESSES = 200`, длинный список обрезается (варианта ошибки
  `TooMany` больше нет), `diff` тоже ограничивает поле двумя сотнями;
* DHT: `ANNOUNCE_ROUNDS = 3` итеративных раунда `get_peers` в общем бюджете
  `ANNOUNCE_TIMEOUT`; раунд не начинается, если бюджет исчерпан, никто не
  ответил в предыдущем раунде или пиров уже достаточно.

### Настройки и миграция

* `EngineConfig::default().max_peers_per_torrent = 200`, `sanitized()` зажимает
  значение в `1..=MAX_PEERS_PER_TORRENT = 500`;
* в Tauri-слое `settings::DEFAULT_MAX_PEERS = 200` и
  `settings::migrate_legacy_defaults`: файл `settings.json`, в котором
  `max_peers == LEGACY_DEFAULT_MAX_PEERS (50)`, при загрузке поднимается до 200.
  Миграция применяется только к файлу, прочитанному с диска, и только к точному
  совпадению: 30, 100 или 128 не переписываются.

### Ограничения 1.8

* шифрование трафика (MSE/PE) не реализовано: пиры, требующие шифрование,
  недостижимы;
* uTP не реализован — только TCP; пиры, у которых открыт лишь uTP-порт, для нас
  невидимы;
* fast extension (`allowed fast`) не поддержан;
* `Message::Port` (BEP 5 relay) игнорируется;
* адаптация окна — эвристика, а не полноценная оценка BDP: на нестабильном
  канале возможны и недобор, и лишние запросы (лишние снимаются таймаутом);
* `swarm_seeds` / `swarm_leechers` известны только после ответа трекера: у
  раздачи без трекеров (DHT/PEX) они остаются `0`;
* `max_peers_per_torrent` больше не ограничен двумя сотнями, но каждое
  соединение — сокет, задача tokio и буфер приёма; 500 — сознательный потолок.

### Проверки

`cargo test --workspace` — 437 тестов, 436 пройдено, 1 пропущен
(`port_map::tests::map_port_on_real_gateway` — нужен живой роутер). Из них 228 в
библиотеке ядра (227 пройдено + 1 `#[ignore]`), 45 интеграционных в 8 наборах и
164 в Tauri-обвязке. По 1.8 добавлено: `engine::tests::rate_window_averages_and_forgets_old_samples`,
`rate_window_ignores_bad_intervals`, `eta_state_stays_stable_between_refreshes`,
`eta_is_none_without_measurable_speed`,
`engine::tests::peer_record_starts_choked_in_both_directions` (обновлён под
BEP 3), `tracker::tests::is_usable_peer_rejects_unusable_addresses`,
`parse_compact_peers_keeps_full_records_before_incomplete_tail`,
`pex::tests::decode_truncates_long_list_instead_of_rejecting`,
`dht::tests::announce_опрашивает_узлы_из_ответа_и_собирает_их_пиров`,
`settings::tests::load_migrates_legacy_peer_default` и
`load_keeps_explicit_peer_limit`. Гейты: `cargo clippy --workspace --all-targets
-- -D warnings` и `cargo fmt --all -- --check` — чисто.