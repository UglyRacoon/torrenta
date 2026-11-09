//! Прокси для TCP-соединений: SOCKS5 (RFC 1928 + RFC 1929) и HTTP CONNECT.
//!
//! Модуль умеет установить туннель до конкретного [`SocketAddr`] — этого достаточно
//! для peer wire protocol: соединения с пирами идут через прокси, а трекеры и
//! web seeds используют [`ProxyConfig::to_url`] вместе с `reqwest::Proxy::all`.
//!
//! Ограничения: поддержана только команда `CONNECT` (BIND и UDP ASSOCIATE не нужны
//! торрент-клиенту), цель задаётся уже разрешённым [`SocketAddr`] — поэтому доменный
//! тип адреса (ATYP=3) в запросе не используется, зато поддержаны IPv4 и IPv6.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Таймаут установки TCP-соединения с прокси.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
/// Таймаут рукопожатия (приветствие/авторизация/CONNECT) с прокси.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Предел размера ответа HTTP-прокси: защита от бесконечных заголовков.
const MAX_PROXY_RESPONSE: usize = 8 * 1024;

/// Версия SOCKS5.
const SOCKS5_VERSION: u8 = 0x05;
/// Метод «без авторизации».
const AUTH_NONE: u8 = 0x00;
/// Метод «логин/пароль» (RFC 1929).
const AUTH_USER_PASS: u8 = 0x02;
/// Ответ «ни один метод не приемлем».
const AUTH_NO_ACCEPTABLE: u8 = 0xFF;
/// Команда CONNECT.
const CMD_CONNECT: u8 = 0x01;
/// Тип адреса: IPv4.
const ATYP_IPV4: u8 = 0x01;
/// Тип адреса: домен.
const ATYP_DOMAIN: u8 = 0x03;
/// Тип адреса: IPv6.
const ATYP_IPV6: u8 = 0x04;
/// Версия подпротокола логин/пароль (RFC 1929).
const USER_PASS_VERSION: u8 = 0x01;

/// Вид прокси.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// SOCKS5 (RFC 1928) с командой CONNECT.
    Socks5,
    /// HTTP-прокси с методом CONNECT.
    Http,
}

impl ProxyKind {
    /// Схема, которую понимает `reqwest::Proxy`.
    pub fn as_scheme(self) -> &'static str {
        match self {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Http => "http",
        }
    }

    /// Порт по умолчанию, если в спецификации он не указан.
    fn default_port(self) -> u16 {
        match self {
            ProxyKind::Socks5 => 1080,
            ProxyKind::Http => 8080,
        }
    }
}

impl fmt::Display for ProxyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_scheme())
    }
}

/// Настройки прокси.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// Протокол прокси.
    pub kind: ProxyKind,
    /// Хост (IPv4, IPv6 в квадратных скобках или имя).
    pub host: String,
    /// Порт прокси.
    pub port: u16,
    /// Логин, если прокси требует авторизацию.
    pub username: Option<String>,
    /// Пароль (пустая строка, если задан только логин).
    pub password: Option<String>,
}

