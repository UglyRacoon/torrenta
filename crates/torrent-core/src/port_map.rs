//! Проброс портов на домашнем роутере: UPnP-IGD (SSDP + SOAP) и NAT-PMP (RFC 6886).
//!
//! Публичный API намеренно «мягкий»: [`map_port`] возвращает `None`, если проброс
//! не удался (нет UPnP, провайдер блокирует, шлюз молчит), а [`unmap_port`] —
//! best effort и никогда не паникует. Сетевые вызовы ограничены таймаутами,
//! потому что ждать роутер при старте клиента никто не хочет.
//!
//! Ограничения (честно): шлюз для NAT-PMP определяется эвристикой «локальный
//! адрес с последним октетом 1», потому что без новых зависимостей получить
//! таблицу маршрутов Windows нельзя. Если эвристика не сработает, останется
//! попытка через UPnP.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use reqwest::Client;
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::time::{timeout, Instant};

/// Адрес multicast-рассылки SSDP.
const SSDP_MULTICAST: &str = "239.255.255.250:1900";
/// Тип устройства, который ищем в SSDP.
const IGD_DEVICE_ST: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";
/// Сервис UPnP: WANIPConnection (типичный роутер).
const WAN_IP_SERVICE: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
/// Сервис UPnP: WANPPPConnection (ADSL/PPPoE-роутеры).
const WAN_PPP_SERVICE: &str = "urn:schemas-upnp-org:service:WANPPPConnection:1";
/// Описание правила, видимое в веб-интерфейсе роутера.
const MAPPING_DESCRIPTION: &str = "Torrenta";
/// Сколько ждём ответы на SSDP M-SEARCH.
const SSDP_TIMEOUT: Duration = Duration::from_millis(2500);
/// Таймаут HTTP-запросов к роутеру (описание устройства, SOAP-вызовы).
const HTTP_TIMEOUT: Duration = Duration::from_secs(4);
/// Предел на снятие правила (роутер может не отвечать вовсе).
const UNMAP_TIMEOUT: Duration = Duration::from_secs(6);
/// Разумный предел XML-описания устройства.
const MAX_DESCRIPTION_BYTES: usize = 512 * 1024;
/// UDP-порт NAT-PMP.
const NATPMP_PORT: u16 = 5351;
/// Версия протокола NAT-PMP.
const NATPMP_VERSION: u8 = 0;
/// Опкод NAT-PMP: запрос внешнего адреса.
const NATPMP_OP_EXTERNAL_ADDRESS: u8 = 0;
/// Опкод NAT-PMP: проброс UDP.
const NATPMP_OP_MAP_UDP: u8 = 1;
/// Опкод NAT-PMP: проброс TCP.
const NATPMP_OP_MAP_TCP: u8 = 2;
/// Флаг «это ответ» в опкоде NAT-PMP.
const NATPMP_RESPONSE_FLAG: u8 = 0x80;
/// Базовый таймаут ожидания пакета NAT-PMP.
const NATPMP_TIMEOUT: Duration = Duration::from_millis(250);
/// Сколько раз повторяем запрос NAT-PMP (RFC 6886 разрешает до трёх).
const NATPMP_ATTEMPTS: u32 = 3;

/// Протокол, для которого пробрасывается порт.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingProtocol {
    /// TCP (входящие соединения пиров).
    Tcp,
    /// UDP (DHT, uTP).
    Udp,
}

impl MappingProtocol {
    /// Имя протокола в терминах UPnP.
    pub fn as_str(self) -> &'static str {
        match self {
            MappingProtocol::Tcp => "TCP",
            MappingProtocol::Udp => "UDP",
        }
    }
}

/// Установленное правило проброса.
///
/// Внутренний порт здесь не хранится: мы всегда просим внешний порт равным
/// внутреннему, поэтому [`PortMapping::external_port`] же используется и при
/// снятии правила.
#[derive(Debug, Clone)]
pub struct PortMapping {
    /// Шлюз (IP роутера), который согласился пробросить порт.
    pub gateway: IpAddr,
    /// Внешний порт, на который роутер принимает соединения.
    pub external_port: u16,
    /// Протокол проброса.
    pub protocol: MappingProtocol,
    /// Сколько живёт правило.
    pub lifetime: Duration,
}

