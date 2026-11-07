//! Разбор `.torrent` (метаинформация, BEP 3) и идентификатор торрента.
//!
//! # Схема
//!
//! ```text
//! .torrent {
//!   announce: "http://tracker/announce"
//!   announce-list: [["udp://..."], ...]
//!   info: {
//!     name: "ubuntu.iso"
//!     piece length: 262144
//!     pieces: <20-байтовые SHA-1 подряд>
//!     length: 12345            # single-file
//!     files: [{length, path:[...]}]  # multi-file
//!   }
//! }
//! ```
//!
//! info-hash — SHA-1 от bencode-словаря `info`, именно эти сырые байты служат
//! идентификатором торрента в сети.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use thiserror::Error;

use crate::bencode::{self, BencodeError, Value};

/// Идентификатор торрента — SHA-1 (20 байт).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InfoHash(pub [u8; 20]);

impl InfoHash {
    pub const LEN: usize = 20;

    pub fn new(bytes: [u8; 20]) -> InfoHash {
        InfoHash(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Option<InfoHash> {
        if bytes.len() != Self::LEN {
            return None;
        }
        let mut buf = [0u8; Self::LEN];
        buf.copy_from_slice(bytes);
        Some(InfoHash(buf))
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Нижний регистр hex (40 символов).
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(40);
        for byte in self.0.iter() {
            s.push_str(&format!("{byte:02x}"));
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<InfoHash> {
        if s.len() != 40 {
            return None;
        }
        let mut out = [0u8; 20];
        for i in 0..20 {
            out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(InfoHash(out))
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHash({})", self.to_hex())
    }
}

/// Файл торрента в глобальном потоке данных.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Относительный путь ("dir/file.bin" для multi-file, имя для single-file).
    pub path: String,
    pub length: u64,
    /// Смещение начала файла в общем потоке торрента.
    pub offset: u64,
}

/// Разобранный `info`-словарь торрента.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub name: String,
    /// Размер куска в байтах.
    pub piece_length: u32,
    /// SHA-1 каждого куска.
    pub pieces: Vec<InfoHash>,
    /// Файлы торрента (single-file — один элемент).
    pub files: Vec<FileEntry>,
    /// Суммарный размер.
    pub total_length: u64,
    /// Приватный торрент (BEP 27) — DHT запрещён.
    pub private: bool,
}

impl Info {
    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    /// Размер куска `index` (последний кусок может быть короче).
    pub fn piece_size(&self, index: usize) -> u32 {
        if self.piece_length == 0 || index >= self.pieces.len() {
            return 0;
        }
        let offset = index as u64 * self.piece_length as u64;
        if offset >= self.total_length {
            return 0;
        }
        let remaining = self.total_length - offset;
        remaining.min(self.piece_length as u64) as u32
    }

    /// Смещение куска в потоке торрента.
    pub fn piece_offset(&self, index: usize) -> u64 {
        index as u64 * self.piece_length as u64
    }
}

/// Полная метаинформация торрента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metainfo {
    pub announce: Option<String>,
    pub announce_list: Vec<String>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub creation_date: Option<i64>,
    pub info: Info,
    /// SHA-1 от сырых байтов `info`.
    pub info_hash: InfoHash,
    /// Сырые байты bencode-словаря `info` (нужны для magnet/metadata и DHT).
    pub raw_info: Vec<u8>,
}

/// Ошибки разбора метаинформации.
#[derive(Debug, Error)]
pub enum MetainfoError {
    #[error("ошибка bencode: {0}")]
    Bencode(#[from] BencodeError),
    #[error("в файле нет словаря info")]
    MissingInfo,
    #[error("некорректное поле info: {0}")]
    InvalidInfo(&'static str),
    #[error("у торрента нет имени")]
    MissingName,
    #[error("некорректный info-hash")]
    InvalidInfoHash,
}

impl Metainfo {
    /// Разбирает полный `.torrent` файл.
    pub fn parse(data: &[u8]) -> Result<Metainfo, MetainfoError> {
        let value = bencode::decode_all(data).map_err(MetainfoError::Bencode)?;
        let dict = value
            .as_dict()
            .ok_or(MetainfoError::Bencode(BencodeError::NotADict))?;

        // Сырые байты info-словаря: находим его точные границы в исходном буфере,
        // чтобы SHA-1 считался от канонических байтов, а не от пересобранных.
        let spans = top_level_spans(data)?;
        let (start, end) = spans
            .get(b"info".as_slice())
            .copied()
            .ok_or(MetainfoError::MissingInfo)?;
        let raw_info = data[start..end].to_vec();

        let info = parse_info(&raw_info, None)?;
        let info_hash = sha1_of(&raw_info);

        Ok(Metainfo {
            announce: dict_str(dict, "announce").filter(|s| !s.is_empty()),
            announce_list: announce_list_of(dict),
            comment: dict_str(dict, "comment"),
            created_by: dict_str(dict, "created by"),
            creation_date: dict.get(b"creation date".as_slice()).and_then(Value::as_int),
            info,
            info_hash,
            raw_info,
        })
    }

    /// Собирает метаинформацию из сырых байтов `info` (magnet + ut_metadata).
    pub fn from_raw_info(
        raw_info: &[u8],
        announce_list: Vec<String>,
        name_hint: Option<&str>,
    ) -> Result<Metainfo, MetainfoError> {
        let info = parse_info(raw_info, name_hint)?;
        let info_hash = sha1_of(raw_info);
        Ok(Metainfo {
            announce: None,
            announce_list,
            comment: None,
            created_by: None,
            creation_date: None,
            info,
            info_hash,
            raw_info: raw_info.to_vec(),
        })
    }

    /// Все трекеры: основной + announce-list без дублей.
    pub fn trackers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(a) = &self.announce {
            if !a.is_empty() {
                out.push(a.clone());
            }
        }
        for t in &self.announce_list {
            if !t.is_empty() && !out.iter().any(|x| x == t) {
                out.push(t.clone());
            }
        }
        out
    }

    pub fn total_length(&self) -> u64 {
        self.info.total_length
    }

    pub fn piece_count(&self) -> usize {
        self.info.pieces.len()
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }
}

/// SHA-1 от произвольных байтов.
fn sha1_of(data: &[u8]) -> InfoHash {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut bytes = [0u8; InfoHash::LEN];
    bytes.copy_from_slice(&digest);
    InfoHash::new(bytes)
}

/// Байтовая строка как `String` (невалидный UTF-8 заменяется, а не теряется).
fn value_str(value: &Value) -> Option<String> {
    value
        .as_bytes()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

fn dict_str(dict: &BTreeMap<Vec<u8>, Value>, key: &str) -> Option<String> {
    dict.get(key.as_bytes()).and_then(value_str)
}

/// Строка из словаря; пустые значения отбрасываются.
fn dict_nonempty_str(dict: &BTreeMap<Vec<u8>, Value>, key: &str) -> Option<String> {
    dict_str(dict, key).filter(|s| !s.is_empty())
}

/// Границы значений верхнего уровня: `ключ → (начало, конец)` в исходном буфере.
///
/// Верхний уровень bencode — словарь, поэтому его можно обойти без рекурсии:
/// каждое значение разбирается `bencode::decode`, который сообщает, сколько
/// байтов он занял.
fn top_level_spans(data: &[u8]) -> Result<BTreeMap<Vec<u8>, (usize, usize)>, MetainfoError> {
    if data.first() != Some(&b'd') {
        return Err(MetainfoError::Bencode(BencodeError::NotADict));
    }
    let mut pos = 1usize;
    let mut spans = BTreeMap::new();
    loop {
        match data.get(pos) {
            None => return Err(MetainfoError::Bencode(BencodeError::UnexpectedEof)),
            Some(b'e') => return Ok(spans),
            Some(byte) if byte.is_ascii_digit() => {
                let (key_value, key_len) =
                    bencode::decode(&data[pos..]).map_err(MetainfoError::Bencode)?;
                let key = match key_value {
                    Value::Bytes(key) => key,
                    _ => return Err(MetainfoError::Bencode(BencodeError::NotADict)),
                };
                pos += key_len;
                let start = pos;
                let (_, value_len) = bencode::decode(&data[pos..]).map_err(MetainfoError::Bencode)?;
                pos += value_len;
                spans.insert(key, (start, pos));
            }
            Some(other) => {
                return Err(MetainfoError::Bencode(BencodeError::InvalidByte(
                    *other, pos,
                )))
            }
        }
    }
}

/// Список трекеров из `announce-list`. По BEP 12 это список списков, но
/// встречаются и плоские списки строк — поддерживаем обе формы.
fn announce_list_of(dict: &BTreeMap<Vec<u8>, Value>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(value) = dict.get(b"announce-list".as_slice()) else {
        return out;
    };
    let Some(items) = value.as_list() else {
        return out;
    };
    for item in items {
        match item {
            Value::List(nested) => {
                for tracker in nested {
                    if let Some(s) = value_str(tracker).filter(|s| !s.is_empty()) {
                        out.push(s);
                    }
                }
            }
            _ => {
                if let Some(s) = value_str(item).filter(|s| !s.is_empty()) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Разбирает `info`-словарь из его сырых байтов.
fn parse_info(raw_info: &[u8], name_hint: Option<&str>) -> Result<Info, MetainfoError> {
    let value = bencode::decode_all(raw_info).map_err(MetainfoError::Bencode)?;
    let dict = value
        .as_dict()
        .ok_or(MetainfoError::Bencode(BencodeError::NotADict))?;

    let name = dict_nonempty_str(dict, "name.utf-8")
        .or_else(|| dict_nonempty_str(dict, "name"))
        .or_else(|| name_hint.filter(|s| !s.is_empty()).map(str::to_owned))
        .ok_or(MetainfoError::MissingName)?;

    let piece_length = dict
        .get(b"piece length".as_slice())
        .and_then(Value::as_int)
        .ok_or(MetainfoError::InvalidInfo("piece length"))?;
    if piece_length <= 0 || piece_length > u32::MAX as i64 {
        return Err(MetainfoError::InvalidInfo("piece length"));
    }
    let piece_length = piece_length as u32;

    let pieces_raw = dict
        .get(b"pieces".as_slice())
        .and_then(Value::as_bytes)
        .ok_or(MetainfoError::InvalidInfo("pieces"))?;
    if pieces_raw.len() % InfoHash::LEN != 0 {
        return Err(MetainfoError::InvalidInfo("pieces"));
    }
    let pieces = pieces_raw
        .chunks_exact(InfoHash::LEN)
        .map(|chunk| {
            let mut bytes = [0u8; InfoHash::LEN];
            bytes.copy_from_slice(chunk);
            InfoHash::new(bytes)
        })
        .collect();

    let private = dict.get(b"private".as_slice()).and_then(Value::as_int) == Some(1);

    let (files, total_length) = if let Some(files_value) = dict.get(b"files".as_slice()) {
        let entries = files_value
            .as_list()
            .ok_or(MetainfoError::InvalidInfo("files"))?;
        let mut files = Vec::with_capacity(entries.len());
        let mut offset: u64 = 0;
        for entry in entries {
            let file_dict = entry
                .as_dict()
                .ok_or(MetainfoError::InvalidInfo("files"))?;
            let length = file_dict
                .get(b"length".as_slice())
                .and_then(Value::as_int)
                .filter(|v| *v >= 0)
                .ok_or(MetainfoError::InvalidInfo("file length"))?
                as u64;
            let path = file_path(file_dict)?;
            files.push(FileEntry {
                path,
                length,
                offset,
            });
            offset = offset
                .checked_add(length)
                .ok_or(MetainfoError::InvalidInfo("files"))?;
        }
        (files, offset)
    } else {
        let length = dict
            .get(b"length".as_slice())
            .and_then(Value::as_int)
            .filter(|v| *v >= 0)
            .ok_or(MetainfoError::InvalidInfo("length"))? as u64;
        (
            vec![FileEntry {
                path: name.clone(),
                length,
                offset: 0,
            }],
            length,
        )
    };

    Ok(Info {
        name,
        piece_length,
        pieces,
        files,
        total_length,
        private,
    })
}

/// Путь multi-file торрента: компоненты `path`/`path.utf-8` через `/`.
fn file_path(dict: &BTreeMap<Vec<u8>, Value>) -> Result<String, MetainfoError> {
    let components = dict
        .get(b"path.utf-8".as_slice())
        .or_else(|| dict.get(b"path".as_slice()))
        .and_then(Value::as_list)
        .ok_or(MetainfoError::InvalidInfo("file path"))?;
    if components.is_empty() {
        return Err(MetainfoError::InvalidInfo("file path"));
    }
    let mut path = String::new();
    for component in components {
        let part = value_str(component).ok_or(MetainfoError::InvalidInfo("file path"))?;
        if part.is_empty() {
            return Err(MetainfoError::InvalidInfo("file path"));
        }
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(&part);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIECE_LEN: u32 = 256 * 1024;

    fn piece_hashes(count: usize, seed: u8) -> Vec<u8> {
        let mut pieces = Vec::with_capacity(count * 20);
        for i in 0..count {
            pieces.extend(std::iter::repeat(seed.wrapping_add(i as u8)).take(20));
        }
        pieces
    }

    fn single_file_info() -> Value {
        Value::Dict(BTreeMap::from([
            (b"length".to_vec(), Value::int(700_000)),
            (b"name".to_vec(), Value::string("ubuntu.iso")),
            (
                b"piece length".to_vec(),
                Value::int(PIECE_LEN as i64),
            ),
            (
                b"pieces".to_vec(),
                Value::bytes(piece_hashes(3, 0x11)),
            ),
        ]))
    }

    fn single_file_torrent() -> Vec<u8> {
        bencode::encode(&Value::Dict(BTreeMap::from([
            (
                b"announce".to_vec(),
                Value::string("http://tracker.example/announce"),
            ),
            (b"comment".to_vec(), Value::string("demo torrent")),
            (b"created by".to_vec(), Value::string("torrenta tests")),
            (b"creation date".to_vec(), Value::int(1_700_000_000)),
            (b"info".to_vec(), single_file_info()),
        ])))
    }

    fn multi_file_torrent() -> Vec<u8> {
        let info = Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::string("collection")),
            (
                b"piece length".to_vec(),
                Value::int(PIECE_LEN as i64),
            ),
            (
                b"pieces".to_vec(),
                Value::bytes(piece_hashes(4, 0x40)),
            ),
            (
                b"files".to_vec(),
                Value::List(vec![
                    Value::Dict(BTreeMap::from([
                        (b"length".to_vec(), Value::int(1000)),
                        (
                            b"path".to_vec(),
                            Value::List(vec![Value::string("dir"), Value::string("a.bin")]),
                        ),
                    ])),
                    Value::Dict(BTreeMap::from([
                        (b"length".to_vec(), Value::int(2048)),
                        (
                            b"path".to_vec(),
                            Value::List(vec![Value::string("b.bin")]),
                        ),
                    ])),
                ]),
            ),
        ]));
        bencode::encode(&Value::Dict(BTreeMap::from([
            (b"info".to_vec(), info),
            (
                b"announce-list".to_vec(),
                Value::List(vec![
                    Value::List(vec![Value::string("http://t1.example/announce")]),
                    Value::List(vec![
                        Value::string("http://t2.example/announce"),
                        Value::string("http://t1.example/announce"),
                    ]),
                ]),
            ),
        ])))
    }

    #[test]
    fn parses_single_file_torrent() {
        let metainfo = Metainfo::parse(&single_file_torrent()).unwrap();
        assert_eq!(metainfo.name(), "ubuntu.iso");
        assert_eq!(metainfo.total_length(), 700_000);
        assert_eq!(metainfo.piece_count(), 3);
        assert_eq!(metainfo.info.files.len(), 1);
        assert_eq!(metainfo.info.files[0].path, "ubuntu.iso");
        assert_eq!(metainfo.info.files[0].offset, 0);
        assert_eq!(metainfo.info.files[0].length, 700_000);
        assert_eq!(metainfo.announce.as_deref(), Some("http://tracker.example/announce"));
        assert_eq!(metainfo.comment.as_deref(), Some("demo torrent"));
        assert_eq!(metainfo.created_by.as_deref(), Some("torrenta tests"));
        assert_eq!(metainfo.creation_date, Some(1_700_000_000));
        assert!(!metainfo.info.private);
    }

    #[test]
    fn info_hash_is_sha1_of_raw_info() {
        let metainfo = Metainfo::parse(&single_file_torrent()).unwrap();
        // Независимый подсчёт: SHA-1 от ровно тех байтов, что закодированы в info.
        let expected_raw = bencode::encode(&single_file_info());
        assert_eq!(metainfo.raw_info, expected_raw);
        let mut hasher = Sha1::new();
        hasher.update(&expected_raw);
        let expected: [u8; 20] = hasher.finalize().into();
        assert_eq!(metainfo.info_hash.as_bytes(), &expected);
        assert_eq!(metainfo.info_hash.to_hex().len(), 40);
        assert_eq!(
            InfoHash::from_hex(&metainfo.info_hash.to_hex()).unwrap(),
            metainfo.info_hash
        );
    }

    #[test]
    fn parses_multi_file_torrent() {
        let metainfo = Metainfo::parse(&multi_file_torrent()).unwrap();
        assert_eq!(metainfo.name(), "collection");
        assert_eq!(metainfo.info.files.len(), 2);
        assert_eq!(metainfo.info.files[0].path, "dir/a.bin");
        assert_eq!(metainfo.info.files[0].length, 1000);
        assert_eq!(metainfo.info.files[0].offset, 0);
        assert_eq!(metainfo.info.files[1].path, "b.bin");
        assert_eq!(metainfo.info.files[1].length, 2048);
        assert_eq!(metainfo.info.files[1].offset, 1000);
        assert_eq!(metainfo.total_length(), 3048);
        // announce-list (BEP 12) — плоский список трекеров.
        assert_eq!(
            metainfo.announce_list,
            vec![
                "http://t1.example/announce".to_string(),
                "http://t2.example/announce".to_string(),
                "http://t1.example/announce".to_string(),
            ]
        );
        assert!(metainfo.announce.is_none());
        assert_eq!(
            metainfo.trackers(),
            vec![
                "http://t1.example/announce".to_string(),
                "http://t2.example/announce".to_string(),
            ]
        );
    }

    #[test]
    fn piece_geometry_and_offsets() {
        let metainfo = Metainfo::parse(&single_file_torrent()).unwrap();
        let info = &metainfo.info;
        assert_eq!(info.piece_offset(0), 0);
        assert_eq!(info.piece_offset(2), 2 * PIECE_LEN as u64);
        assert_eq!(info.piece_size(0), PIECE_LEN);
        // Последний кусок короче: 700 000 - 2*262 144 = 175 712.
        assert_eq!(info.piece_size(2), 175_712);
        assert_eq!(info.piece_size(3), 0);
        assert_eq!(info.piece_size(usize::MAX), 0);
    }

    #[test]
    fn rejects_pieces_not_multiple_of_20() {
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), Value::string("broken"));
        info.insert(b"piece length".to_vec(), Value::int(16384));
        info.insert(b"length".to_vec(), Value::int(1000));
        info.insert(b"pieces".to_vec(), Value::bytes(vec![0u8; 30]));
        let torrent = bencode::encode(&Value::Dict(BTreeMap::from([(
            b"info".to_vec(),
            Value::Dict(info),
        )])));
        assert!(matches!(
            Metainfo::parse(&torrent),
            Err(MetainfoError::InvalidInfo("pieces"))
        ));
    }

    #[test]
    fn private_flag_and_missing_fields() {
        let info = Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::string("private.iso")),
            (b"piece length".to_vec(), Value::int(16384)),
            (b"pieces".to_vec(), Value::bytes(piece_hashes(1, 1))),
            (b"length".to_vec(), Value::int(100)),
            (b"private".to_vec(), Value::int(1)),
        ]));
        let torrent = bencode::encode(&Value::Dict(BTreeMap::from([(b"info".to_vec(), info)])));
        let metainfo = Metainfo::parse(&torrent).unwrap();
        assert!(metainfo.info.private);

