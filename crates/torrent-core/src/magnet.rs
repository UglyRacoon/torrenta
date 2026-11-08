//! Разбор magnet-ссылок (BEP 9 / BEP 53 / BEP 10).
//!
//! Поддерживаемая форма:
//! `magnet:?xt=urn:btih:<40 hex | 32 base32>&dn=<имя>&tr=<трекер>&tr=<трекер>`

use percent_encoding::percent_decode;
use thiserror::Error;

use crate::metainfo::InfoHash;

/// Разобранная magnet-ссылка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magnet {
    pub info_hash: InfoHash,
    /// `dn` — отображаемое имя (не доверять: только для UI).
    pub display_name: Option<String>,
    /// `tr` — трекеры из ссылки.
    pub trackers: Vec<String>,
}

/// Ошибки разбора magnet-ссылки.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MagnetError {
    #[error("ссылка не является magnet-ссылкой")]
    NotMagnet,
    #[error("в ссылке нет xt=urn:btih")]
    MissingInfoHash,
    #[error("некорректный info-hash: {0}")]
    BadInfoHash(String),
}

/// Префикс magnet-ссылки.
const MAGNET_PREFIX: &str = "magnet:?";
/// Префикс info-hash в `xt`.
const BTIH_PREFIX: &str = "urn:btih:";

/// Быстрая проверка без полного разбора.
pub fn is_magnet(link: &str) -> bool {
    link.trim_start()
        .to_ascii_lowercase()
        .starts_with("magnet:?")
}

/// Разбирает magnet-ссылку.
pub fn parse(link: &str) -> Result<Magnet, MagnetError> {
    let link = link.trim();
    if !is_magnet(link) {
        return Err(MagnetError::NotMagnet);
    }
    let query = &link[MAGNET_PREFIX.len()..];

    let mut info_hash: Option<InfoHash> = None;
    let mut bad_hash: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut trackers: Vec<String> = Vec::new();

    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            // Точный идентификатор: xt=urn:btih:<hex|base32>.
            "xt" => {
                let decoded = decode_component(value);
                let text = String::from_utf8_lossy(&decoded);
                let Some(rest) = strip_btih(&text) else {
                    continue;
                };
                let parsed = match rest.len() {
                    40 => InfoHash::from_hex(rest),
                    32 => from_base32(rest),
                    _ => None,
                };
                match parsed {
                    // Первый валидный xt выигрывает.
                    Some(hash) if info_hash.is_none() => info_hash = Some(hash),
                    Some(_) => {}
                    None => {
                        if bad_hash.is_none() {
                            bad_hash = Some(rest.to_string());
                        }
                    }
                }
            }
            // Отображаемое имя: только для UI, доверять нельзя.
            "dn" => {
                let decoded = decode_component(value);
                let text = String::from_utf8_lossy(&decoded).trim().to_string();
                if !text.is_empty() && display_name.is_none() {
                    display_name = Some(text);
                }
            }
            // Трекеры: без схемы или с неизвестной схемой — мусор, отбрасываем.
            "tr" => {
                let decoded = decode_component(value);
                let text = String::from_utf8_lossy(&decoded).trim().to_string();
                if is_tracker_url(&text) && !trackers.iter().any(|t| t == &text) {
                    trackers.push(text);
                }
            }
            _ => {}
        }
    }

    match (info_hash, bad_hash) {
        (Some(info_hash), _) => Ok(Magnet {
            info_hash,
            display_name,
            trackers,
        }),
        (None, Some(bad)) => Err(MagnetError::BadInfoHash(bad)),
        (None, None) => Err(MagnetError::MissingInfoHash),
    }
}

/// Декодирует percent-encoding компонента query-строки (`+` — пробел).
fn decode_component(value: &str) -> Vec<u8> {
    // Сначала `+` → пробел (magnet-ссылки следуют form-urlencoded), затем %XX.
    let mut raw = Vec::with_capacity(value.len());
    for ch in value.chars() {
        if ch == '+' {
            raw.push(b' ');
        } else {
            let mut buf = [0u8; 4];
            raw.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
    percent_decode(&raw).collect()
}

/// Отрезает регистронезависимый префикс `urn:btih:`.
fn strip_btih(text: &str) -> Option<&str> {
    let prefix = text.get(..BTIH_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(BTIH_PREFIX) {
        return None;
    }
    let rest = text.get(BTIH_PREFIX.len()..)?.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// Похоже ли значение `tr` на URL трекера.
fn is_tracker_url(text: &str) -> bool {
    let Ok(url) = url::Url::parse(text) else {
        return false;
    };
    if url.host_str().map(str::is_empty).unwrap_or(true) {
        return false;
    }
    matches!(url.scheme(), "http" | "https" | "udp" | "ws" | "wss")
}

/// Кодирует info-hash в base32 (без паддинга) — используется magnet-ссылками.
pub fn to_base32(info_hash: &InfoHash) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let bytes = info_hash.as_bytes();
    let mut out = String::with_capacity(32);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in bytes.iter() {
        buffer = (buffer << 8) | *byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1F) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1F) as usize] as char);
    }
    out
}