/// Ошибки проброса портов (наружу почти не выходят — API возвращает `Option`).
#[derive(Debug, Error)]
pub enum PortMapError {
    #[error("сеть: {0}")]
    Network(String),
    #[error("разбор ответа: {0}")]
    Parse(String),
    #[error("устройство вернуло ошибку {code}: {message}")]
    Soap { code: String, message: String },
}

/// Пробует пробросить `internal_port` на роутере: сначала UPnP-IGD, затем NAT-PMP.
///
/// `lifetime_secs` зажимается в разумные рамки (1 минута … 7 суток). Возвращённое
/// правило можно передать в [`unmap_port`], когда клиент выключается; если правило
/// потеряется, роутер забудет проброс сам по истечении `lifetime`.
pub async fn map_port(
    internal_port: u16,
    protocol: MappingProtocol,
    lifetime_secs: u32,
) -> Option<PortMapping> {
    if internal_port == 0 {
        return None;
    }
    let lifetime_secs = lifetime_secs.clamp(60, 604_800);

    if let Some(mapping) = upnp_map(internal_port, protocol, lifetime_secs).await {
        return Some(mapping);
    }
    natpmp_map(internal_port, protocol, lifetime_secs).await
}

/// Снимает правило, если это возможно; ошибки игнорируются.
pub async fn unmap_port(mapping: &PortMapping) {
    let _ = timeout(
        UNMAP_TIMEOUT,
        upnp_unmap(mapping.external_port, mapping.protocol),
    )
    .await;
    if let IpAddr::V4(gateway) = mapping.gateway {
        let _ = timeout(
            UNMAP_TIMEOUT,
            natpmp_unmap(gateway, mapping.external_port, mapping.protocol),
        )
        .await;
    }
}

/// Параметры одного правила для SOAP-запроса.
struct PortRule {
    external_port: u16,
    internal_port: u16,
    protocol: MappingProtocol,
    internal_client: Ipv4Addr,
    lifetime_secs: u32,
}

// ---------------------------------------------------------------------------
// UPnP-IGD: SSDP + SOAP
// ---------------------------------------------------------------------------

/// Проброс через UPnP-IGD. `None`, если устройство не нашлось или отказало.
async fn upnp_map(
    internal_port: u16,
    protocol: MappingProtocol,
    lifetime_secs: u32,
) -> Option<PortMapping> {
    let internal_client = local_ipv4_address().await?;
    let client = http_client()?;
    for location in ssdp_discover().await {
        let Some(description) = fetch_text(&client, &location).await else {
            continue;
        };
        let Some((service, control_url)) = pick_control_url(&description, &location) else {
            continue;
        };
        let Some(gateway) = url_host_ip(&location) else {
            continue;
        };
        let rule = PortRule {
            external_port: internal_port,
            internal_port,
            protocol,
            internal_client,
            lifetime_secs,
        };
        let body = build_add_port_mapping(&service, &rule);
        if soap_call(&client, &control_url, &service, "AddPortMapping", &body)
            .await
            .is_err()
        {
            continue;
        }
        return Some(PortMapping {
            gateway,
            external_port: internal_port,
            protocol,
            lifetime: Duration::from_secs(u64::from(lifetime_secs)),
        });
    }
    None
}

/// Снятие правила через UPnP (best effort, без результата).
async fn upnp_unmap(external_port: u16, protocol: MappingProtocol) {
    let Some(client) = http_client() else {
        return;
    };
    for location in ssdp_discover().await {
        let Some(description) = fetch_text(&client, &location).await else {
            continue;
        };
        let Some((service, control_url)) = pick_control_url(&description, &location) else {
            continue;
        };
        let body = build_delete_port_mapping(&service, external_port, protocol);
        if soap_call(&client, &control_url, &service, "DeletePortMapping", &body)
            .await
            .is_ok()
        {
            return;
        }
    }
}

/// HTTP-клиент для общения с роутером.
fn http_client() -> Option<Client> {
    Client::builder().timeout(HTTP_TIMEOUT).build().ok()
}