        // Нет info вовсе.
        let no_info = bencode::encode(&Value::Dict(BTreeMap::from([(
            b"announce".to_vec(),
            Value::string("http://t/announce"),
        )])));
        assert!(matches!(
            Metainfo::parse(&no_info),
            Err(MetainfoError::MissingInfo)
        ));
        // Не словарь на верхнем уровне.
        assert!(matches!(
            Metainfo::parse(b"i1e"),
            Err(MetainfoError::Bencode(BencodeError::NotADict))
        ));
        // Без имени и целиком битый bencode.
        let no_name = bencode::encode(&Value::Dict(BTreeMap::from([(
            b"info".to_vec(),
            Value::Dict(BTreeMap::from([
                (b"piece length".to_vec(), Value::int(16384)),
                (b"pieces".to_vec(), Value::bytes(vec![])),
                (b"length".to_vec(), Value::int(0)),
            ])),
        )])));
        assert!(matches!(
            Metainfo::parse(&no_name),
            Err(MetainfoError::MissingName)
        ));
        assert!(matches!(
            Metainfo::parse(b"d4:infoi1ee"),
            Err(MetainfoError::Bencode(BencodeError::NotADict))
        ));
    }

    #[test]
    fn from_raw_info_supports_name_hint_and_utf8() {
        let info = Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::string("russian-name")),
            (b"name.utf-8".to_vec(), Value::string("русское-имя")),
            (b"piece length".to_vec(), Value::int(16384)),
            (b"pieces".to_vec(), Value::bytes(piece_hashes(1, 7))),
            (b"length".to_vec(), Value::int(1234)),
        ]));
        let raw = bencode::encode(&info);
        let metainfo = Metainfo::from_raw_info(
            &raw,
            vec!["udp://tracker.example:6969/announce".to_string()],
            Some("ignored-hint"),
        )
        .unwrap();
        // name.utf-8 приоритетнее name и тем более подсказки.
        assert_eq!(metainfo.name(), "русское-имя");
        assert_eq!(metainfo.total_length(), 1234);
        assert_eq!(
            metainfo.announce_list,
            vec!["udp://tracker.example:6969/announce".to_string()]
        );
        assert_eq!(metainfo.raw_info, raw);
        let mut hasher = Sha1::new();
        hasher.update(&raw);
        let expected: [u8; 20] = hasher.finalize().into();
        assert_eq!(metainfo.info_hash.as_bytes(), &expected);

        // Без имени в словаре — берётся name_hint.
        let raw_no_name = bencode::encode(&Value::Dict(BTreeMap::from([
            (b"piece length".to_vec(), Value::int(16384)),
            (b"pieces".to_vec(), Value::bytes(vec![])),
            (b"length".to_vec(), Value::int(0)),
        ])));
        let hinted = Metainfo::from_raw_info(&raw_no_name, Vec::new(), Some("hint.iso")).unwrap();
        assert_eq!(hinted.name(), "hint.iso");
        // И без hint — ошибка.
        assert!(matches!(
            Metainfo::from_raw_info(&raw_no_name, Vec::new(), None),
            Err(MetainfoError::MissingName)
        ));
    }

    #[test]
    fn from_raw_info_rejects_trailing_and_bad_piece_length() {
        let raw = bencode::encode(&Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::string("x")),
            (b"piece length".to_vec(), Value::int(0)),
            (b"pieces".to_vec(), Value::bytes(vec![])),
            (b"length".to_vec(), Value::int(0)),
        ])));
        assert!(matches!(
            Metainfo::from_raw_info(&raw, Vec::new(), None),
            Err(MetainfoError::InvalidInfo("piece length"))
        ));
        // Данные после значения info — ошибка, а не молчаливое усечение.
        let mut with_trailing = raw.clone();
        with_trailing.extend_from_slice(b"junk");
        assert!(matches!(
            Metainfo::from_raw_info(&with_trailing, Vec::new(), None),
            Err(MetainfoError::Bencode(BencodeError::TrailingData))
        ));
    }

    #[test]
    fn announce_list_flat_strings_and_empty_trackers() {
        let info = single_file_info();
        let torrent = bencode::encode(&Value::Dict(BTreeMap::from([
            (
                b"announce-list".to_vec(),
                Value::List(vec![
                    Value::string("http://t1.example/announce"),
                    Value::string("http://t2.example/announce"),
                ]),
            ),
            (b"info".to_vec(), info),
        ])));
        let metainfo = Metainfo::parse(&torrent).unwrap();
        assert_eq!(metainfo.trackers().len(), 2);
        assert_eq!(metainfo.trackers()[1], "http://t2.example/announce");
        assert!(metainfo.announce.is_none());
    }

    #[test]
    fn rejects_negative_and_overflowing_lengths() {
        let negative = bencode::encode(&Value::Dict(BTreeMap::from([(
            b"info".to_vec(),
            Value::Dict(BTreeMap::from([
                (b"name".to_vec(), Value::string("n")),
                (b"piece length".to_vec(), Value::int(16384)),
                (b"pieces".to_vec(), Value::bytes(vec![])),
                (b"length".to_vec(), Value::int(-5)),
            ])),
        )])));
        assert!(matches!(
            Metainfo::parse(&negative),
            Err(MetainfoError::InvalidInfo("length"))
        ));

        // piece length больше u32.
        let huge = bencode::encode(&Value::Dict(BTreeMap::from([(
            b"info".to_vec(),
            Value::Dict(BTreeMap::from([
                (b"name".to_vec(), Value::string("n")),
                (b"piece length".to_vec(), Value::int(1 << 40)),
                (b"pieces".to_vec(), Value::bytes(vec![])),
                (b"length".to_vec(), Value::int(0)),
            ])),
        )])));
        assert!(matches!(
            Metainfo::parse(&huge),
            Err(MetainfoError::InvalidInfo("piece length"))
        ));
    }

    #[test]
    fn rejects_multifile_with_broken_path() {
        let torrent = bencode::encode(&Value::Dict(BTreeMap::from([(
            b"info".to_vec(),
            Value::Dict(BTreeMap::from([
                (b"name".to_vec(), Value::string("n")),
                (b"piece length".to_vec(), Value::int(16384)),
                (b"pieces".to_vec(), Value::bytes(vec![])),
                (
                    b"files".to_vec(),
                    Value::List(vec![Value::Dict(BTreeMap::from([
                        (b"length".to_vec(), Value::int(10)),
                        (b"path".to_vec(), Value::List(vec![])),
                    ]))]),
                ),
            ])),
        )])));
        assert!(matches!(
            Metainfo::parse(&torrent),
            Err(MetainfoError::InvalidInfo("file path"))
        ));
    }

    #[test]
    fn raw_info_spans_survive_unknown_top_level_fields() {
        let torrent = bencode::encode(&Value::Dict(BTreeMap::from([
            (b"announce".to_vec(), Value::string("http://t/announce")),
            (b"zzz".to_vec(), Value::List(vec![Value::int(1), Value::int(2)])),
            (b"info".to_vec(), single_file_info()),
        ])));
        let metainfo = Metainfo::parse(&torrent).unwrap();
        assert_eq!(metainfo.raw_info, bencode::encode(&single_file_info()));
        assert_eq!(metainfo.info_hash, sha1_of(&metainfo.raw_info));
    }
}