impl ProxyConfig {
    /// Разбирает спецификацию вида `socks5://user:pass@host:port` или `http://host:port`.
    ///
    /// Схемы: `socks5`/`socks5h` — SOCKS5, `http`/`https` — HTTP-прокси (для
    /// `reqwest` всегда сериализуется как `http://`, потому что сам прокси говорит
    /// открытым текстом, а TLS шифруется внутри туннеля). Путь и query в
    /// спецификации игнорируются, порт по умолчанию — 1080 (SOCKS5) и 8080 (HTTP).
    pub fn parse(spec: &str) -> Result<Self, ProxyError> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err(ProxyError::BadScheme(
                "пустая спецификация прокси".to_string(),
            ));
        }
        let parsed = url::Url::parse(trimmed)
            .map_err(|err| ProxyError::BadScheme(format!("{trimmed}: {err}")))?;
        let kind = match parsed.scheme() {
            "socks5" | "socks5h" => ProxyKind::Socks5,
            "http" | "https" => ProxyKind::Http,
            other => {
                return Err(ProxyError::BadScheme(format!(
                    "{trimmed}: неизвестная схема прокси {other} (ожидалась socks5 или http)"
                )))
            }
        };
        let host = match parsed.host_str() {
            Some(host) if !host.is_empty() => host.to_string(),
            _ => return Err(ProxyError::BadHost(trimmed.to_string())),
        };
        let port = parsed.port().unwrap_or_else(|| kind.default_port());
        if port == 0 {
            return Err(ProxyError::BadPort(trimmed.to_string()));
        }

        let username = decoded_userinfo(parsed.username());
        // Пароль без логина бессмыслен — считаем, что авторизации нет.
        let password = match username {
            Some(_) => decoded_userinfo(parsed.password().unwrap_or("")),
            None => None,
        };

        Ok(ProxyConfig {
            kind,
            host,
            port,
            username,
            password,
        })
    }

    /// Строка для `reqwest::Proxy::all` (трекеры, web seeds).
    ///
    /// Логин и пароль percent-кодируются, чтобы `@`, `:` и пробелы не ломали URL.
    pub fn to_url(&self) -> String {
        let mut url = String::with_capacity(24 + self.host.len());
        url.push_str(self.kind.as_scheme());
        url.push_str("://");
        if let Some(username) = self.username.as_deref() {
            url.push_str(&encode_userinfo(username));
            if let Some(password) = self.password.as_deref() {
                url.push(':');
                url.push_str(&encode_userinfo(password));
            }
            url.push('@');
        }
        url.push_str(&self.host);
        url.push(':');
        url.push_str(&self.port.to_string());
        url
    }

    /// Хост без квадратных скобок — в таком виде его понимает `ToSocketAddrs`.
    fn connect_host(&self) -> &str {
        self.host.trim_start_matches('[').trim_end_matches(']')
    }
}

/// Percent-кодирование значения userinfo для [`ProxyConfig::to_url`].
fn encode_userinfo(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

/// Percent-декодирование значения из URL; пустая строка — «значения нет».
fn decoded_userinfo(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    Some(percent_decode_str(value).decode_utf8_lossy().into_owned())
}

/// Ошибки разбора спецификации прокси.
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("неподдерживаемая схема прокси: {0}")]
    BadScheme(String),
    #[error("не указан хост прокси: {0}")]
    BadHost(String),
    #[error("некорректный порт прокси: {0}")]
    BadPort(String),
}

// ---------------------------------------------------------------------------
// Установка соединения
// ---------------------------------------------------------------------------

/// Устанавливает TCP-соединение до `target` через прокси.
///
/// Для SOCKS5 выполняет приветствие, при необходимости авторизацию по RFC 1929 и
/// команду CONNECT; для HTTP-прокси — метод `CONNECT` с `Proxy-Authorization: Basic`.
/// Возвращённый поток уже является туннелем: в него можно писать протокол пира.
pub async fn connect(config: &ProxyConfig, target: SocketAddr) -> io::Result<TcpStream> {
    let host = config.connect_host();
    let port = config.port;
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("прокси {host}:{port}: таймаут подключения"),
            )
        })??;
    let _ = stream.set_nodelay(true);

    timeout(HANDSHAKE_TIMEOUT, async {
        match config.kind {
            ProxyKind::Socks5 => socks5_handshake(&mut stream, config, target).await,
            ProxyKind::Http => http_connect_handshake(&mut stream, config, target).await,
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "прокси: таймаут рукопожатия (приветствие или CONNECT)",
        )
    })??;

    Ok(stream)
}

/// Читает ровно `buf.len()` байт, подменяя текст ошибки на понятный.
async fn read_exactly(stream: &mut TcpStream, buf: &mut [u8], what: &str) -> io::Result<()> {
    stream
        .read_exact(buf)
        .await
        .map(|_read| ())
        .map_err(|err| io::Error::new(err.kind(), format!("прокси: чтение {what}: {err}")))
}