/// Рассылает SSDP M-SEARCH и собирает `LOCATION` из первого же ответа.
async fn ssdp_discover() -> Vec<String> {
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
        return Vec::new();
    };
    let _ = socket.set_multicast_ttl_v4(2);
    let search = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_MULTICAST}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {IGD_DEVICE_ST}\r\n\r\n"
    );
    if socket
        .send_to(search.as_bytes(), SSDP_MULTICAST)
        .await
        .is_err()
    {
        return Vec::new();
    }

    let deadline = Instant::now() + SSDP_TIMEOUT;
    let mut buf = vec![0u8; 2048];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Vec::new();
        }
        match timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _peer))) => {
                let text = String::from_utf8_lossy(&buf[..len]);
                let locations = parse_ssdp_locations(&text);
                if !locations.is_empty() {
                    // Первого же ответившего шлюза достаточно.
                    return locations;
                }
            }
            // Таймаут или сетевая ошибка: дальше ждать бессмысленно.
            _ => return Vec::new(),
        }
    }
}

/// Достаёт `LOCATION: http://…` из ответа SSDP (имена заголовков без регистра).
fn parse_ssdp_locations(response: &str) -> Vec<String> {
    let mut locations: Vec<String> = Vec::new();
    for line in response.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("location") {
            continue;
        }
        let value = value.trim();
        let lowered = value.to_ascii_lowercase();
        if !(lowered.starts_with("http://") || lowered.starts_with("https://")) {
            continue;
        }
        if !locations.iter().any(|known| known == value) {
            locations.push(value.to_string());
        }
    }
    locations
}

/// Ищет в XML-описании устройства сервис WANIP/WANPPP и его `controlURL`.
///
/// Возвращает пару `(serviceType, абсолютный controlURL)`.
fn pick_control_url(description: &str, location: &str) -> Option<(String, String)> {
    let lowered = description.to_ascii_lowercase();
    let mut cursor = 0usize;
    while let Some(offset) = lowered[cursor..].find("<service>") {
        let start = cursor + offset;
        let Some(end_offset) = lowered[start..].find("</service>") else {
            break;
        };
        let end = start + end_offset + "</service>".len();
        let block = &description[start..end];
        let service_type = extract_tag_text(block, "servicetype").unwrap_or_default();
        if let Some(canonical) = canonical_wan_service(&service_type) {
            if let Some(control) = extract_tag_text(block, "controlurl") {
                if let Some(url) = resolve_url(location, &control) {
                    // В SOAPAction уходит имя из описания (там бывает версия `:2`),
                    // а стандартное имя подставляем, если описание странное.
                    let service = if service_type.starts_with("urn:") {
                        service_type
                    } else {
                        canonical.to_string()
                    };
                    return Some((service, url));
                }
            }
        }
        cursor = end;
    }
    None
}

/// Узнаёт стандартное имя WAN-сервиса по типу из описания устройства.
///
/// Версия в `serviceType` (`:1`, `:2`) при сравнении игнорируется: роутеры с
/// WANIPConnection:2 по-прежнему пробрасывают порты тем же действием.
fn canonical_wan_service(service_type: &str) -> Option<&'static str> {
    let lowered = service_type.to_ascii_lowercase();
    if lowered.contains("wanipconnection") {
        Some(WAN_IP_SERVICE)
    } else if lowered.contains("wanpppconnection") {
        Some(WAN_PPP_SERVICE)
    } else {
        None
    }
}

