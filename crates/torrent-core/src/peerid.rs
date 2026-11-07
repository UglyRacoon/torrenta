//! Идентификатор пира в стиле Azureus (BEP 20): `-TR1000-` + 12 случайных
//! символов. Уникален настолько, насколько это нужно трекеру.

use rand::Rng;

/// Префикс клиента: `TR` = Torrenta, `1000` = версия 1.0.0.
pub const CLIENT_PREFIX: &[u8; 8] = b"-TR1000-";

/// Генерирует 20-байтовый peer id.
pub fn generate_peer_id() -> [u8; 20] {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(CLIENT_PREFIX);
    let mut rng = rand::thread_rng();
    for slot in id[8..].iter_mut() {
        *slot = ALPHABET[rng.gen_range(0..ALPHABET.len())];
    }
    id
}

/// Генерирует peer id как строку (для логов и тестов).
pub fn generate_peer_id_string() -> String {
    String::from_utf8_lossy(&generate_peer_id()).into_owned()
}

/// Человекочитаемое имя клиента по peer id (для списка пиров в UI).
pub fn client_name(peer_id: &[u8; 20]) -> String {
    if peer_id.len() >= 8 && peer_id[0] == b'-' {
        let code = &peer_id[1..3];
        let version = String::from_utf8_lossy(&peer_id[3..7]).to_string();
        let name = match code {
            b"TR" => "Torrenta",
            b"qB" => "qBittorrent",
            b"UT" => "µTorrent",
            b"AZ" => "Azureus",
            b"DE" => "Deluge",
            b"lt" => "libtorrent",
            b"BT" => "BitTorrent",
            b"TX" => "Transmission",
            _ => return String::from("Unknown"),
        };
        return format!("{name} {version}");
    }
    String::from("Unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_id_is_20_bytes_with_client_prefix() {
        let id = generate_peer_id();
        assert_eq!(id.len(), 20);
        assert_eq!(&id[..8], CLIENT_PREFIX);
        assert!(id[8..].iter().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn peer_ids_differ() {
        let a = generate_peer_id();
        let b = generate_peer_id();
        assert_ne!(a, b);
    }

    #[test]
    fn recognizes_known_clients() {
        let mut id = *b"-qB4650-abcdefghijkl";
        assert!(client_name(&id).starts_with("qBittorrent"));
        id[1] = b'X';
        id[2] = b'Y';
        assert_eq!(client_name(&id), "Unknown");
    }
}