/// Отправляет байты, подменяя текст ошибки на понятный.
async fn write_all(stream: &mut TcpStream, data: &[u8], what: &str) -> io::Result<()> {
    stream
        .write_all(data)
        .await
        .map_err(|err| io::Error::new(err.kind(), format!("прокси: отправка {what}: {err}")))
}

/// Подпротокол SOCKS5: приветствие, авторизация (RFC 1929), CONNECT.
async fn socks5_handshake(
    stream: &mut TcpStream,
    config: &ProxyConfig,
    target: SocketAddr,
) -> io::Result<()> {
    // Приветствие: предлагаем «без авторизации», а при наличии логина ещё и user/pass.
    let mut greeting = Vec::with_capacity(4);
    greeting.push(SOCKS5_VERSION);
    if config.username.is_some() {
        greeting.push(2);
        greeting.push(AUTH_NONE);
        greeting.push(AUTH_USER_PASS);
    } else {
        greeting.push(1);
        greeting.push(AUTH_NONE);
    }
    write_all(stream, &greeting, "приветствия SOCKS5").await?;

    let mut method = [0u8; 2];
    read_exactly(stream, &mut method, "ответа на приветствие").await?;
    if method[0] != SOCKS5_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "прокси: ответ SOCKS5 с версией {} вместо {}",
                method[0], SOCKS5_VERSION
            ),
        ));
    }

    match method[1] {
        AUTH_NONE => {}
        AUTH_USER_PASS => {
            let Some(username) = config.username.as_deref() else {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "прокси: требует логин/пароль, а в настройках его нет",
                ));
            };
            let password = config.password.as_deref().unwrap_or("");
            if username.len() > 255 || password.len() > 255 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "прокси: логин или пароль длиннее 255 байт не поддержаны SOCKS5",
                ));
            }
            let mut packet = Vec::with_capacity(3 + username.len() + password.len());
            packet.push(USER_PASS_VERSION);
            packet.push(username.len() as u8);
            packet.extend_from_slice(username.as_bytes());
            packet.push(password.len() as u8);
            packet.extend_from_slice(password.as_bytes());
            write_all(stream, &packet, "логина и пароля SOCKS5").await?;

            let mut auth_reply = [0u8; 2];
            read_exactly(stream, &mut auth_reply, "ответа на авторизацию").await?;
            if auth_reply[1] != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("прокси: отклонил логин/пароль (код {})", auth_reply[1]),
                ));
            }
        }
        AUTH_NO_ACCEPTABLE => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "прокси: не принял ни один предложенный метод авторизации",
            ))
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("прокси: выбрал неизвестный метод авторизации {other}"),
            ))
        }
    }

    // CONNECT до цели: адрес передаём уже разрешённым (ATYP=1 или ATYP=4).
    let mut request = Vec::with_capacity(22);
    request.push(SOCKS5_VERSION);
    request.push(CMD_CONNECT);
    request.push(0);
    match target.ip() {
        IpAddr::V4(ip) => {
            request.push(ATYP_IPV4);
            request.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            request.push(ATYP_IPV6);
            request.extend_from_slice(&ip.octets());
        }
    }
    request.extend_from_slice(&target.port().to_be_bytes());
    write_all(stream, &request, "запроса CONNECT").await?;

    let mut head = [0u8; 4];
    read_exactly(stream, &mut head, "ответа на CONNECT").await?;
    if head[0] != SOCKS5_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("прокси: ответ CONNECT с версией {} вместо 5", head[0]),
        ));
    }
    if head[1] != 0 {
        return Err(socks5_reply_error(head[1]));
    }

    // За адресом и портом в ответе мы не используем, но дочитать их нужно.
    let addr_len = match head[3] {
        ATYP_IPV4 => 4,
        ATYP_IPV6 => 16,
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            read_exactly(stream, &mut len, "длины домена в ответе CONNECT").await?;
            len[0] as usize
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("прокси: неизвестный тип адреса {other} в ответе CONNECT"),
            ))
        }
    };
    let mut tail = vec![0u8; addr_len + 2];
    read_exactly(stream, &mut tail, "адреса в ответе CONNECT").await?;
    Ok(())
}