/// Возвращает текст первого тега `tag` (имена без регистра, атрибуты допустимы).
fn extract_tag_text(xml: &str, tag: &str) -> Option<String> {
    let lowered = xml.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = lowered.find(&open)?;
    let value_start = start + open.len();
    let gt = value_start + lowered[value_start..].find('>')? + 1;
    let value_end = gt + lowered[gt..].find(&close)?;
    let value = xml[gt..value_end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Приводит `controlURL` к абсолютному виду относительно документа описания.
fn resolve_url(location: &str, control: &str) -> Option<String> {
    let control = control.trim();
    if control.is_empty() {
        return None;
    }
    let lowered = control.to_ascii_lowercase();
    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        return Some(control.to_string());
    }
    let base = url::Url::parse(location).ok()?;
    base.join(control).ok().map(|url| url.to_string())
}

/// IP-адрес роутера из `LOCATION`.
fn url_host_ip(location: &str) -> Option<IpAddr> {
    let parsed = url::Url::parse(location).ok()?;
    let host = parsed.host_str()?;
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Скачивает XML-описание устройства.
async fn fetch_text(client: &Client, url: &str) -> Option<String> {
    let response = client.get(url).timeout(HTTP_TIMEOUT).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = response.text().await.ok()?;
    if text.len() > MAX_DESCRIPTION_BYTES {
        return None;
    }
    Some(text)
}

/// Вызов SOAP-действия; интерпретирует только ошибки (`Fault`).
async fn soap_call(
    client: &Client,
    control_url: &str,
    service: &str,
    action: &str,
    body: &str,
) -> Result<(), PortMapError> {
    let response = client
        .post(control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPAction", format!("\"{service}#{action}\""))
        .body(body.to_string())
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|err| PortMapError::Network(format!("POST {control_url}: {err}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| PortMapError::Network(format!("ответ {control_url}: {err}")))?;
    if !status.is_success() {
        // Тело всё равно разбираем: там может быть внятный Fault.
        parse_soap_response(&text)?;
        return Err(PortMapError::Network(format!(
            "{control_url}: HTTP {status}"
        )));
    }
    parse_soap_response(&text)
}

/// Проверяет SOAP-ответ: `Fault` превращается в ошибку, успех — в `Ok(())`.
fn parse_soap_response(body: &str) -> Result<(), PortMapError> {
    let lowered = body.to_ascii_lowercase();
    if lowered.contains("<s:fault") || lowered.contains("<fault") {
        let code = extract_tag_text(body, "errorcode").unwrap_or_else(|| "?".to_string());
        let message = extract_tag_text(body, "errordescription").unwrap_or_default();
        return Err(PortMapError::Soap { code, message });
    }
    Ok(())
}

/// SOAP-запрос `AddPortMapping`.
fn build_add_port_mapping(service: &str, rule: &PortRule) -> String {
    let arguments = format!(
        "<NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{}</NewExternalPort>\
         <NewProtocol>{}</NewProtocol>\
         <NewInternalPort>{}</NewInternalPort>\
         <NewInternalClient>{}</NewInternalClient>\
         <NewEnabled>1</NewEnabled>\
         <NewPortMappingDescription>{}</NewPortMappingDescription>\
         <NewLeaseDuration>{}</NewLeaseDuration>",
        rule.external_port,
        rule.protocol.as_str(),
        rule.internal_port,
        rule.internal_client,
        MAPPING_DESCRIPTION,
        rule.lifetime_secs,
    );
    soap_envelope(service, "AddPortMapping", &arguments)
}

/// SOAP-запрос `DeletePortMapping`.
fn build_delete_port_mapping(
    service: &str,
    external_port: u16,
    protocol: MappingProtocol,
) -> String {
    let arguments = format!(
        "<NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{external_port}</NewExternalPort>\
         <NewProtocol>{}</NewProtocol>",
        protocol.as_str(),
    );
    soap_envelope(service, "DeletePortMapping", &arguments)
}

/// Собирает конверт SOAP 1.1 вокруг аргументов действия.
fn soap_envelope(service: &str, action: &str, arguments: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\r\n\
         <s:Body>\r\n\
         <u:{action} xmlns:u=\"{service}\">\r\n\
         {arguments}\r\n\
         </u:{action}>\r\n\
         </s:Body>\r\n\
         </s:Envelope>\r\n"
    )
}

// ---------------------------------------------------------------------------
// NAT-PMP (RFC 6886)
// ---------------------------------------------------------------------------

/// Опкод NAT-PMP для протокола проброса.
fn natpmp_opcode(protocol: MappingProtocol) -> u8 {
    match protocol {
        MappingProtocol::Udp => NATPMP_OP_MAP_UDP,
        MappingProtocol::Tcp => NATPMP_OP_MAP_TCP,
    }
}

/// Запрос внешнего адреса — два байта: версия и опкод.
fn build_external_address_request() -> [u8; 2] {
    [NATPMP_VERSION, NATPMP_OP_EXTERNAL_ADDRESS]
}

/// Запрос проброса: версия, опкод, резерв, внутренний порт, желаемый внешний, время жизни.
fn build_map_request(
    protocol: MappingProtocol,
    internal_port: u16,
    suggested_external_port: u16,
    lifetime_secs: u32,
) -> [u8; 12] {
    let mut request = [0u8; 12];
    request[0] = NATPMP_VERSION;
    request[1] = natpmp_opcode(protocol);
    request[4..6].copy_from_slice(&internal_port.to_be_bytes());
    request[6..8].copy_from_slice(&suggested_external_port.to_be_bytes());
    request[8..12].copy_from_slice(&lifetime_secs.to_be_bytes());
    request
}

/// Проброс через NAT-PMP. `None`, если шлюз не ответил или отказал.
async fn natpmp_map(
    internal_port: u16,
    protocol: MappingProtocol,
    lifetime_secs: u32,
) -> Option<PortMapping> {
    let local = local_ipv4_address().await?;
    let gateway = guess_gateway(local)?;
    // Сначала проверяем, что шлюз вообще говорит на NAT-PMP (RFC 6886, п. 3.2).
    let address_response = natpmp_request(gateway, &build_external_address_request())
        .await
        .ok()?;
    parse_external_address_response(&address_response).ok()?;
    let request = build_map_request(protocol, internal_port, internal_port, lifetime_secs);
    let map_response = natpmp_request(gateway, &request).await.ok()?;
    let (external_port, lifetime) = parse_map_response(&map_response, protocol).ok()?;
    Some(PortMapping {
        gateway: IpAddr::V4(gateway),
        external_port,
        protocol,
        lifetime: Duration::from_secs(u64::from(lifetime)),
    })
}

/// Снятие правила NAT-PMP — тот же запрос с нулевым временем жизни.
async fn natpmp_unmap(gateway: Ipv4Addr, internal_port: u16, protocol: MappingProtocol) {
    let request = build_map_request(protocol, internal_port, 0, 0);
    let _ = natpmp_request(gateway, &request).await;
}

/// Отправляет UDP-запрос на `gateway:5351` и ждёт ответ (с повторами по RFC 6886).
async fn natpmp_request(gateway: Ipv4Addr, payload: &[u8]) -> Result<Vec<u8>, PortMapError> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .map_err(|err| PortMapError::Network(format!("NAT-PMP: сокет: {err}")))?;
    socket
        .connect((gateway, NATPMP_PORT))
        .await
        .map_err(|err| PortMapError::Network(format!("NAT-PMP: соединение с {gateway}: {err}")))?;
    let mut buf = [0u8; 32];
    for attempt in 0..NATPMP_ATTEMPTS {
        socket
            .send(payload)
            .await
            .map_err(|err| PortMapError::Network(format!("NAT-PMP: отправка: {err}")))?;
        // RFC 6886: повторяем запрос с удвоенным интервалом ожидания.
        let wait = NATPMP_TIMEOUT * (1u32 << attempt);
        match timeout(wait, socket.recv(&mut buf)).await {
            Ok(Ok(len)) => return Ok(buf[..len].to_vec()),
            Ok(Err(err)) => return Err(PortMapError::Network(format!("NAT-PMP: приём: {err}"))),
            Err(_) => continue,
        }
    }
    Err(PortMapError::Network(format!(
        "NAT-PMP: шлюз {gateway} не ответил"
    )))
}

/// Разбирает ответ на запрос внешнего адреса (12 байт).
fn parse_external_address_response(buf: &[u8]) -> Result<IpAddr, PortMapError> {
    if buf.len() < 12 {
        return Err(PortMapError::Parse(format!(
            "NAT-PMP: ответ внешнего адреса короче 12 байт ({})",
            buf.len()
        )));
    }
    check_natpmp_header(buf, NATPMP_OP_EXTERNAL_ADDRESS, "внешнего адреса")?;
    Ok(IpAddr::V4(Ipv4Addr::new(buf[8], buf[9], buf[10], buf[11])))
}

/// Разбирает ответ на запрос проброса (16 байт): внешний порт и время жизни.
fn parse_map_response(buf: &[u8], protocol: MappingProtocol) -> Result<(u16, u32), PortMapError> {
    if buf.len() < 16 {
        return Err(PortMapError::Parse(format!(
            "NAT-PMP: ответ проброса короче 16 байт ({})",
            buf.len()
        )));
    }
    check_natpmp_header(buf, natpmp_opcode(protocol), "проброса")?;
    let external_port = u16::from_be_bytes([buf[10], buf[11]]);
    let lifetime = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
    Ok((external_port, lifetime))
}

/// Проверяет версию/опкод/код результата в ответе NAT-PMP.
fn check_natpmp_header(buf: &[u8], opcode: u8, what: &str) -> Result<(), PortMapError> {
    if buf[0] != NATPMP_VERSION {
        return Err(PortMapError::Parse(format!(
            "NAT-PMP: неизвестная версия {} в ответе {what}",
            buf[0]
        )));
    }
    let expected = NATPMP_RESPONSE_FLAG | opcode;
    if buf[1] != expected {
        return Err(PortMapError::Parse(format!(
            "NAT-PMP: неверный опкод {} в ответе {what} (ожидался {expected})",
            buf[1]
        )));
    }
    let result = u16::from_be_bytes([buf[2], buf[3]]);
    if result != 0 {
        return Err(natpmp_result_error(result));
    }
    Ok(())
}

/// Код результата NAT-PMP → понятная ошибка.
fn natpmp_result_error(code: u16) -> PortMapError {
    let text = match code {
        1 => "версия протокола не поддерживается",
        2 => "в пробросе отказано",
        3 => "сетевой сбой на шлюзе",
        4 => "у шлюза закончились ресурсы",
        5 => "опкод не поддерживается",
        _ => "неизвестная ошибка",
    };
    PortMapError::Parse(format!("NAT-PMP: шлюз вернул ошибку {code}: {text}"))
}

/// Локальный IPv4-адрес, через который машина выходит в интернет.
///
/// `UdpSocket::connect` не отправляет пакеты: он лишь назначает маршрут, поэтому
/// операция мгновенная и не требует сети.
async fn local_ipv4_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    socket.connect((Ipv4Addr::new(8, 8, 8, 8), 53)).await.ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) => Some(*addr.ip()),
        SocketAddr::V6(_) => None,
    }
}