/// Декодирует base32 (без паддинга) в info-hash.
pub fn from_base32(s: &str) -> Option<InfoHash> {
    let mut out = [0u8; 20];
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    let mut index = 0usize;
    for ch in s.chars() {
        let val = match ch.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u32 - 'A' as u32,
            c @ '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | val;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            if index >= 20 {
                return None;
            }
            out[index] = ((buffer >> bits) & 0xFF) as u8;
            index += 1;
        }
    }
    if index == 20 {
        Some(InfoHash(out))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef01234567";

    fn hex_hash() -> InfoHash {
        InfoHash::from_hex(HEX).unwrap()
    }

    #[test]
    fn parses_hex_magnet_with_name_and_trackers() {
        let link = format!(
            "magnet:?xt=urn:btih:{HEX}&dn=Ubuntu+24.04&tr=udp%3A%2F%2Ftracker.example%3A6969%2Fannounce&tr=http%3A%2F%2Ft2.example%2Fannounce"
        );
        let magnet = parse(&link).unwrap();
        assert_eq!(magnet.info_hash, hex_hash());
        // `+` — пробел, `%3A`/`%2F` раскодированы.
        assert_eq!(magnet.display_name.as_deref(), Some("Ubuntu 24.04"));
        assert_eq!(
            magnet.trackers,
            vec![
                "udp://tracker.example:6969/announce".to_string(),
                "http://t2.example/announce".to_string(),
            ]
        );
    }

    #[test]
    fn parses_base32_magnet_both_cases() {
        let base32 = to_base32(&hex_hash());
        assert_eq!(base32.len(), 32);
        for variant in [base32.clone(), base32.to_ascii_lowercase()] {
            let magnet = parse(&format!("magnet:?xt=urn:btih:{variant}")).unwrap();
            assert_eq!(magnet.info_hash, hex_hash());
            assert!(magnet.display_name.is_none());
            assert!(magnet.trackers.is_empty());
        }
    }

    #[test]
    fn base32_known_vectors() {
        assert_eq!(to_base32(&InfoHash::new([0u8; 20])), "A".repeat(32));
        assert_eq!(to_base32(&InfoHash::new([0xFFu8; 20])), "7".repeat(32));
        let hash = hex_hash();
        assert_eq!(from_base32(&to_base32(&hash)), Some(hash));
        assert_eq!(from_base32(""), None);
        assert_eq!(from_base32("A"), None);
        assert_eq!(from_base32(&"1".repeat(32)), None);
    }

    #[test]
    fn scheme_and_keys_are_case_insensitive() {
        let link = format!("  Magnet:?XT=URN:BTIH:{HEX}&DN=Name  ");
        let magnet = parse(&link).unwrap();
        assert_eq!(magnet.info_hash, hex_hash());
        assert_eq!(magnet.display_name.as_deref(), Some("Name"));
    }

    #[test]
    fn drops_garbage_trackers_and_dedupes() {
        let link = format!(
            "magnet:?xt=urn:btih:{HEX}&tr=tracker.example/announce&tr=&tr=http://t.example/a&tr=http://t.example/a&tr=ftp://bad/a"
        );
        let magnet = parse(&link).unwrap();
        assert_eq!(magnet.trackers, vec!["http://t.example/a".to_string()]);
    }

    #[test]
    fn decodes_cyrillic_and_utf8_percent_encoding() {
        let link = format!(
            "magnet:?xt=urn:btih:{HEX}&dn=%D0%A2%D0%BE%D1%80%D1%80%D0%B5%D0%BD%D1%82&tr=udp%3A%2F%2Ft.example%3A80%2Fannounce"
        );
        let magnet = parse(&link).unwrap();
        assert_eq!(magnet.display_name.as_deref(), Some("Торрент"));
        assert_eq!(
            magnet.trackers,
            vec!["udp://t.example:80/announce".to_string()]
        );
    }

    #[test]
    fn rejects_non_magnet_and_missing_hash() {
        assert_eq!(
            parse("http://example.com/x.torrent").unwrap_err(),
            MagnetError::NotMagnet
        );
        assert_eq!(parse("magnet:?").unwrap_err(), MagnetError::MissingInfoHash);
        assert_eq!(
            parse("magnet:?dn=name&tr=http%3A%2F%2Ft.example%2Fa").unwrap_err(),
            MagnetError::MissingInfoHash
        );
        // Чужой xt (не btih) — как отсутствующий info-hash.
        assert_eq!(
            parse("magnet:?xt=urn:sha1:0123456789abcdef0123456789abcdef01234567").unwrap_err(),
            MagnetError::MissingInfoHash
        );
    }

    #[test]
    fn rejects_bad_hash_length_or_alphabet() {
        assert_eq!(
            parse("magnet:?xt=urn:btih:xyz").unwrap_err(),
            MagnetError::BadInfoHash("xyz".to_string())
        );
        // 40 символов, но не hex.
        let bad_hex = "z".repeat(40);
        assert_eq!(
            parse(&format!("magnet:?xt=urn:btih:{bad_hex}")).unwrap_err(),
            MagnetError::BadInfoHash(bad_hex)
        );
        // 32 символа, но '1' не входит в base32-алфавит.
        let bad_base32 = "1".repeat(32);
        assert_eq!(
            parse(&format!("magnet:?xt=urn:btih:{bad_base32}")).unwrap_err(),
            MagnetError::BadInfoHash(bad_base32)
        );
    }

    #[test]
    fn first_valid_xt_wins_and_empty_btih_is_skipped() {
        let other = "ffffffffffffffffffffffffffffffffffffffff";
        let link = format!("magnet:?xt=urn:btih:&xt=urn:btih:{other}&xt=urn:btih:{HEX}");
        let magnet = parse(&link).unwrap();
        // Пустой xt пропускается, приоритет у первого валидного.
        assert_eq!(magnet.info_hash, InfoHash::from_hex(other).unwrap());
    }

    #[test]
    fn is_magnet_is_fast_and_forgiving() {
        assert!(is_magnet("magnet:?xt=urn:btih:x"));
        assert!(is_magnet("  MAGNET:?xt=x"));
        assert!(!is_magnet("magnet:xt=x"));
        assert!(!is_magnet("http://x"));
    }
}