/// Переводит код ошибки SOCKS5 (REP) в [`io::Error`] с осмысленным текстом.
fn socks5_reply_error(code: u8) -> io::Error {
    let (kind, text) = match code {
        1 => (io::ErrorKind::Other, "общий сбой прокси"),
        2 => (
            io::ErrorKind::PermissionDenied,
            "соединение запрещено правилами прокси",
        ),
        3 => (io::ErrorKind::NotConnected, "сеть недоступна"),
        4 => (io::ErrorKind::AddrNotAvailable, "узел недоступен"),
        5 => (io::ErrorKind::ConnectionRefused, "в соединении отказано"),
        6 => (io::ErrorKind::TimedOut, "истёк TTL"),
        7 => (io::ErrorKind::Unsupported, "команда не поддерживается"),
        8 => (io::ErrorKind::Unsupported, "тип адреса не поддерживается"),
        _ => (io::ErrorKind::Other, "неизвестный код ответа"),
    };
    io::Error::new(
        kind,
        format!("прокси: SOCKS5 не установил соединение — {text} (код {code})"),
    )
}

/// HTTP-прокси: метод CONNECT с необязательной Basic-авторизацией.
async fn http_connect_handshake(
    stream: &mut TcpStream,
    config: &ProxyConfig,
    target: SocketAddr,
) -> io::Result<()> {
    let authority = match target.ip() {
        IpAddr::V4(ip) => format!("{ip}:{}", target.port()),
        IpAddr::V6(ip) => format!("[{ip}]:{}", target.port()),
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(username) = config.username.as_deref() {
        let password = config.password.as_deref().unwrap_or("");
        let token = base64_encode(format!("{username}:{password}").as_bytes());
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("User-Agent: Torrenta\r\nProxy-Connection: Keep-Alive\r\n\r\n");
    write_all(stream, request.as_bytes(), "запроса CONNECT").await?;

    // Читаем ответ побайтово: после `\r\n\r\n` может сразу пойти трафик туннеля,
    // и «лишние» байты нельзя терять в буфере.
    let mut head = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        if head.len() >= MAX_PROXY_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "прокси: ответ HTTP длиннее допустимого",
            ));
        }
        let read = stream
            .read(&mut byte)
            .await
            .map_err(|err| io::Error::new(err.kind(), format!("прокси: чтение ответа: {err}")))?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "прокси: закрыл соединение, не прислав ответ на CONNECT",
            ));
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let head = String::from_utf8_lossy(&head);
    let status_line = head.lines().next().unwrap_or("").trim();
    let mut parts = status_line.split_whitespace();
    let protocol = parts.next().unwrap_or("");
    let code: u16 = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("прокси: не разобран статус ответа «{status_line}»"),
            )
        })?;
    if !protocol.starts_with("HTTP/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("прокси: ответ не похож на HTTP («{status_line}»)"),
        ));
    }
    if code != 200 {
        let kind = if code == 407 {
            io::ErrorKind::PermissionDenied
        } else {
            io::ErrorKind::Other
        };
        return Err(io::Error::new(
            kind,
            format!("прокси: HTTP CONNECT отклонён с кодом {code}"),
        ));
    }
    Ok(())
}

