//! Peer wire protocol (BEP 3) и расширения (BEP 10).
//!
//! Каждое сообщение, кроме handshake, упаковано как `u32 BE длина + payload`.
//! `Payload` начинается с байта типа сообщения; пустой payload — keep-alive.

use thiserror::Error;

use crate::bitfield::Bitfield;
use crate::metainfo::InfoHash;

/// Строка протокола в handshake.
pub const PROTOCOL: &[u8] = b"BitTorrent protocol";

/// Длина handshake в байтах.
pub const HANDSHAKE_LEN: usize = 68;

/// Ошибки разбора wire-протокола.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    /// Буфер короче, чем требует сообщение.
    #[error("буфер короче ожидаемого")]
    Short,
    /// В handshake пришла не наша строка протокола.
    #[error("неверная строка протокола в handshake")]
    BadProtocol,
    /// Поле сообщения имеет неверную длину/содержимое.
    #[error("некорректное поле сообщения: {0}")]
    Malformed(&'static str),
    /// Неизвестный id сообщения.
    #[error("неизвестный id сообщения: {0}")]
    UnknownId(u8),
}

/// Handshake: 19 байт протокола + 8 reserved + 20 info-hash + 20 peer-id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    pub info_hash: InfoHash,
    pub peer_id: [u8; 20],
    /// Reserved-байты; `reserved[5] & 0x10` — поддержка BEP 10 (extended).
    pub reserved: [u8; 8],
}

impl Handshake {
    /// Обычный handshake без расширений.
    pub fn new(info_hash: InfoHash, peer_id: [u8; 20]) -> Handshake {
        Handshake {
            info_hash,
            peer_id,
            reserved: [0u8; 8],
        }
    }

    /// Handshake с поддержкой extension protocol (BEP 10).
    pub fn with_extensions(info_hash: InfoHash, peer_id: [u8; 20]) -> Handshake {
        let mut reserved = [0u8; 8];
        reserved[5] |= 0x10;
        Handshake {
            info_hash,
            peer_id,
            reserved,
        }
    }

    pub fn supports_extended(&self) -> bool {
        self.reserved[5] & 0x10 != 0
    }

    /// Сериализует в 68 байт.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HANDSHAKE_LEN);
        out.push(PROTOCOL.len() as u8);
        out.extend_from_slice(PROTOCOL);
        out.extend_from_slice(&self.reserved);
        out.extend_from_slice(self.info_hash.as_bytes());
        out.extend_from_slice(&self.peer_id);
        debug_assert_eq!(out.len(), HANDSHAKE_LEN);
        out
    }

    /// Разбирает 68 байт handshake.
    pub fn parse(buf: &[u8]) -> Result<Handshake, WireError> {
        // Трейлер от последующих сообщений допустим: берём первые 68 байт.
        if buf.len() < HANDSHAKE_LEN {
            return Err(WireError::Short);
        }
        if buf[0] as usize != PROTOCOL.len() || &buf[1..1 + PROTOCOL.len()] != PROTOCOL {
            return Err(WireError::BadProtocol);
        }
        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&buf[20..28]);
        let info_hash =
            InfoHash::from_slice(&buf[28..48]).ok_or(WireError::Malformed("info-hash"))?;
        let mut peer_id = [0u8; 20];
        peer_id.copy_from_slice(&buf[48..68]);
        Ok(Handshake {
            info_hash,
            peer_id,
            reserved,
        })
    }
}

/// Сообщение peer wire protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Bitfield),
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        index: u32,
        begin: u32,
        data: Vec<u8>,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    Port(u16),
    /// Сообщение extension protocol (BEP 10).
    Extended {
        ext_id: u8,
        payload: Vec<u8>,
    },
}

