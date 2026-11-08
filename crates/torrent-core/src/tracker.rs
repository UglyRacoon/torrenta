//! Общение с трекерами: HTTP(S) announce (BEP 3, BEP 23) и UDP announce (BEP 15).

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use percent_encoding::{percent_encode, NON_ALPHANUMERIC};
use thiserror::Error;
use tokio::net::UdpSocket;

use crate::bencode::{self, Value};
use crate::metainfo::InfoHash;

/// Событие announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceEvent {
    Started,
    Completed,
    Stopped,
}

impl AnnounceEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            AnnounceEvent::Started => "started",
            AnnounceEvent::Completed => "completed",
            AnnounceEvent::Stopped => "stopped",
        }
    }

    /// Числовой код события для UDP-трекера (BEP 15).
    pub fn as_udp_code(self) -> u32 {
        match self {
            AnnounceEvent::Started => 2,
            AnnounceEvent::Completed => 1,
            AnnounceEvent::Stopped => 3,
        }
    }
}

/// Параметры одного announce.
#[derive(Debug, Clone)]
pub struct AnnounceRequest {
    pub info_hash: InfoHash,
    pub peer_id: [u8; 20],
    pub port: u16,
    /// Сколько байт отдано (для статистики трекера).
    pub uploaded: u64,
    pub downloaded: u64,
    /// Сколько осталось скачать.
    pub left: u64,
    pub event: Option<AnnounceEvent>,
    pub num_want: Option<u32>,
    pub key: Option<u32>,
}

/// Адрес пира, полученный от трекера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerAddr {
    pub ip: IpAddr,
    pub port: u16,
}

impl fmt::Display for PeerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

/// Ответ трекера.
#[derive(Debug, Clone, Default)]
pub struct AnnounceResponse {
    /// Интервал повторного announce, сек.
    pub interval: u64,
    pub min_interval: Option<u64>,
    pub peers: Vec<PeerAddr>,
    pub seeds: Option<u32>,
    pub leechers: Option<u32>,
    pub warning: Option<String>,
}

/// Ошибки трекеров.
#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("неподдерживаемый трекер: {0}")]
    Unsupported(String),
    #[error("сетевая ошибка: {0}")]
    Network(String),
    #[error("ошибка разбора ответа трекера: {0}")]
    Parse(String),
    #[error("трекер вернул ошибку: {0}")]
    Failure(String),
}

// Константы протокола UDP-трекера (BEP 15).
const UDP_PROTOCOL_ID: u64 = 0x0000_0417_2710_1980;
const UDP_ACTION_CONNECT: u32 = 0;
const UDP_ACTION_ANNOUNCE: u32 = 1;
const UDP_ACTION_ERROR: u32 = 3;

/// Собирает URL announce с query-параметрами.
///
/// `info_hash` и `peer_id` кодируются как сырые байты (percent-encoding),
/// поэтому нельзя использовать обычный urlencode строки.
pub fn build_http_url(base: &str, req: &AnnounceRequest) -> Result<String, TrackerError> {
    let mut url =
        url::Url::parse(base).map_err(|err| TrackerError::Unsupported(format!("{base}: {err}")))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(TrackerError::Unsupported(format!(
                "{base}: неподдерживаемая схема {other}"
            )))
        }
    }

    // Собираем query вручную: info_hash/peer_id должны попасть в него ровно как
    // percent-encoded байты, без дополнительного кодирования '%'.
    let mut query = String::with_capacity(256);
    let mut push = |key: &str, value: &str| {
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(key);
        query.push('=');
        query.push_str(value);
    };

    let raw_info_hash = percent_encode(req.info_hash.as_bytes(), NON_ALPHANUMERIC).to_string();
    let raw_peer_id = percent_encode(&req.peer_id, NON_ALPHANUMERIC).to_string();
    push("info_hash", &raw_info_hash);
    push("peer_id", &raw_peer_id);
    push("port", &req.port.to_string());
    push("uploaded", &req.uploaded.to_string());
    push("downloaded", &req.downloaded.to_string());
    push("left", &req.left.to_string());
    push("compact", "1");
    push("numwant", &req.num_want.unwrap_or(50).to_string());
    if let Some(event) = req.event {
        push("event", event.as_str());
    }
    if let Some(key) = req.key {
        push("key", &format!("{key:08x}"));
    }
    // Query из базового URL (например, passkey) сохраняем.
    let existing = url.query().unwrap_or("").to_string();
    if existing.is_empty() {
        url.set_query(Some(&query));
    } else {
        url.set_query(Some(&format!("{existing}&{query}")));
    }
    Ok(url.to_string())
}

