//! Peer Exchange (PEX, BEP 11).
//!
//! PEX позволяет пирам обмениваться адресами других пиров, не обращаясь к
//! трекеру. Сообщение — bencode-словарь с необязательными полями:
//!
//! ```text
//! d5:added18:<3 адреса по 6 байт>7:added.f3:<флаги>7:dropped6:<1 адрес>e
//! ```
//!
//! `added` и `dropped` — компактные списки адресов: 4 байта IPv4 + 2 байта
//! порта в big-endian, всего 6 байт на адрес. `added.f` — по одному байту флагов
//! на каждый адрес из `added`; этот модуль флаги не хранит и при кодировании
//! пишет нули (зарезервированные биты обязаны быть нулевыми). Поля IPv6
//! (`added6`/`dropped6`) при разборе просто игнорируются: сообщения этого модуля
//! описывают только IPv4.
//!
//! В каждом поле — не больше [`MAX_ADDRESSES`] адресов: столько же отдаёт
//! [`PexMessage::diff`], а [`PexMessage::decode`] обрезает более длинный список
//! до этого предела, а не отвергает сообщение. Раньше лимит был 50 и длинный
//! список считался ошибкой — сообщения libtorrent и qBittorrent (они кладут в
//! `added` до 200 адресов) отбрасывались целиком вместе со всеми пирами.

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use thiserror::Error;

use crate::bencode::{self, Value};

/// Идентификатор расширения PEX в рукопожатии BEP 10 (`ext_id`).
pub const EXT_PEX: u8 = 1;

/// Предельное число адресов в одном поле сообщения.
///
/// BEP 11 рекомендует 50, но libtorrent и qBittorrent шлют до 200 — берём по
/// ним: лишние адреса всё равно полезны, а память на 200 адресов ничтожна.
const MAX_ADDRESSES: usize = 200;

/// Размер одного адреса в компактном виде: 4 байта IPv4 + 2 байта порта.
const COMPACT_LEN: usize = 6;

/// Значение флагов в `added.f`: 0 — обычный пир, без особых признаков.
const ADDED_FLAGS: u8 = 0;

/// Ошибки разбора и кодирования сообщения PEX.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PexError {
    /// Полезная нагрузка не является корректным bencode.
    #[error("некорректный bencode: {0}")]
    Bencode(String),
    /// Верхний уровень сообщения — не словарь.
    #[error("сообщение PEX должно быть словарём")]
    NotDict,
    /// Поле имеет неверный тип (например, `added` — не строка).
    #[error("некорректное поле «{0}»")]
    Malformed(&'static str),
    /// Длина компактного списка не кратна размеру адреса.
    #[error("длина списка «{0}» не кратна 6 байтам")]
    Truncated(&'static str),
}

/// Сообщение PEX: кого предложить пиру и кого предложить выбросить.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PexMessage {
    /// Новые адреса: компактно по 6 байт, только IPv4.
    pub added: Vec<SocketAddr>,
    /// Более не работающие адреса: компактно по 6 байт, только IPv4.
    pub dropped: Vec<SocketAddr>,
}

impl PexMessage {
    /// Кодирует сообщение в bencode.
    ///
    /// Пустые поля не пишутся (пустое сообщение — словарь `de`), адреса IPv6
    /// пропускаются: для них в BEP 11 отдельные поля `added6`/`dropped6`,
    /// которых этот модуль не формирует.
    pub fn encode(&self) -> Vec<u8> {
        let mut dict: BTreeMap<Vec<u8>, Value> = BTreeMap::new();

        let mut compact = Vec::with_capacity(self.added.len() * COMPACT_LEN);
        let mut flags = Vec::with_capacity(self.added.len());
        for addr in &self.added {
            if let Some(bytes) = compact_addr(addr) {
                compact.extend_from_slice(&bytes);
                flags.push(ADDED_FLAGS);
            }
        }
        if !compact.is_empty() {
            dict.insert(b"added".to_vec(), Value::Bytes(compact));
            dict.insert(b"added.f".to_vec(), Value::Bytes(flags));
        }

        let mut compact_dropped = Vec::with_capacity(self.dropped.len() * COMPACT_LEN);
        for addr in &self.dropped {
            if let Some(bytes) = compact_addr(addr) {
                compact_dropped.extend_from_slice(&bytes);
            }
        }
        if !compact_dropped.is_empty() {
            dict.insert(b"dropped".to_vec(), Value::Bytes(compact_dropped));
        }

        bencode::encode(&Value::Dict(dict))
    }