/// Эвристика адреса шлюза: локальный адрес с последним октетом 1.
///
/// Без новых зависимостей таблицу маршрутов Windows не прочитать, а в подавляющем
/// большинстве домашних сетей шлюз — это `x.y.z.1`.
fn guess_gateway(local: Ipv4Addr) -> Option<Ipv4Addr> {
    if local.is_loopback() || local.is_unspecified() || local.is_link_local() {
        return None;
    }
    let octets = local.octets();
    Some(Ipv4Addr::new(octets[0], octets[1], octets[2], 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ответ SSDP от типичного роутера.
    const SSDP_RESPONSE: &str = "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age=120\r\n\
         ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
         USN: uuid:1234::urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
         LOCATION: http://192.168.1.1:5000/rootDesc.xml\r\n\
         \r\n";

    /// XML-описание устройства с сервисом WANIPConnection:1.
    const IGD_DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
    <deviceList>
      <device>
        <deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>
        <deviceList>
          <device>
            <deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>
            <serviceList>
              <service>
                <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
                <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
                <controlURL>/ctl/IPConn</controlURL>
                <eventSubURL>/evt/IPConn</eventSubURL>
                <SCPDURL>/WANIPCn.xml</SCPDURL>
              </service>
            </serviceList>
          </device>
        </deviceList>
      </device>
    </deviceList>
  </device>
</root>"#;

    /// Успешный SOAP-ответ на AddPortMapping.
    const SOAP_ADD_OK: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:AddPortMappingResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1"/>
  </s:Body>
</s:Envelope>"#;

    /// SOAP-Fault с типичной ошибкой «порт уже занят другим правилом».
    const SOAP_FAULT: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <s:Fault>
      <faultcode>s:Client</faultcode>
      <faultstring>UPnPError</faultstring>
      <detail>
        <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
          <errorCode>718</errorCode>
          <errorDescription>ConflictInMappingEntry</errorDescription>
        </UPnPError>
      </detail>
    </s:Fault>
  </s:Body>
</s:Envelope>"#;

    /// Правило для проверки SOAP-запросов.
    fn rule() -> PortRule {
        PortRule {
            external_port: 6881,
            internal_port: 51413,
            protocol: MappingProtocol::Tcp,
            internal_client: Ipv4Addr::new(192, 168, 1, 5),
            lifetime_secs: 3600,
        }
    }

    // ---------- SSDP ----------

    #[test]
    fn parse_ssdp_locations_extracts_headers() {
        let second = SSDP_RESPONSE
            .replace("LOCATION", "location")
            .replace("rootDesc.xml", "igd.xml");
        let text = format!("{SSDP_RESPONSE}{second}");
        assert_eq!(
            parse_ssdp_locations(&text),
            vec![
                "http://192.168.1.1:5000/rootDesc.xml".to_string(),
                "http://192.168.1.1:5000/igd.xml".to_string(),
            ]
        );
    }

    #[test]
    fn parse_ssdp_locations_skips_junk_and_duplicates() {
        let text = "HTTP/1.1 200 OK\r\n\
             SERVER: Linux UPnP/1.0\r\n\
             LOCATION:\r\n\
             LOCATION: ftp://192.168.1.1/rootDesc.xml\r\n\
             LOCATION: http://192.168.1.1/rootDesc.xml\r\n\
             LOCATION: http://192.168.1.1/rootDesc.xml\r\n\
             \r\n";
        assert_eq!(
            parse_ssdp_locations(text),
            vec!["http://192.168.1.1/rootDesc.xml".to_string()]
        );
    }

    #[test]
    fn parse_ssdp_locations_returns_empty_for_garbage() {
        assert!(parse_ssdp_locations("HTTP/1.1 404 Not Found\r\n\r\n").is_empty());
    }

    // ---------- разбор описания устройства ----------

    #[test]
    fn pick_control_url_finds_wan_ip_connection() {
        let (service, control) = pick_control_url(IGD_DESCRIPTION, "http://192.168.1.1:5000/d.xml")
            .expect("WANIPConnection найден");
        assert_eq!(service, WAN_IP_SERVICE);
        assert_eq!(control, "http://192.168.1.1:5000/ctl/IPConn");
    }

    #[test]
    fn pick_control_url_finds_wan_ppp_connection() {
        let description = IGD_DESCRIPTION
            .replace("WANIPConnection:1", "WANPPPConnection:1")
            .replace("/ctl/IPConn", "/ctl/PPPConn");
        let (service, control) = pick_control_url(&description, "http://192.168.1.1:5000/d.xml")
            .expect("WANPPPConnection найден");
        assert_eq!(service, WAN_PPP_SERVICE);
        assert_eq!(control, "http://192.168.1.1:5000/ctl/PPPConn");
    }

    #[test]
    fn pick_control_url_returns_none_without_wan_services() {
        let description = IGD_DESCRIPTION.replace(
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1",
        );
        assert!(pick_control_url(&description, "http://192.168.1.1:5000/d.xml").is_none());
    }

    #[test]
    fn pick_control_url_keeps_absolute_control_url() {
        let description =
            IGD_DESCRIPTION.replace("/ctl/IPConn", "http://192.168.1.2:80/ctl/IPConn");
        let (_service, control) =
            pick_control_url(&description, "http://192.168.1.1:5000/d.xml").expect("сервис найден");
        assert_eq!(control, "http://192.168.1.2:80/ctl/IPConn");
    }

    // ---------- SOAP ----------

    #[test]
    fn build_add_port_mapping_contains_expected_fields() {
        let body = build_add_port_mapping(WAN_IP_SERVICE, &rule());
        assert!(
            body.contains(&format!("<u:AddPortMapping xmlns:u=\"{WAN_IP_SERVICE}\">")),
            "{body}"
        );
        assert!(
            body.contains("<NewExternalPort>6881</NewExternalPort>"),
            "{body}"
        );
        assert!(
            body.contains("<NewInternalPort>51413</NewInternalPort>"),
            "{body}"
        );
        assert!(body.contains("<NewProtocol>TCP</NewProtocol>"), "{body}");
        assert!(
            body.contains("<NewInternalClient>192.168.1.5</NewInternalClient>"),
            "{body}"
        );
        assert!(body.contains("<NewEnabled>1</NewEnabled>"), "{body}");
        assert!(
            body.contains("<NewPortMappingDescription>Torrenta</NewPortMappingDescription>"),
            "{body}"
        );
        assert!(
            body.contains("<NewLeaseDuration>3600</NewLeaseDuration>"),
            "{body}"
        );
    }

    #[test]
    fn build_delete_port_mapping_contains_expected_fields() {
        let body = build_delete_port_mapping(WAN_IP_SERVICE, 6881, MappingProtocol::Udp);
        assert!(body.contains("<u:DeletePortMapping"), "{body}");
        assert!(
            body.contains("<NewExternalPort>6881</NewExternalPort>"),
            "{body}"
        );
        assert!(body.contains("<NewProtocol>UDP</NewProtocol>"), "{body}");
    }

    #[test]
    fn parse_soap_response_accepts_success() {
        assert!(parse_soap_response(SOAP_ADD_OK).is_ok());
    }

    #[test]
    fn parse_soap_response_extracts_fault() {
        let err = parse_soap_response(SOAP_FAULT).expect_err("должен быть Fault");
        match err {
            PortMapError::Soap { code, message } => {
                assert_eq!(code, "718");
                assert_eq!(message, "ConflictInMappingEntry");
            }
            other => panic!("неожиданная ошибка: {other:?}"),
        }
    }

    // ---------- NAT-PMP ----------

    #[test]
    fn natpmp_external_address_request_is_two_bytes() {
        assert_eq!(build_external_address_request(), [0, 0]);
    }

    #[test]
    fn natpmp_map_request_uses_tcp_opcode() {
        let request = build_map_request(MappingProtocol::Tcp, 6881, 6881, 3600);
        assert_eq!(
            request,
            [0, 2, 0, 0, 0x1a, 0xe1, 0x1a, 0xe1, 0x00, 0x00, 0x0e, 0x10]
        );
    }

    #[test]
    fn natpmp_delete_request_uses_udp_opcode_and_zero_lifetime() {
        let request = build_map_request(MappingProtocol::Udp, 6881, 0, 0);
        assert_eq!(request, [0, 1, 0, 0, 0x1a, 0xe1, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn natpmp_parses_external_address_response() {
        let response = [0, 0x80, 0, 0, 0, 0, 0x0e, 0x10, 203, 0, 113, 7];
        let address = parse_external_address_response(&response).expect("внешний адрес");
        assert_eq!(address, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
    }

    #[test]
    fn natpmp_parses_map_response() {
        let response = [
            0, 0x82, 0, 0, 0, 0, 0x0e, 0x10, 0x1a, 0xe1, 0x1a, 0xe1, 0x00, 0x00, 0x0e, 0x10,
        ];
        let (port, lifetime) =
            parse_map_response(&response, MappingProtocol::Tcp).expect("правило проброса");
        assert_eq!(port, 6881);
        assert_eq!(lifetime, 3600);
    }

    #[test]
    fn natpmp_reports_error_result_code() {
        let response = [0, 0x80, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0];
        let err = parse_external_address_response(&response).expect_err("шлюз отказал");
        assert!(err.to_string().contains("ошибку 2"), "{err}");
    }

    #[test]
    fn natpmp_rejects_short_packets() {
        assert!(parse_map_response(&[0, 0x82], MappingProtocol::Tcp).is_err());
        assert!(parse_external_address_response(&[0, 0x80, 0, 0]).is_err());
    }

    #[test]
    fn natpmp_rejects_mismatched_opcode() {
        let response = [0, 0x81, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let err = parse_map_response(&response, MappingProtocol::Tcp).expect_err("опкод не тот");
        assert!(err.to_string().contains("опкод"), "{err}");
    }

    #[test]
    fn mapping_protocol_values() {
        assert_eq!(MappingProtocol::Tcp.as_str(), "TCP");
        assert_eq!(MappingProtocol::Udp.as_str(), "UDP");
        assert_eq!(natpmp_opcode(MappingProtocol::Tcp), 2);
        assert_eq!(natpmp_opcode(MappingProtocol::Udp), 1);
    }

    // ---------- публичный API без сети ----------

    #[tokio::test]
    async fn map_port_rejects_zero_port() {
        assert!(map_port(0, MappingProtocol::Tcp, 3600).await.is_none());
    }

    #[tokio::test]
    #[ignore = "нужна реальная сеть с роутером, поддерживающим UPnP/NAT-PMP"]
    async fn map_port_on_real_gateway() {
        let mapping = map_port(51413, MappingProtocol::Tcp, 3600)
            .await
            .expect("роутер должен поддержать UPnP или NAT-PMP");
        assert!(mapping.external_port > 0);
        unmap_port(&mapping).await;
    }
}