impl Message {
    /// Полное сообщение с 4-байтовым префиксом длины.
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.encode_payload();
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Payload без префикса длины (для keep-alive — пустой).
    fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Message::KeepAlive => {}
            Message::Choke => out.push(0),
            Message::Unchoke => out.push(1),
            Message::Interested => out.push(2),
            Message::NotInterested => out.push(3),
            Message::Have(index) => {
                out.push(4);
                out.extend_from_slice(&index.to_be_bytes());
            }
            Message::Bitfield(bitfield) => {
                out.push(5);
                out.extend_from_slice(bitfield.as_bytes());
            }
            Message::Request {
                index,
                begin,
                length,
            }
            | Message::Cancel {
                index,
                begin,
                length,
            } => {
                out.push(if matches!(self, Message::Request { .. }) {
                    6
                } else {
                    8
                });
                out.extend_from_slice(&index.to_be_bytes());
                out.extend_from_slice(&begin.to_be_bytes());
                out.extend_from_slice(&length.to_be_bytes());
            }
            Message::Piece { index, begin, data } => {
                out.push(7);
                out.extend_from_slice(&index.to_be_bytes());
                out.extend_from_slice(&begin.to_be_bytes());
                out.extend_from_slice(data);
            }
            Message::Port(port) => {
                out.push(9);
                out.extend_from_slice(&port.to_be_bytes());
            }
            Message::Extended { ext_id, payload } => {
                out.push(20);
                out.push(*ext_id);
                out.extend_from_slice(payload);
            }
        }
        out
    }

    /// Тип сообщения (id) для расшифровки в логах.
    pub fn kind(&self) -> &'static str {
        match self {
            Message::KeepAlive => "keep-alive",
            Message::Choke => "choke",
            Message::Unchoke => "unchoke",
            Message::Interested => "interested",
            Message::NotInterested => "not-interested",
            Message::Have(_) => "have",
            Message::Bitfield(_) => "bitfield",
            Message::Request { .. } => "request",
            Message::Piece { .. } => "piece",
            Message::Cancel { .. } => "cancel",
            Message::Port(_) => "port",
            Message::Extended { .. } => "extended",
        }
    }
}

/// Разбирает payload без префикса длины (пустой payload — keep-alive).
pub fn decode(payload: &[u8]) -> Result<Message, WireError> {
    let Some(&id) = payload.first() else {
        return Ok(Message::KeepAlive);
    };
    let body = &payload[1..];
    match id {
        0 => {
            expect_empty(body, "choke")?;
            Ok(Message::Choke)
        }
        1 => {
            expect_empty(body, "unchoke")?;
            Ok(Message::Unchoke)
        }
        2 => {
            expect_empty(body, "interested")?;
            Ok(Message::Interested)
        }
        3 => {
            expect_empty(body, "not-interested")?;
            Ok(Message::NotInterested)
        }
        4 => {
            let index = read_u32(body, 0, "have")?;
            if body.len() != 4 {
                return Err(WireError::Malformed("have"));
            }
            Ok(Message::Have(index))
        }
        5 => Ok(Message::Bitfield(Bitfield::from_bytes(
            body.to_vec(),
            body.len() * 8,
        ))),
        6 => Ok(Message::Request {
            index: read_u32(body, 0, "request")?,
            begin: read_u32(body, 4, "request")?,
            length: read_u32(body, 8, "request")?,
        }),
        7 => {
            let index = read_u32(body, 0, "piece")?;
            let begin = read_u32(body, 4, "piece")?;
            Ok(Message::Piece {
                index,
                begin,
                data: body[8..].to_vec(),
            })
        }
        8 => Ok(Message::Cancel {
            index: read_u32(body, 0, "cancel")?,
            begin: read_u32(body, 4, "cancel")?,
            length: read_u32(body, 8, "cancel")?,
        }),
        9 => {
            if body.len() < 2 {
                return Err(WireError::Short);
            }
            if body.len() != 2 {
                return Err(WireError::Malformed("port"));
            }
            Ok(Message::Port(u16::from_be_bytes([body[0], body[1]])))
        }
        20 => {
            let Some(&ext_id) = body.first() else {
                return Err(WireError::Short);
            };
            Ok(Message::Extended {
                ext_id,
                payload: body[1..].to_vec(),
            })
        }
        other => Err(WireError::UnknownId(other)),
    }
}

fn expect_empty(body: &[u8], name: &'static str) -> Result<(), WireError> {
    if body.is_empty() {
        Ok(())
    } else {
        Err(WireError::Malformed(name))
    }
}