/// HTTP(S) announce: выполняет GET и разбирает bencode-ответ.
pub async fn http_announce(
    client: &reqwest::Client,
    base: &str,
    req: &AnnounceRequest,
    timeout: Duration,
) -> Result<AnnounceResponse, TrackerError> {
    let url = build_http_url(base, req)?;
    let response = client
        .get(url.as_str())
        .timeout(timeout)
        .send()
        .await
        .map_err(|err| TrackerError::Network(format!("GET {url}: {err}")))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| TrackerError::Network(format!("GET {url}: {err}")))?;

    match parse_response(&body) {
        Ok(parsed) => Ok(parsed),
        // Трекер может ответить ошибкой с любым HTTP-кодом.
        Err(TrackerError::Failure(reason)) => Err(TrackerError::Failure(reason)),
        Err(other) => {
            if status.is_success() {
                Err(other)
            } else {
                Err(TrackerError::Network(format!("{url}: HTTP {status}")))
            }
        }
    }
}

/// UDP announce (BEP 15): connect + announce с таймаутом и повтором.
pub async fn udp_announce(
    base: &str,
    req: &AnnounceRequest,
    timeout: Duration,
) -> Result<AnnounceResponse, TrackerError> {
    let url =
        url::Url::parse(base).map_err(|err| TrackerError::Unsupported(format!("{base}: {err}")))?;
    if url.scheme() != "udp" {
        return Err(TrackerError::Unsupported(format!(
            "{base}: ожидалась схема udp"
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| TrackerError::Unsupported(format!("{base}: нет хоста")))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| TrackerError::Unsupported(format!("{base}: нет порта")))?;
    let mut resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| TrackerError::Network(format!("{base}: {err}")))?;
    let addr: SocketAddr = resolved
        .next()
        .ok_or_else(|| TrackerError::Network(format!("{base}: адрес не разрешён")))?;

    let bind: SocketAddr = if addr.is_ipv4() {
        SocketAddr::from(([0u8, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let socket = UdpSocket::bind(bind)
        .await
        .map_err(|err| TrackerError::Network(format!("{base}: bind {bind}: {err}")))?;
    socket
        .connect(addr)
        .await
        .map_err(|err| TrackerError::Network(format!("{base}: connect {addr}: {err}")))?;

    // --- connect ---
    let connect_tx: u32 = rand::random();
    let mut packet = Vec::with_capacity(16);
    packet.extend_from_slice(&UDP_PROTOCOL_ID.to_be_bytes());
    packet.extend_from_slice(&UDP_ACTION_CONNECT.to_be_bytes());
    packet.extend_from_slice(&connect_tx.to_be_bytes());
    let response = exchange(&socket, &packet, timeout).await?;
    if response.len() < 16 {
        return Err(TrackerError::Parse("короткий ответ connect".to_string()));
    }
    let action = be_u32(&response, 0)?;
    if action == UDP_ACTION_ERROR {
        return Err(TrackerError::Failure(udp_error_message(&response, 8)));
    }
    if action != UDP_ACTION_CONNECT {
        return Err(TrackerError::Parse(format!(
            "неожиданный action {action} в ответе connect"
        )));
    }
    if be_u32(&response, 4)? != connect_tx {
        return Err(TrackerError::Network(
            "udp: transaction id connect не совпал".to_string(),
        ));
    }
    let mut connection_id = [0u8; 8];
    connection_id.copy_from_slice(&response[8..16]);

    // --- announce ---
    let announce_tx: u32 = rand::random();
    let mut packet = Vec::with_capacity(98);
    packet.extend_from_slice(&connection_id);
    packet.extend_from_slice(&UDP_ACTION_ANNOUNCE.to_be_bytes());
    packet.extend_from_slice(&announce_tx.to_be_bytes());
    packet.extend_from_slice(req.info_hash.as_bytes());
    packet.extend_from_slice(&req.peer_id);
    packet.extend_from_slice(&req.downloaded.to_be_bytes());
    packet.extend_from_slice(&req.left.to_be_bytes());
    packet.extend_from_slice(&req.uploaded.to_be_bytes());
    packet.extend_from_slice(
        &req.event
            .map(|e| e.as_udp_code())
            .unwrap_or(0)
            .to_be_bytes(),
    );
    packet.extend_from_slice(&0u32.to_be_bytes()); // ip: 0.0.0.0 = «пусть трекер угадает»
    packet.extend_from_slice(&req.key.unwrap_or(0).to_be_bytes());
    packet.extend_from_slice(&req.num_want.map(|n| n as i32).unwrap_or(-1).to_be_bytes());
    packet.extend_from_slice(&req.port.to_be_bytes());
    debug_assert_eq!(packet.len(), 98);

    let response = exchange(&socket, &packet, timeout).await?;
    if response.len() < 20 {
        return Err(TrackerError::Parse("короткий ответ announce".to_string()));
    }
    let action = be_u32(&response, 0)?;
    if action == UDP_ACTION_ERROR {
        return Err(TrackerError::Failure(udp_error_message(&response, 8)));
    }
    if action != UDP_ACTION_ANNOUNCE {
        return Err(TrackerError::Parse(format!(
            "неожиданный action {action} в ответе announce"
        )));
    }
    if be_u32(&response, 4)? != announce_tx {
        return Err(TrackerError::Network(
            "udp: transaction id announce не совпал".to_string(),
        ));
    }
    let interval = be_u32(&response, 8)? as u64;
    let leechers = be_u32(&response, 12)?;
    let seeds = be_u32(&response, 16)?;
    // Хвост — 6-байтовые адреса; неполный хвостовой адрес молча отбрасываем
    // (трекеры иногда дописывают служебные байты).
    let tail = &response[20..];
    let peers = parse_compact_peers(&tail[..tail.len() - tail.len() % 6])?;
    Ok(AnnounceResponse {
        interval,
        min_interval: None,
        peers,
        seeds: Some(seeds),
        leechers: Some(leechers),
        warning: None,
    })
}

/// Универсальный announce: сам выбирает HTTP или UDP по схеме URL.
pub async fn announce(
    client: &reqwest::Client,
    base: &str,
    req: &AnnounceRequest,
    timeout: Duration,
) -> Result<AnnounceResponse, TrackerError> {
    let scheme = url::Url::parse(base)
        .map_err(|err| TrackerError::Unsupported(format!("{base}: {err}")))?
        .scheme()
        .to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" => http_announce(client, base, req, timeout).await,
        "udp" => udp_announce(base, req, timeout).await,
        other => Err(TrackerError::Unsupported(format!(
            "{base}: неподдерживаемая схема {other}"
        ))),
    }
}

/// Разбирает bencode-ответ HTTP-трекера.
pub fn parse_response(bytes: &[u8]) -> Result<AnnounceResponse, TrackerError> {
    // Некоторые трекеры дописывают в конец ответа '\n' или пробелы — это не
    // повод считать корректный ответ ошибкой.
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let value =
        bencode::decode_all(&bytes[..end]).map_err(|err| TrackerError::Parse(err.to_string()))?;
    let dict = value
        .as_dict()
        .ok_or_else(|| TrackerError::Parse("ответ трекера — не словарь".to_string()))?;

    if let Some(value) = dict_get(dict, "failure reason") {
        let reason = value
            .as_bytes()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_else(|| "трекер вернул ошибку без описания".to_string());
        return Err(TrackerError::Failure(reason));
    }

    let mut response = AnnounceResponse {
        interval: dict_int(dict, "interval")
            .map(non_negative_u64)
            .unwrap_or(0),
        min_interval: dict_int(dict, "min interval").map(non_negative_u64),
        warning: dict_str(dict, "warning message"),
        seeds: dict_int(dict, "complete").map(non_negative_u32),
        leechers: dict_int(dict, "incomplete").map(non_negative_u32),
        ..AnnounceResponse::default()
    };

    if let Some(peers) = dict_get(dict, "peers") {
        match peers {
            Value::Bytes(compact) => {
                response.peers.extend(parse_compact_peers(compact)?);
            }
            Value::List(items) => {
                for item in items {
                    if let Some(peer) = parse_peer_dict(item) {
                        response.peers.push(peer);
                    }
                }
            }
            _ => {
                return Err(TrackerError::Parse(
                    "поле peers не является строкой или списком".to_string(),
                ))
            }
        }
    }
    // peers6 (BEP 7) — компактный список IPv6-пиров.
    if let Some(Value::Bytes(compact6)) = dict_get(dict, "peers6") {
        response.peers.extend(parse_compact_peers6(compact6)?);
    }
    Ok(response)
}

/// Разбирает компактный список пиров (BEP 23): 6 байт на пир (4 IP + 2 порт).
pub fn parse_compact_peers(data: &[u8]) -> Result<Vec<PeerAddr>, TrackerError> {
    if data.len() % 6 != 0 {
        return Err(TrackerError::Parse(format!(
            "компактный список пиров длиной {} не кратен 6",
            data.len()
        )));
    }
    let mut peers = Vec::with_capacity(data.len() / 6);
    for chunk in data.chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        let peer = PeerAddr {
            ip: IpAddr::V4(ip),
            port,
        };
        if !is_usable_peer(&peer) {
            continue;
        }
        peers.push(peer);
    }
    Ok(peers)
}

/// Компактный список IPv6-пиров (18 байт на пир).
fn parse_compact_peers6(data: &[u8]) -> Result<Vec<PeerAddr>, TrackerError> {
    if data.len() % 18 != 0 {
        return Err(TrackerError::Parse(format!(
            "компактный список IPv6-пиров длиной {} не кратен 18",
            data.len()
        )));
    }
    let mut peers = Vec::with_capacity(data.len() / 18);
    for chunk in data.chunks_exact(18) {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&chunk[..16]);
        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
        let peer = PeerAddr {
            ip: IpAddr::V6(Ipv6Addr::from(octets)),
            port,
        };
        if !is_usable_peer(&peer) {
            continue;
        }
        peers.push(peer);
    }
    Ok(peers)
}

/// Пир из некороткого формата (`peers: [{ip, port, ...}]`).
fn parse_peer_dict(value: &Value) -> Option<PeerAddr> {
    let dict = value.as_dict()?;
    let ip: IpAddr = dict_str(dict, "ip")?.parse().ok()?;
    let port = dict_int(dict, "port")?;
    if !(0..=u16::MAX as i64).contains(&port) {
        return None;
    }
    let peer = PeerAddr {
        ip,
        port: port as u16,
    };
    is_usable_peer(&peer).then_some(peer)
}

/// Пир, к которому вообще имеет смысл подключаться.
///
/// Трекеры любят присылать записи-заглушки `0.0.0.0:0` и адреса с нулевым
/// портом; подключаться к ним бессмысленно, поэтому отфильтровываем.
fn is_usable_peer(peer: &PeerAddr) -> bool {
    peer.port != 0 && !peer.ip.is_unspecified()
}

/// Отправляет пакет и ждёт ответ; при таймауте повторяет отправку один раз.
async fn exchange(
    socket: &UdpSocket,
    packet: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>, TrackerError> {
    let mut buf = vec![0u8; 4096];
    for attempt in 0..2 {
        socket
            .send(packet)
            .await
            .map_err(|err| TrackerError::Network(format!("udp send: {err}")))?;
        match tokio::time::timeout(timeout, socket.recv(&mut buf)).await {
            Ok(Ok(len)) => return Ok(buf[..len].to_vec()),
            Ok(Err(err)) => return Err(TrackerError::Network(format!("udp recv: {err}"))),
            Err(_) if attempt == 0 => continue,
            Err(_) => {
                return Err(TrackerError::Network(
                    "udp: трекер не ответил вовремя".to_string(),
                ))
            }
        }
    }
    Err(TrackerError::Network("udp: нет ответа".to_string()))
}

fn be_u32(data: &[u8], offset: usize) -> Result<u32, TrackerError> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| TrackerError::Parse("ответ короче ожидаемого".to_string()))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Сообщение об ошибке UDP-трекера: action+transaction уже прочитаны.
fn udp_error_message(response: &[u8], offset: usize) -> String {
    let text = String::from_utf8_lossy(&response[offset.min(response.len())..]).into_owned();
    if text.is_empty() {
        "трекер вернул ошибку без описания".to_string()
    } else {
        text
    }
}

fn dict_int(dict: &BTreeMap<Vec<u8>, Value>, key: &str) -> Option<i64> {
    dict.get(key.as_bytes()).and_then(Value::as_int)
}

fn dict_str(dict: &BTreeMap<Vec<u8>, Value>, key: &str) -> Option<String> {
    dict.get(key.as_bytes())
        .and_then(Value::as_bytes)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .filter(|s| !s.is_empty())
}

fn non_negative_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn non_negative_u32(value: i64) -> u32 {
    value.clamp(0, u32::MAX as i64) as u32
}

fn dict_get<'a>(dict: &'a BTreeMap<Vec<u8>, Value>, key: &str) -> Option<&'a Value> {
    dict.get(key.as_bytes())
}