    /// Разбирает полезную нагрузку расширения PEX.
    ///
    /// Отсутствующее поле — пустой список. Ошибка возвращается, если данные не
    /// bencode, верхний уровень не словарь, поле не является строкой или длина
    /// компактного списка не кратна 6 байтам. Слишком длинный список — не
    /// ошибка: лишние адреса сверх [`MAX_ADDRESSES`] просто отбрасываются.
    pub fn decode(payload: &[u8]) -> Result<Self, PexError> {
        let value =
            bencode::decode_all(payload).map_err(|err| PexError::Bencode(err.to_string()))?;
        if value.as_dict().is_none() {
            return Err(PexError::NotDict);
        }
        Ok(PexMessage {
            added: parse_compact(value.get("added"), "added")?,
            dropped: parse_compact(value.get("dropped"), "dropped")?,
        })
    }

    /// Считает, что имеет смысл отправить пиру.
    ///
    /// * `known` — адреса, которые пир уже знает (то, что мы ему раньше
    ///   отправляли, включая его собственный адрес) — в `added` не попадают;
    /// * `fresh` — адреса, которые мы готовы предложить;
    /// * `dropped` — адреса, которые, по нашим данным, больше не работают.
    ///
    /// Дубли и заведомо нерабочие адреса (порт 0, `0.0.0.0`,
    /// широковещательный, multicast) отбрасываются, IPv6 в сообщение не
    /// попадает. Каждое поле ограничено [`MAX_ADDRESSES`] адресами. Если адрес
    /// есть и в `fresh`, и в `dropped`, он считается живым и попадает только в
    /// `added`.
    pub fn diff(known: &[SocketAddr], fresh: &[SocketAddr], dropped: &[SocketAddr]) -> PexMessage {
        let known: HashSet<SocketAddr> = known.iter().copied().collect();
        let mut message = PexMessage::default();
        // `added` — то, что реально попало в сообщение: этим же набором
        // проверяем конфликт с `dropped`.
        let mut added_seen: HashSet<SocketAddr> = HashSet::new();

        for addr in fresh {
            if message.added.len() >= MAX_ADDRESSES {
                break;
            }
            if !is_useful(addr) || known.contains(addr) || !added_seen.insert(*addr) {
                continue;
            }
            message.added.push(*addr);
        }

        let mut dropped_seen: HashSet<SocketAddr> = HashSet::new();
        for addr in dropped {
            if message.dropped.len() >= MAX_ADDRESSES {
                break;
            }
            if !is_useful(addr) || added_seen.contains(addr) || !dropped_seen.insert(*addr) {
                continue;
            }
            message.dropped.push(*addr);
        }

        message
    }
}

/// Упаковывает IPv4-адрес в 6 байт: 4 байта IP + 2 байта порта.
///
/// Для IPv6 возвращает `None`: компактный формат PEX описывает только IPv4.
fn compact_addr(addr: &SocketAddr) -> Option<[u8; COMPACT_LEN]> {
    let SocketAddr::V4(v4) = addr else {
        return None;
    };
    let mut out = [0u8; COMPACT_LEN];
    out[..4].copy_from_slice(&v4.ip().octets());
    out[4..].copy_from_slice(&v4.port().to_be_bytes());
    Some(out)
}

/// Годится ли адрес, чтобы его отдавать другим: IPv4 с осмысленным портом.
///
/// Loopback и приватные адреса не отбрасываются: в локальной сети они рабочие.
fn is_useful(addr: &SocketAddr) -> bool {
    let SocketAddr::V4(v4) = addr else {
        return false;
    };
    let ip = v4.ip();
    v4.port() != 0 && !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast()
}