fn read_u32(body: &[u8], offset: usize, name: &'static str) -> Result<u32, WireError> {
    let bytes = body.get(offset..offset + 4).ok_or(WireError::Short)?;
    let _ = name;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> InfoHash {
        InfoHash::new([byte; 20])
    }

    fn peer_id() -> [u8; 20] {
        *b"-TR0001-abcdefghijkl"
    }

    /// Проверяет префикс длины и roundtrip через `decode`.
    fn roundtrip(message: Message) {
        let encoded = message.encode();
        assert!(encoded.len() >= 4);
        let declared = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(declared as usize, encoded.len() - 4, "{message:?}");
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn handshake_is_exactly_68_bytes() {
        let hs = Handshake::new(hash(0xAB), peer_id());
        let encoded = hs.encode();
        assert_eq!(encoded.len(), HANDSHAKE_LEN);
        assert_eq!(encoded[0], 19);
        assert_eq!(&encoded[1..20], PROTOCOL);
        assert_eq!(&encoded[28..48], hash(0xAB).as_bytes());
        assert_eq!(&encoded[48..68], &peer_id());
        assert!(!hs.supports_extended());
        assert_eq!(Handshake::parse(&encoded).unwrap(), hs);
    }

    #[test]
    fn handshake_extended_flag() {
        let hs = Handshake::with_extensions(hash(1), peer_id());
        assert!(hs.supports_extended());
        let encoded = hs.encode();
        assert_eq!(encoded[25] & 0x10, 0x10);
        let parsed = Handshake::parse(&encoded).unwrap();
        assert!(parsed.supports_extended());
        assert_eq!(parsed.reserved, hs.reserved);
    }

    #[test]
    fn handshake_rejects_bad_protocol_and_short() {
        let encoded = Handshake::new(hash(2), peer_id()).encode();
        // Обрыв на любом префиксе — Short, не паника.
        for cut in 0..HANDSHAKE_LEN {
            assert_eq!(
                Handshake::parse(&encoded[..cut]).unwrap_err(),
                WireError::Short
            );
        }
        // Другая строка протокола.
        let mut bad = encoded.clone();
        bad[1] = b'X';
        assert_eq!(Handshake::parse(&bad).unwrap_err(), WireError::BadProtocol);
        // Верная длина, но неверная длина строки протокола.
        let mut bad_len = encoded.clone();
        bad_len[0] = 18;
        assert_eq!(
            Handshake::parse(&bad_len).unwrap_err(),
            WireError::BadProtocol
        );
        // Трейлер после handshake допустим (дальше идут сообщения).
        let mut with_tail = encoded.clone();
        with_tail.extend_from_slice(&Message::Interested.encode());
        assert_eq!(
            Handshake::parse(&with_tail).unwrap().info_hash,
            hash(2)
        );
    }

    #[test]
    fn keep_alive_is_four_zero_bytes() {
        let encoded = Message::KeepAlive.encode();
        assert_eq!(encoded, vec![0, 0, 0, 0]);
        assert_eq!(decode(&encoded[4..]).unwrap(), Message::KeepAlive);
    }

    #[test]
    fn roundtrip_all_simple_messages() {
        roundtrip(Message::KeepAlive);
        roundtrip(Message::Choke);
        roundtrip(Message::Unchoke);
        roundtrip(Message::Interested);
        roundtrip(Message::NotInterested);
        roundtrip(Message::Have(777));
        roundtrip(Message::Port(6881));
    }

    #[test]
    fn roundtrip_block_messages() {
        roundtrip(Message::Request {
            index: 1,
            begin: 16384,
            length: 16384,
        });
        roundtrip(Message::Cancel {
            index: 2,
            begin: 0,
            length: 32768,
        });
        roundtrip(Message::Piece {
            index: 3,
            begin: 4096,
            data: (0..=255u8).collect(),
        });
        // Пустой блок — тоже валидное сообщение.
        roundtrip(Message::Piece {
            index: 0,
            begin: 0,
            data: Vec::new(),
        });
    }

    #[test]
    fn roundtrip_bitfield_and_extended() {
        let bitfield = Bitfield::from_bytes(vec![0b1010_1010, 0xFF], 16);
        roundtrip(Message::Bitfield(bitfield));
        roundtrip(Message::Bitfield(Bitfield::new(8)));
        roundtrip(Message::Extended {
            ext_id: 0,
            payload: b"d1:md11:ut_metadatai1eee".to_vec(),
        });
        roundtrip(Message::Extended {
            ext_id: 1,
            payload: Vec::new(),
        });
        // Байты bitfield в сообщении идут как есть (MSB-first).
        let encoded = Message::Bitfield(Bitfield::from_bytes(vec![0x80, 0x40], 16)).encode();
        assert_eq!(&encoded[4..], &[5u8, 0x80, 0x40]);
    }

    #[test]
    fn decode_rejects_unknown_and_malformed() {
        assert_eq!(decode(&[99]).unwrap_err(), WireError::UnknownId(99));
        // choke с лишним байтом.
        assert_eq!(
            decode(&[0, 0]).unwrap_err(),
            WireError::Malformed("choke")
        );
        // have короче 5 байт.
        assert_eq!(decode(&[4, 0, 0]).unwrap_err(), WireError::Short);
        // request короче 13 байт.
        assert_eq!(
            decode(&[6, 0, 0, 0, 1, 0, 0]).unwrap_err(),
            WireError::Short
        );
        // piece без заголовка.
        assert_eq!(decode(&[7, 0, 0, 0]).unwrap_err(), WireError::Short);
        // port ровно 3 байта обязателен.
        assert_eq!(decode(&[9, 0x1a]).unwrap_err(), WireError::Short);
        assert_eq!(decode(&[9, 0x1a, 0xe1, 0x00]).unwrap_err(), WireError::Malformed("port"));
        // extended без ext_id.
        assert_eq!(decode(&[20]).unwrap_err(), WireError::Short);
    }

    #[test]
    fn decode_matches_length_prefix_from_encode() {
        let messages = vec![
            Message::KeepAlive,
            Message::Have(0),
            Message::Request {
                index: 9,
                begin: 9,
                length: 9,
            },
            Message::Piece {
                index: 1,
                begin: 2,
                data: vec![3, 4, 5],
            },
            Message::Port(1),
            Message::Extended {
                ext_id: 3,
                payload: vec![7],
            },
        ];
        let mut stream = Vec::new();
        for message in &messages {
            stream.extend_from_slice(&message.encode());
        }
        // Последовательное чтение потока: длина → payload → decode.
        let mut pos = 0usize;
        for expected in &messages {
            let len = u32::from_be_bytes([
                stream[pos],
                stream[pos + 1],
                stream[pos + 2],
                stream[pos + 3],
            ]) as usize;
            pos += 4;
            let decoded = decode(&stream[pos..pos + len]).unwrap();
            pos += len;
            assert_eq!(&decoded, expected);
        }
        assert_eq!(pos, stream.len());
    }

    #[test]
    fn bitfield_payload_len_rounds_to_full_bytes() {
        // Пир может прислать bitfield с лишними хвостовыми битами: длина
        // восстанавливается как bytes*8, а хвост нормализуется в ноль.
        let decoded = decode(&[5, 0xFF, 0xF0]).unwrap();
        match decoded {
            Message::Bitfield(bf) => {
                assert_eq!(bf.len(), 16);
                assert_eq!(bf.as_bytes(), &[0xFF, 0xF0]);
                assert_eq!(bf.count(), 12);
            }
            other => panic!("ожидался Bitfield, получено {other:?}"),
        }
    }

    #[test]
    fn message_kind_names() {
        assert_eq!(Message::KeepAlive.kind(), "keep-alive");
        assert_eq!(Message::Have(1).kind(), "have");
        assert_eq!(
            Message::Piece {
                index: 0,
                begin: 0,
                data: vec![]
            }
            .kind(),
            "piece"
        );
        assert_eq!(
            Message::Extended {
                ext_id: 0,
                payload: vec![]
            }
            .kind(),
            "extended"
        );
    }
}