/// Декодирует percent-encoding в query-параметрах URL (`+` — пробел).
#[cfg(test)]
fn decode_query_value(value: &str) -> Vec<u8> {
    let mut raw = Vec::with_capacity(value.len());
    for byte in value.as_bytes() {
        raw.push(if *byte == b'+' { b' ' } else { *byte });
    }
    percent_encoding::percent_decode(&raw).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn hash(byte: u8) -> InfoHash {
        InfoHash::new([byte; 20])
    }

    fn request() -> AnnounceRequest {
        AnnounceRequest {
            info_hash: hash(0x11),
            peer_id: *b"-TR0001-abcdefghijkl",
            port: 6881,
            uploaded: 100,
            downloaded: 200,
            left: 300,
            event: Some(AnnounceEvent::Started),
            num_want: Some(30),
            key: Some(0xdead_beef),
        }
    }

    fn query_map(url: &str) -> BTreeMap<String, String> {
        let parsed = url::Url::parse(url).unwrap();
        let mut map = BTreeMap::new();
        if let Some(query) = parsed.query() {
            for pair in query.split('&') {
                if let Some((key, value)) = pair.split_once('=') {
                    map.insert(
                        key.to_string(),
                        String::from_utf8_lossy(&decode_query_value(value)).into_owned(),
                    );
                }
            }
        }
        map
    }

    // ---------- build_http_url ----------

    #[test]
    fn build_http_url_percent_encodes_raw_hash_bytes() {
        let mut req = request();
        req.info_hash = InfoHash::new([
            0x00, 0xFF, b' ', b'+', b'&', b'=', 0x80, 0x7F, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ]);
        let url = build_http_url("http://tracker.example/announce", &req).unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.scheme(), "http");
        assert_eq!(parsed.host_str(), Some("tracker.example"));
        assert_eq!(parsed.path(), "/announce");

        let query = parsed.query().unwrap();
        // Ровно те байты, что в info-hash: '%' не должен перекодироваться в '%25'.
        assert!(
            query.contains("info_hash=%00%FF%20%2B%26%3D%80%7F"),
            "{query}"
        );

        let mut seen = 0;
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap();
            match key {
                "info_hash" => {
                    assert_eq!(decode_query_value(value), req.info_hash.as_bytes().to_vec());
                    seen += 1;
                }
                "peer_id" => {
                    assert_eq!(decode_query_value(value), req.peer_id.to_vec());
                    seen += 1;
                }
                _ => {}
            }
        }
        assert_eq!(seen, 2);
    }

    #[test]
    fn build_http_url_includes_parameters_and_keeps_base_query() {
        let url = build_http_url("http://t.example/announce?passkey=abc", &request()).unwrap();
        let map = query_map(&url);
        assert_eq!(map["passkey"], "abc");
        assert_eq!(map["port"], "6881");
        assert_eq!(map["uploaded"], "100");
        assert_eq!(map["downloaded"], "200");
        assert_eq!(map["left"], "300");
        assert_eq!(map["compact"], "1");
        assert_eq!(map["numwant"], "30");
        assert_eq!(map["key"], "deadbeef");
        assert_eq!(map["event"], "started");

        // Необязательные параметры отсутствуют — нет пустых значений.
        let mut req = request();
        req.event = None;
        req.num_want = None;
        req.key = None;
        let url = build_http_url("https://t.example/a", &req).unwrap();
        let map = query_map(&url);
        assert!(!map.contains_key("event"));
        assert!(!map.contains_key("key"));
        assert_eq!(map["numwant"], "50");
        // Значения по умолчанию для счётчиков.
        assert!(url.starts_with("https://t.example/a?info_hash="));
    }

    #[test]
    fn build_http_url_rejects_unsupported_schemes() {
        let req = request();
        for base in [
            "udp://t.example:6969/announce",
            "ftp://t.example/a",
            "не url",
            "http://",
        ] {
            assert!(
                matches!(
                    build_http_url(base, &req),
                    Err(TrackerError::Unsupported(_))
                ),
                "{base}"
            );
        }
    }

    // ---------- parse_compact_peers ----------

    #[test]
    fn parse_compact_peers_skips_zero_entries() {
        let data = [
            127, 0, 0, 1, 0x1a, 0xe1, // 127.0.0.1:6881
            0, 0, 0, 0, 0, 0, // заглушка трекера
            10, 0, 0, 2, 0, 0, // порт 0 — бесполезен
            0xFF, 0xFF, 0xFF, 0xFE, 0x00, 0x50, // 255.255.255.254:80
        ];
        let peers = parse_compact_peers(&data).unwrap();
        assert_eq!(
            peers,
            vec![
                PeerAddr {
                    ip: "127.0.0.1".parse().unwrap(),
                    port: 6881
                },
                PeerAddr {
                    ip: "255.255.255.254".parse().unwrap(),
                    port: 80
                },
            ]
        );
        assert_eq!(peers[0].to_string(), "127.0.0.1:6881");
        assert!(parse_compact_peers(&[]).unwrap().is_empty());
        assert!(matches!(
            parse_compact_peers(&[1, 2, 3, 4, 5]),
            Err(TrackerError::Parse(_))
        ));
    }

    // ---------- parse_response ----------

    #[test]
    fn parse_response_compact_with_all_fields() {
        let peers = [127u8, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0, 3, 0x1f, 0x90];
        let mut body =
            b"d8:completei5e10:incompletei3e8:intervali1800e12:min intervali60e5:peers".to_vec();
        body.extend_from_slice(format!("{}:", peers.len()).as_bytes());
        body.extend_from_slice(&peers);
        body.extend_from_slice(b"15:warning message4:oops");
        body.push(b'e');

        let resp = parse_response(&body).unwrap();
        assert_eq!(resp.interval, 1800);
        assert_eq!(resp.min_interval, Some(60));
        assert_eq!(resp.seeds, Some(5));
        assert_eq!(resp.leechers, Some(3));
        assert_eq!(resp.warning.as_deref(), Some("oops"));
        assert_eq!(resp.peers.len(), 2);
        assert_eq!(resp.peers[1].to_string(), "10.0.0.3:8080");
    }

    #[test]
    fn parse_response_dict_peers_and_missing_fields() {
        let mut body = b"d8:intervali900e5:peersl".to_vec();
        body.extend_from_slice(b"d2:ip9:127.0.0.14:porti6881ee");
        // Порт 0 и битый ip отбрасываются.
        body.extend_from_slice(b"d2:ip3:1.24:porti0ee");
        body.push(b'e');
        body.push(b'e');

        let resp = parse_response(&body).unwrap();
        assert_eq!(resp.interval, 900);
        assert_eq!(resp.min_interval, None);
        assert_eq!(resp.seeds, None);
        assert_eq!(resp.leechers, None);
        assert_eq!(resp.warning, None);
        assert_eq!(
            resp.peers,
            vec![PeerAddr {
                ip: "127.0.0.1".parse().unwrap(),
                port: 6881
            }]
        );

        // Пустой словарь: всё по нулям, но не ошибка.
        let empty = parse_response(b"de").unwrap();
        assert_eq!(empty.interval, 0);
        assert!(empty.peers.is_empty());
    }

    #[test]
    fn parse_response_reports_failure_and_rejects_garbage() {
        assert!(matches!(
            parse_response(b"d14:failure reason6:brokene"),
            Err(TrackerError::Failure(reason)) if reason == "broken"
        ));
        // Ошибка трекера важнее кривого HTTP-статуса.
        assert!(matches!(
            parse_response(b"not bencode"),
            Err(TrackerError::Parse(_))
        ));
        // Не словарь.
        assert!(matches!(
            parse_response(b"i42e"),
            Err(TrackerError::Parse(_))
        ));
        // peers неверного типа.
        assert!(matches!(
            parse_response(b"d5:peersi1ee"),
            Err(TrackerError::Parse(_))
        ));
        // peers6 не кратен 18.
        assert!(matches!(
            parse_response(b"d6:peers65:abcdee"),
            Err(TrackerError::Parse(_))
        ));
        // Хвостовые пробелы/переводы строк допустимы.
        let resp = parse_response(b"d8:intervali60ee\n").unwrap();
        assert_eq!(resp.interval, 60);
    }

    #[test]
    fn parse_response_handles_ipv6_compact_peers() {
        let mut body = b"d6:peers6".to_vec();
        let mut peer6 = vec![0u8; 18];
        peer6[15] = 1; // ::1
        peer6[16] = 0x1a;
        peer6[17] = 0xe1;
        body.extend_from_slice(format!("{}:", peer6.len()).as_bytes());
        body.extend_from_slice(&peer6);
        body.push(b'e');

        let resp = parse_response(&body).unwrap();
        assert_eq!(
            resp.peers,
            vec![PeerAddr {
                ip: "::1".parse().unwrap(),
                port: 6881
            }]
        );
    }

    // ---------- udp_announce ----------

    #[tokio::test]
    async fn udp_announce_connect_and_announce() {
        const CONNECTION_ID: u64 = 0x1122_3344_5566_7788;
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let req = request();

        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];

            // connect
            let (len, peer) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(len, 16);
            assert_eq!(
                u64::from_be_bytes(buf[0..8].try_into().unwrap()),
                UDP_PROTOCOL_ID
            );
            assert_eq!(u32::from_be_bytes(buf[8..12].try_into().unwrap()), 0);
            let connect_tx = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let mut reply = Vec::new();
            reply.extend_from_slice(&UDP_ACTION_CONNECT.to_be_bytes());
            reply.extend_from_slice(&connect_tx.to_be_bytes());
            reply.extend_from_slice(&CONNECTION_ID.to_be_bytes());
            server.send_to(&reply, peer).await.unwrap();

            // announce
            let (len, peer) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(len, 98);
            assert_eq!(
                u64::from_be_bytes(buf[0..8].try_into().unwrap()),
                CONNECTION_ID
            );
            assert_eq!(
                u32::from_be_bytes(buf[8..12].try_into().unwrap()),
                UDP_ACTION_ANNOUNCE
            );
            let announce_tx = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            assert_eq!(&buf[16..36], req.info_hash.as_bytes());
            assert_eq!(&buf[36..56], &req.peer_id);
            assert_eq!(
                u64::from_be_bytes(buf[56..64].try_into().unwrap()),
                req.downloaded
            );
            assert_eq!(
                u64::from_be_bytes(buf[64..72].try_into().unwrap()),
                req.left
            );
            assert_eq!(
                u64::from_be_bytes(buf[72..80].try_into().unwrap()),
                req.uploaded
            );
            assert_eq!(
                u32::from_be_bytes(buf[80..84].try_into().unwrap()),
                AnnounceEvent::Started.as_udp_code()
            );
            assert_eq!(u32::from_be_bytes(buf[84..88].try_into().unwrap()), 0);
            assert_eq!(
                u32::from_be_bytes(buf[88..92].try_into().unwrap()),
                req.key.unwrap()
            );
            assert_eq!(
                u32::from_be_bytes(buf[92..96].try_into().unwrap()),
                req.num_want.unwrap()
            );
            assert_eq!(
                u16::from_be_bytes(buf[96..98].try_into().unwrap()),
                req.port
            );

            let mut reply = Vec::new();
            reply.extend_from_slice(&UDP_ACTION_ANNOUNCE.to_be_bytes());
            reply.extend_from_slice(&announce_tx.to_be_bytes());
            reply.extend_from_slice(&1800u32.to_be_bytes()); // interval
            reply.extend_from_slice(&3u32.to_be_bytes()); // leechers
            reply.extend_from_slice(&5u32.to_be_bytes()); // seeds
            reply.extend_from_slice(&[127, 0, 0, 1, 0x1a, 0xe1]);
            // Неполный хвост (3 байта) не должен ломать разбор.
            reply.extend_from_slice(&[1, 2, 3]);
            server.send_to(&reply, peer).await.unwrap();
        });

        let base = format!("udp://127.0.0.1:{}/announce", server_addr.port());
        let resp = udp_announce(&base, &req, Duration::from_secs(2))
            .await
            .unwrap();
        handle.await.unwrap();
        assert_eq!(resp.interval, 1800);
        assert_eq!(resp.seeds, Some(5));
        assert_eq!(resp.leechers, Some(3));
        assert_eq!(resp.min_interval, None);
        assert_eq!(
            resp.peers,
            vec![PeerAddr {
                ip: "127.0.0.1".parse().unwrap(),
                port: 6881
            }]
        );
    }

    #[tokio::test]
    async fn udp_announce_error_action_is_failure() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (_, peer) = server.recv_from(&mut buf).await.unwrap();
            let connect_tx = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let mut reply = Vec::new();
            reply.extend_from_slice(&UDP_ACTION_ERROR.to_be_bytes());
            reply.extend_from_slice(&connect_tx.to_be_bytes());
            reply.extend_from_slice(b"tracker is down");
            server.send_to(&reply, peer).await.unwrap();
        });

        let base = format!("udp://127.0.0.1:{}/announce", server_addr.port());
        let err = udp_announce(&base, &request(), Duration::from_secs(2))
            .await
            .unwrap_err();
        handle.await.unwrap();
        match err {
            TrackerError::Failure(reason) => assert_eq!(reason, "tracker is down"),
            other => panic!("ожидался Failure, получено {other:?}"),
        }
    }

    #[tokio::test]
    async fn udp_announce_rejects_foreign_transaction_id() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (_, peer) = server.recv_from(&mut buf).await.unwrap();
            let mut reply = Vec::new();
            reply.extend_from_slice(&UDP_ACTION_CONNECT.to_be_bytes());
            reply.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // чужой tx
            reply.extend_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
            server.send_to(&reply, peer).await.unwrap();
        });

        let base = format!("udp://127.0.0.1:{}/announce", server_addr.port());
        let err = udp_announce(&base, &request(), Duration::from_secs(2))
            .await
            .unwrap_err();
        handle.await.unwrap();
        assert!(matches!(err, TrackerError::Network(_)), "{err:?}");
    }

    #[tokio::test]
    async fn udp_announce_requires_udp_scheme() {
        let err = udp_announce(
            "http://t.example/announce",
            &request(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, TrackerError::Unsupported(_)), "{err:?}");
    }

    // ---------- http_announce / announce ----------

    /// Поднимает одноразовый HTTP-сервер, отдающий заданный ответ.
    async fn http_server(
        status_line: &'static str,
        body: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let len = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..len]).into_owned();
            let header = format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.flush().await.unwrap();
            request
        });
        (format!("http://127.0.0.1:{}/announce", addr.port()), handle)
    }

    fn bencode_peers_response(interval: i64, peers: &[u8]) -> Vec<u8> {
        let mut body = format!("d8:intervali{interval}e5:peers{}:", peers.len()).into_bytes();
        body.extend_from_slice(peers);
        body.push(b'e');
        body
    }

    #[tokio::test]
    async fn http_announce_parses_response_and_sends_query() {
        let peers = [127u8, 0, 0, 1, 0x1a, 0xe1];
        let (base, server) = http_server("200 OK", bencode_peers_response(1800, &peers)).await;
        let resp = http_announce(
            &reqwest::Client::new(),
            &base,
            &request(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let wire_request = server.await.unwrap();

        assert_eq!(resp.interval, 1800);
        assert_eq!(resp.peers.len(), 1);
        assert_eq!(resp.peers[0].to_string(), "127.0.0.1:6881");
        let first_line = wire_request.lines().next().unwrap();
        assert!(
            first_line.starts_with("GET /announce?info_hash="),
            "{first_line}"
        );
        assert!(wire_request.contains("compact=1"), "{wire_request}");
    }

    #[tokio::test]
    async fn http_announce_surfaces_tracker_failure_and_http_errors() {
        let (base, server) = http_server("200 OK", b"d14:failure reason6:brokene".to_vec()).await;
        let err = http_announce(
            &reqwest::Client::new(),
            &base,
            &request(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(matches!(err, TrackerError::Failure(reason) if reason == "broken"));

        // Не-2xx без bencode → Network, а не Parse.
        let (base, server) = http_server("404 Not Found", Vec::new()).await;
        let err = http_announce(
            &reqwest::Client::new(),
            &base,
            &request(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(matches!(err, TrackerError::Network(_)), "{err:?}");
    }

    #[tokio::test]
    async fn announce_dispatches_by_scheme() {
        let err = announce(
            &reqwest::Client::new(),
            "ftp://t.example/announce",
            &request(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, TrackerError::Unsupported(_)), "{err:?}");

        let peers = [10u8, 0, 0, 7, 0x00, 0x50];
        let (base, server) = http_server("200 OK", bencode_peers_response(60, &peers)).await;
        let resp = announce(
            &reqwest::Client::new(),
            &base,
            &request(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(resp.interval, 60);
        assert_eq!(resp.peers[0].to_string(), "10.0.0.7:80");
    }
}