/// Разбирает компактный список адресов из поля bencode-словаря.
fn parse_compact(
    value: Option<&Value>,
    field_name: &'static str,
) -> Result<Vec<SocketAddr>, PexError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Value::Bytes(bytes) = value else {
        return Err(PexError::Malformed(field_name));
    };
    if bytes.len() % COMPACT_LEN != 0 {
        return Err(PexError::Truncated(field_name));
    }

    // Длинный список не отвергаем, а обрезаем: первые [`MAX_ADDRESSES`]
    // адресов ничуть не хуже остальных, а терять из-за них пиров незачем.
    Ok(bytes
        .chunks_exact(COMPACT_LEN)
        .take(MAX_ADDRESSES)
        .map(|chunk| {
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3])),
                u16::from_be_bytes([chunk[4], chunk[5]]),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Адрес из строки — чтобы тесты читались как протокол.
    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("тестовый адрес должен разбираться")
    }

    /// Полезная нагрузка из полей-строк в компактном виде.
    fn compact_payload(fields: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut dict: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
        for (key, bytes) in fields {
            dict.insert(key.as_bytes().to_vec(), Value::Bytes(bytes.clone()));
        }
        bencode::encode(&Value::Dict(dict))
    }

    #[test]
    fn extension_id_matches_contract() {
        assert_eq!(EXT_PEX, 1);
    }

    #[test]
    fn round_trip_keeps_added_and_dropped() {
        let message = PexMessage {
            added: vec![addr("10.0.0.1:6881"), addr("192.168.1.5:51413")],
            dropped: vec![addr("203.0.113.9:80")],
        };
        let payload = message.encode();
        assert_eq!(PexMessage::decode(&payload), Ok(message));
    }

    #[test]
    fn empty_message_is_empty_dict() {
        let message = PexMessage::default();
        assert_eq!(message.encode(), b"de".to_vec());
        assert_eq!(PexMessage::decode(b"de"), Ok(PexMessage::default()));
        // Явно пустые поля — то же самое, что отсутствующие.
        assert_eq!(
            PexMessage::decode(&compact_payload(&[("added", Vec::new())])),
            Ok(PexMessage::default())
        );
    }

    #[test]
    fn encode_writes_zero_flags_for_added() {
        let message = PexMessage {
            added: vec![addr("10.0.0.1:1"), addr("10.0.0.2:2")],
            dropped: Vec::new(),
        };
        let value = bencode::decode_all(&message.encode()).expect("сообщение должно разбираться");
        let flags = value
            .get("added.f")
            .and_then(Value::as_bytes)
            .expect("added.f должен присутствовать");
        assert_eq!(flags, &[0u8, 0u8]);
        assert!(value.get("dropped").is_none());
    }

    #[test]
    fn encode_skips_ipv6_entries() {
        let message = PexMessage {
            added: vec![addr("[2001:db8::1]:6881")],
            dropped: vec![addr("[2001:db8::2]:6881")],
        };
        assert_eq!(message.encode(), b"de".to_vec());
    }

    #[test]
    fn decode_rejects_garbage_without_panic() {
        assert!(matches!(
            PexMessage::decode(b"not bencode"),
            Err(PexError::Bencode(_))
        ));
        assert!(matches!(PexMessage::decode(b""), Err(PexError::Bencode(_))));
        assert!(matches!(
            PexMessage::decode(b"deX"),
            Err(PexError::Bencode(_))
        ));
        assert_eq!(PexMessage::decode(b"4:abcd"), Err(PexError::NotDict));

        let mut dict: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
        dict.insert(b"added".to_vec(), Value::Int(1));
        let payload = bencode::encode(&Value::Dict(dict));
        assert_eq!(
            PexMessage::decode(&payload),
            Err(PexError::Malformed("added"))
        );
    }

    #[test]
    fn decode_rejects_truncated_compact_address() {
        // 5 байт — адрес обрезан на последнем байте порта.
        let payload = compact_payload(&[("added", vec![10, 0, 0, 1, 0x1a])]);
        assert_eq!(
            PexMessage::decode(&payload),
            Err(PexError::Truncated("added"))
        );

        // 7 байт — лишний байт приклеился к следующему адресу.
        let payload = compact_payload(&[("dropped", vec![10, 0, 0, 1, 0x1a, 0xe1, 0x00])]);
        assert_eq!(
            PexMessage::decode(&payload),
            Err(PexError::Truncated("dropped"))
        );
    }

    #[test]
    fn decode_truncates_long_list_instead_of_rejecting() {
        // Так выглядят сообщения libtorrent/qBittorrent: адресов заметно больше
        // лимита, но терять их полностью нельзя — раньше такое сообщение
        // отбрасывалось целиком.
        let mut compact = Vec::new();
        for octet in 1..=201u8 {
            compact.extend_from_slice(&[10, 0, 0, octet, 0x1a, 0xe1]);
        }
        let payload = compact_payload(&[("added", compact)]);
        let message = PexMessage::decode(&payload).expect("длинный список — не ошибка разбора");
        assert_eq!(message.added.len(), MAX_ADDRESSES);
        assert_eq!(message.added[0], addr("10.0.0.1:6881"));
        assert_eq!(message.added[MAX_ADDRESSES - 1], addr("10.0.0.200:6881"));
    }

    #[test]
    fn decode_ignores_ipv6_fields() {
        let payload = compact_payload(&[
            ("added", vec![10, 0, 0, 1, 0x1a, 0xe1]),
            ("added6", vec![0u8; 18]),
            ("dropped6", vec![0u8; 18]),
        ]);
        let message = PexMessage::decode(&payload).expect("IPv6-поля должны игнорироваться");
        assert_eq!(message.added, vec![addr("10.0.0.1:6881")]);
        assert!(message.dropped.is_empty());
    }

    #[test]
    fn decode_ignores_broken_flags_field() {
        let payload = compact_payload(&[
            (
                "added",
                vec![10, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0, 2, 0x1a, 0xe1],
            ),
            // Длина флагов расходится с числом адресов — просто не смотрим.
            ("added.f", vec![0u8]),
        ]);
        let message = PexMessage::decode(&payload).expect("флаги не должны ломать разбор");
        assert_eq!(message.added.len(), 2);
    }

    #[test]
    fn diff_skips_known_and_duplicates() {
        let known = vec![addr("10.0.0.1:1")];
        let fresh = vec![
            addr("10.0.0.1:1"),
            addr("10.0.0.2:2"),
            addr("10.0.0.2:2"),
            addr("10.0.0.3:3"),
        ];
        let message = PexMessage::diff(&known, &fresh, &[]);
        assert_eq!(message.added, vec![addr("10.0.0.2:2"), addr("10.0.0.3:3")]);
        assert!(message.dropped.is_empty());
    }

    #[test]
    fn diff_limits_added_to_max_addresses() {
        let fresh: Vec<SocketAddr> = (1..=205u16)
            .map(|octet| addr(&format!("10.1.0.{octet}:6881")))
            .collect();
        let message = PexMessage::diff(&[], &fresh, &[]);
        assert_eq!(message.added.len(), MAX_ADDRESSES);
        assert_eq!(message.added[0], addr("10.1.0.1:6881"));
        assert_eq!(message.added[MAX_ADDRESSES - 1], addr("10.1.0.200:6881"));
    }

    #[test]
    fn diff_limits_dropped_to_max_addresses() {
        let dropped: Vec<SocketAddr> = (1..=205u16)
            .map(|octet| addr(&format!("10.2.0.{octet}:1")))
            .collect();
        let message = PexMessage::diff(&[], &[], &dropped);
        assert!(message.added.is_empty());
        assert_eq!(message.dropped.len(), MAX_ADDRESSES);
        assert_eq!(message.dropped[MAX_ADDRESSES - 1], addr("10.2.0.200:1"));
    }

    #[test]
    fn diff_skips_unusable_addresses() {
        let fresh = vec![
            addr("0.0.0.0:6881"),
            addr("10.0.0.1:0"),
            addr("255.255.255.255:6881"),
            addr("239.1.2.3:6881"),
            addr("[2001:db8::1]:6881"),
        ];
        let message = PexMessage::diff(&[], &fresh, &fresh);
        assert!(message.added.is_empty());
        assert!(message.dropped.is_empty());
    }

    #[test]
    fn diff_keeps_dropped_separate_from_added() {
        let known = vec![addr("10.0.0.7:7")];
        let fresh = vec![addr("10.0.0.5:5")];
        let dropped = vec![
            // Одновременно «живой» и «мёртвый»: побеждает `added`.
            addr("10.0.0.5:5"),
            addr("10.0.0.7:7"),
            addr("10.0.0.8:8"),
            addr("10.0.0.8:8"),
        ];
        let message = PexMessage::diff(&known, &fresh, &dropped);
        assert_eq!(message.added, vec![addr("10.0.0.5:5")]);
        assert_eq!(
            message.dropped,
            vec![addr("10.0.0.7:7"), addr("10.0.0.8:8")]
        );
    }
}