/// Кодирует байты в Base64 (RFC 4648) — нужен только для `Proxy-Authorization`.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// Слушатель на 127.0.0.1 со свободным портом.
    async fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("слушатель на 127.0.0.1");
        let addr = listener.local_addr().expect("адрес слушателя");
        (listener, addr)
    }

    /// Конфигурация прокси, указывающая на локальный слушатель.
    fn config(kind: ProxyKind, addr: SocketAddr) -> ProxyConfig {
        ProxyConfig {
            kind,
            host: addr.ip().to_string(),
            port: addr.port(),
            username: None,
            password: None,
        }
    }

    /// Цель туннеля: реальный адрес не нужен, важно только содержимое запроса.
    fn target() -> SocketAddr {
        SocketAddr::from(([93, 184, 216, 34], 6881))
    }

    async fn read_exactly(stream: &mut TcpStream, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        stream
            .read_exact(&mut buf)
            .await
            .expect("чтение от клиента");
        buf
    }

    /// Изображает прокси: после рукопожатия подтверждает прозрачность туннеля.
    async fn tunnel_echo(mut stream: TcpStream) {
        let data = read_exactly(&mut stream, 4).await;
        assert_eq!(data, b"ping");
        stream.write_all(b"pong").await.expect("ответ в туннель");
    }

    /// Клиентская сторона проверки туннеля.
    async fn client_exchange(mut stream: TcpStream) {
        stream.write_all(b"ping").await.expect("отправка в туннель");
        let mut buf = [0u8; 4];
        stream
            .read_exact(&mut buf)
            .await
            .expect("чтение из туннеля");
        assert_eq!(&buf, b"pong");
    }

    /// Читает HTTP-заголовки до пустой строки.
    async fn read_http_head(stream: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream
                .read_exact(&mut byte)
                .await
                .expect("чтение заголовков");
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(head).expect("заголовки в UTF-8")
    }

    // ---------- разбор спецификации ----------

    #[test]
    fn proxy_config_parses_socks5_with_credentials() {
        let parsed = ProxyConfig::parse("socks5://alice:secret@127.0.0.1:11080").expect("разбор");
        assert_eq!(parsed.kind, ProxyKind::Socks5);
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 11080);
        assert_eq!(parsed.username.as_deref(), Some("alice"));
        assert_eq!(parsed.password.as_deref(), Some("secret"));
    }

    #[test]
    fn proxy_config_parses_http_with_default_port() {
        let parsed = ProxyConfig::parse("http://proxy.local").expect("разбор");
        assert_eq!(parsed.kind, ProxyKind::Http);
        assert_eq!(parsed.port, 8080);
        assert!(parsed.username.is_none());
        assert!(parsed.password.is_none());
    }

    #[test]
    fn proxy_config_decodes_percent_encoded_credentials() {
        let parsed =
            ProxyConfig::parse("socks5h://user:p%40ss%3Aword@127.0.0.1:1080").expect("разбор");
        assert_eq!(parsed.kind, ProxyKind::Socks5);
        assert_eq!(parsed.username.as_deref(), Some("user"));
        assert_eq!(parsed.password.as_deref(), Some("p@ss:word"));
    }

    #[test]
    fn proxy_config_rejects_unknown_scheme() {
        let err = ProxyConfig::parse("ftp://127.0.0.1:21").expect_err("ftp не прокси");
        assert!(matches!(err, ProxyError::BadScheme(_)), "{err:?}");
    }

    #[test]
    fn proxy_config_rejects_empty_spec() {
        let err = ProxyConfig::parse("   ").expect_err("пустая строка");
        assert!(matches!(err, ProxyError::BadScheme(_)), "{err:?}");
    }

    #[test]
    fn proxy_config_to_url_roundtrip() {
        let original = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".to_string(),
            port: 11080,
            username: Some("alice".to_string()),
            password: Some("p@ss word".to_string()),
        };
        let url = original.to_url();
        assert_eq!(url, "socks5://alice:p%40ss%20word@127.0.0.1:11080");
        let reparsed = ProxyConfig::parse(&url).expect("повторный разбор");
        assert_eq!(reparsed, original);
    }

    #[test]
    fn proxy_kind_reports_scheme_and_default_port() {
        assert_eq!(ProxyKind::Socks5.as_scheme(), "socks5");
        assert_eq!(ProxyKind::Http.as_scheme(), "http");
        assert_eq!(ProxyKind::Socks5.default_port(), 1080);
        assert_eq!(ProxyKind::Http.default_port(), 8080);
        assert_eq!(ProxyKind::Http.to_string(), "http");
    }

    #[test]
    fn base64_encoding_matches_rfc4648_vectors() {
        let cases: [(&[u8], &str); 8] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (b"user:pass", "dXNlcjpwYXNz"),
        ];
        for (input, expected) in cases {
            assert_eq!(base64_encode(input), expected);
        }
    }

    // ---------- SOCKS5 ----------

    #[tokio::test]
    async fn socks5_connect_without_auth_establishes_tunnel() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение к прокси");
            let greeting = read_exactly(&mut stream, 3).await;
            assert_eq!(greeting, vec![0x05, 0x01, 0x00]);
            stream
                .write_all(&[0x05, AUTH_NONE])
                .await
                .expect("выбор метода");
            let request = read_exactly(&mut stream, 10).await;
            assert_eq!(&request[..4], &[0x05, 0x01, 0x00, ATYP_IPV4]);
            assert_eq!(&request[4..8], &[93, 184, 216, 34]);
            assert_eq!(u16::from_be_bytes([request[8], request[9]]), 6881);
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0x1a, 0xe1])
                .await
                .expect("успешный CONNECT");
            tunnel_echo(stream).await;
        });

        let stream = connect(&config(ProxyKind::Socks5, addr), target())
            .await
            .expect("туннель через SOCKS5");
        client_exchange(stream).await;
        server.await.expect("сервер прокси");
    }

    #[tokio::test]
    async fn socks5_connect_with_auth_sends_credentials() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let greeting = read_exactly(&mut stream, 4).await;
            assert_eq!(greeting, vec![0x05, 0x02, AUTH_NONE, AUTH_USER_PASS]);
            stream
                .write_all(&[0x05, AUTH_USER_PASS])
                .await
                .expect("выбор авторизации");
            let auth = read_exactly(&mut stream, 12).await;
            assert_eq!(auth[0], USER_PASS_VERSION);
            assert_eq!(auth[1], 4);
            assert_eq!(&auth[2..6], b"user");
            assert_eq!(auth[6], 5);
            assert_eq!(&auth[7..12], b"pass!");
            stream
                .write_all(&[0x01, 0x00])
                .await
                .expect("успешная авторизация");
            let request = read_exactly(&mut stream, 10).await;
            assert_eq!(&request[..4], &[0x05, 0x01, 0x00, ATYP_IPV4]);
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 10, 0, 0, 1, 0x04, 0xd2])
                .await
                .expect("успешный CONNECT");
            tunnel_echo(stream).await;
        });

        let mut proxy = config(ProxyKind::Socks5, addr);
        proxy.username = Some("user".to_string());
        proxy.password = Some("pass!".to_string());
        let stream = connect(&proxy, target())
            .await
            .expect("туннель с авторизацией");
        client_exchange(stream).await;
        server.await.expect("сервер прокси");
    }

    #[tokio::test]
    async fn socks5_connect_rejects_bad_credentials() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let _ = read_exactly(&mut stream, 4).await;
            stream
                .write_all(&[0x05, AUTH_USER_PASS])
                .await
                .expect("выбор авторизации");
            // Пакет RFC 1929: версия, длина логина, логин, длина пароля, пароль.
            let _ = read_exactly(&mut stream, 2 + 4 + 1 + 3).await;
            stream.write_all(&[0x01, 0x01]).await.expect("отказ");
        });

        let mut proxy = config(ProxyKind::Socks5, addr);
        proxy.username = Some("user".to_string());
        proxy.password = Some("bad".to_string());
        let err = connect(&proxy, target())
            .await
            .expect_err("должен быть отказ авторизации");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("логин"), "{err}");
        server.await.expect("сервер прокси");
    }

    #[tokio::test]
    async fn socks5_connect_maps_reply_code_to_error_kind() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let _ = read_exactly(&mut stream, 3).await;
            stream
                .write_all(&[0x05, AUTH_NONE])
                .await
                .expect("выбор метода");
            let _ = read_exactly(&mut stream, 10).await;
            // REP=5 — в соединении отказано.
            stream
                .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .expect("отказ CONNECT");
        });

        let err = connect(&config(ProxyKind::Socks5, addr), target())
            .await
            .expect_err("прокси отказал");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert!(err.to_string().contains("код 5"), "{err}");
        server.await.expect("сервер прокси");
    }

    #[tokio::test]
    async fn socks5_connect_fails_when_no_acceptable_method() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let _ = read_exactly(&mut stream, 3).await;
            stream
                .write_all(&[0x05, AUTH_NO_ACCEPTABLE])
                .await
                .expect("метод не принят");
        });

        let err = connect(&config(ProxyKind::Socks5, addr), target())
            .await
            .expect_err("метод не принят");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("метод"), "{err}");
        server.await.expect("сервер прокси");
    }

    #[tokio::test]
    async fn socks5_connect_ipv6_target_uses_atyp_ipv6() {
        let (listener, addr) = listener().await;
        let ipv6: SocketAddr = "[2001:db8::1]:6881".parse().expect("IPv6-адрес");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let _ = read_exactly(&mut stream, 3).await;
            stream
                .write_all(&[0x05, AUTH_NONE])
                .await
                .expect("выбор метода");
            let request = read_exactly(&mut stream, 22).await;
            assert_eq!(&request[..4], &[0x05, 0x01, 0x00, ATYP_IPV6]);
            assert_eq!(
                &request[4..20],
                &[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
            );
            assert_eq!(u16::from_be_bytes([request[20], request[21]]), 6881);
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .expect("успешный CONNECT");
            tunnel_echo(stream).await;
        });

        let stream = connect(&config(ProxyKind::Socks5, addr), ipv6)
            .await
            .expect("туннель до IPv6");
        client_exchange(stream).await;
        server.await.expect("сервер прокси");
    }

    // ---------- HTTP CONNECT ----------

    #[tokio::test]
    async fn http_connect_without_auth_establishes_tunnel() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let head = read_http_head(&mut stream).await;
            assert!(
                head.starts_with("CONNECT 93.184.216.34:6881 HTTP/1.1\r\n"),
                "{head}"
            );
            let lowered = head.to_ascii_lowercase();
            assert!(lowered.contains("host: 93.184.216.34:6881"), "{head}");
            assert!(!lowered.contains("proxy-authorization"), "{head}");
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\nProxy-Agent: test\r\n\r\n")
                .await
                .expect("ответ 200");
            tunnel_echo(stream).await;
        });

        let stream = connect(&config(ProxyKind::Http, addr), target())
            .await
            .expect("туннель через HTTP");
        client_exchange(stream).await;
        server.await.expect("сервер прокси");
    }

    #[tokio::test]
    async fn http_connect_sends_basic_authorization() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let head = read_http_head(&mut stream).await;
            assert!(
                head.contains("Proxy-Authorization: Basic dXNlcjpwYXNz"),
                "{head}"
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .expect("ответ 200");
            tunnel_echo(stream).await;
        });

        let mut proxy = config(ProxyKind::Http, addr);
        proxy.username = Some("user".to_string());
        proxy.password = Some("pass".to_string());
        let stream = connect(&proxy, target()).await.expect("туннель с логином");
        client_exchange(stream).await;
        server.await.expect("сервер прокси");
    }

    #[tokio::test]
    async fn http_connect_reports_authentication_required() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let _ = read_http_head(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .expect("ответ 407");
        });

        let err = connect(&config(ProxyKind::Http, addr), target())
            .await
            .expect_err("прокси требует авторизацию");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("407"), "{err}");
        server.await.expect("сервер прокси");
    }

    #[tokio::test]
    async fn http_connect_fails_on_garbage_response() {
        let (listener, addr) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("подключение");
            let _ = read_http_head(&mut stream).await;
            stream
                .write_all(b"not a proxy at all\r\n")
                .await
                .expect("мусорный ответ");
        });

        let err = connect(&config(ProxyKind::Http, addr), target())
            .await
            .expect_err("мусор вместо ответа");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        server.await.expect("сервер прокси");
    }
}
