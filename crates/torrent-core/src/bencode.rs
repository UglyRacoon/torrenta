//! Bencode — формат сериализации BitTorrent (BEP 3).
//!
//! ```text
//! i42e            → 42
//! 5:hello         → b"hello"
//! l3:abci1ee      → [b"abc", 1]
//! d3:abci1ee      → {"abc": 1}
//! ```
//!
//! Ключи словаря — сырые байты, но сортируются они лексикографически по байтам
//! (канонический порядок сохраняется при кодировании).

use std::collections::BTreeMap;

use thiserror::Error;

/// Предельная глубина вложенности. Защищает рекурсивный разбор от переполнения
/// стека на враждебных данных вида `llllllll...`.
const MAX_DEPTH: usize = 128;

/// Сколько цифр максимум может быть в `i...e` (`-9223372036854775808`).
const MAX_INT_DIGITS: usize = 20;

/// Сколько цифр максимум может быть в длине строки (`u64::MAX`).
const MAX_LEN_DIGITS: usize = 19;

/// Ошибки разбора bencode.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BencodeError {
    #[error("неожиданный конец данных")]
    UnexpectedEof,
    #[error("недопустимый символ 0x{0:02x} по смещению {1}")]
    InvalidByte(u8, usize),
    #[error("некорректное целое")]
    InvalidInteger,
    #[error("некорректная длина строки")]
    InvalidLength,
    #[error("данные после конца значения")]
    TrailingData,
    #[error("ожидался словарь")]
    NotADict,
}

/// Значение bencode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn int(v: i64) -> Value {
        Value::Int(v)
    }

    pub fn bytes(b: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(b.into())
    }

    pub fn string(s: impl Into<String>) -> Value {
        Value::Bytes(s.into().into_bytes())
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Доступ к элементу словаря по строковому ключу.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict().and_then(|d| d.get(key.as_bytes()))
    }
}

/// Декодирует одно значение, возвращая его и число прочитанных байт.
pub fn decode(buf: &[u8]) -> Result<(Value, usize), BencodeError> {
    let mut pos = 0usize;
    let value = parse_value(buf, &mut pos, 0)?;
    Ok((value, pos))
}

/// Декодирует значение и требует, чтобы входные данные закончились.
pub fn decode_all(buf: &[u8]) -> Result<Value, BencodeError> {
    let (value, consumed) = decode(buf)?;
    if consumed != buf.len() {
        return Err(BencodeError::TrailingData);
    }
    Ok(value)
}

/// Кодирует значение в bencode. Ключи словаря пишутся в каноническом порядке.
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(v) => {
            out.push(b'i');
            out.extend_from_slice(v.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(bytes) => {
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(bytes);
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                encode_into(item, out);
            }
            out.push(b'e');
        }
        Value::Dict(map) => {
            out.push(b'd');
            // `BTreeMap<Vec<u8>, _>` обходит ключи в лексикографическом порядке
            // байтов — это и есть канонический порядок bencode.
            for (key, value) in map {
                out.extend_from_slice(key.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(key);
                encode_into(value, out);
            }
            out.push(b'e');
        }
    }
}

fn parse_value(buf: &[u8], pos: &mut usize, depth: usize) -> Result<Value, BencodeError> {
    if depth > MAX_DEPTH {
        // Специального варианта ошибки для «слишком глубоко» в контракте нет,
        // поэтому сообщаем позицию байта, который открыл лишний уровень.
        return Err(BencodeError::InvalidByte(
            buf.get(*pos).copied().unwrap_or(0),
            *pos,
        ));
    }
    let byte = *buf.get(*pos).ok_or(BencodeError::UnexpectedEof)?;
    match byte {
        b'i' => parse_integer(buf, pos),
        b'l' => parse_list(buf, pos, depth),
        b'd' => parse_dict(buf, pos, depth),
        b'0'..=b'9' => parse_bytes(buf, pos),
        other => Err(BencodeError::InvalidByte(other, *pos)),
    }
}

/// `i<целое>e` — строго: без `+`, без ведущих нулей, без `-0`, без пустого.
fn parse_integer(buf: &[u8], pos: &mut usize) -> Result<Value, BencodeError> {
    let start = *pos + 1;
    let mut end = start;
    loop {
        match buf.get(end) {
            None => return Err(BencodeError::UnexpectedEof),
            Some(b'e') => break,
            Some(_) => end += 1,
        }
    }
    let digits = &buf[start..end];
    let value = parse_integer_digits(digits)?;
    *pos = end + 1;
    Ok(Value::Int(value))
}

fn parse_integer_digits(digits: &[u8]) -> Result<i64, BencodeError> {
    if digits.is_empty() || digits.len() > MAX_INT_DIGITS {
        return Err(BencodeError::InvalidInteger);
    }
    let (negative, body) = match digits[0] {
        b'-' => (true, &digits[1..]),
        _ => (false, digits),
    };
    if body.is_empty() {
        return Err(BencodeError::InvalidInteger);
    }
    if !body.iter().all(u8::is_ascii_digit) {
        return Err(BencodeError::InvalidInteger);
    }
    // "0" допустим, "00", "01", "-0", "-01" — нет.
    if body.len() > 1 && body[0] == b'0' {
        return Err(BencodeError::InvalidInteger);
    }
    if negative && body == b"0" {
        return Err(BencodeError::InvalidInteger);
    }
    // Тело — ASCII-цифры, поэтому from_utf8 не может провалиться.
    let magnitude: u64 = std::str::from_utf8(body)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(BencodeError::InvalidInteger)?;
    if negative {
        let value = -(magnitude as i128);
        if value < i64::MIN as i128 {
            return Err(BencodeError::InvalidInteger);
        }
        Ok(value as i64)
    } else {
        if magnitude > i64::MAX as u64 {
            return Err(BencodeError::InvalidInteger);
        }
        Ok(magnitude as i64)
    }
}

/// `<len>:<bytes>` — длина без ведущих нулей, данные не выходят за буфер.
fn parse_bytes(buf: &[u8], pos: &mut usize) -> Result<Value, BencodeError> {
    let start = *pos;
    let mut end = start;
    loop {
        match buf.get(end) {
            None => return Err(BencodeError::UnexpectedEof),
            Some(b':') => break,
            Some(b) if b.is_ascii_digit() => end += 1,
            Some(b) => return Err(BencodeError::InvalidByte(*b, end)),
        }
    }
    let digits = &buf[start..end];
    if digits.len() > MAX_LEN_DIGITS {
        return Err(BencodeError::InvalidLength);
    }
    if digits.len() > 1 && digits[0] == b'0' {
        return Err(BencodeError::InvalidLength);
    }
    let mut len: u64 = 0;
    for digit in digits {
        len = len
            .checked_mul(10)
            .and_then(|v| v.checked_add((digit - b'0') as u64))
            .ok_or(BencodeError::InvalidLength)?;
    }
    let data_start = end + 1;
    let remaining = (buf.len() - data_start) as u64;
    if len > remaining {
        return Err(BencodeError::UnexpectedEof);
    }
    let len = len as usize;
    *pos = data_start + len;
    Ok(Value::Bytes(buf[data_start..data_start + len].to_vec()))
}

/// `l...e`
fn parse_list(buf: &[u8], pos: &mut usize, depth: usize) -> Result<Value, BencodeError> {
    *pos += 1;
    let mut items = Vec::new();
    loop {
        match buf.get(*pos) {
            None => return Err(BencodeError::UnexpectedEof),
            Some(b'e') => {
                *pos += 1;
                return Ok(Value::List(items));
            }
            Some(_) => items.push(parse_value(buf, pos, depth + 1)?),
        }
    }
}

/// `d<key><value>...e` — ключи обязаны быть строками.
fn parse_dict(buf: &[u8], pos: &mut usize, depth: usize) -> Result<Value, BencodeError> {
    *pos += 1;
    let mut map: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
    loop {
        match buf.get(*pos) {
            None => return Err(BencodeError::UnexpectedEof),
            Some(b'e') => {
                *pos += 1;
                return Ok(Value::Dict(map));
            }
            Some(byte) if byte.is_ascii_digit() => {
                let key = match parse_bytes(buf, pos)? {
                    Value::Bytes(bytes) => bytes,
                    _ => unreachable!("parse_bytes всегда возвращает Bytes"),
                };
                let value = parse_value(buf, pos, depth + 1)?;
                map.insert(key, value);
            }
            Some(other) => return Err(BencodeError::InvalidByte(*other, *pos)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(v: i64) -> Value {
        Value::Int(v)
    }

    fn b(bytes: &[u8]) -> Value {
        Value::Bytes(bytes.to_vec())
    }

    #[test]
    fn decode_scalars() {
        assert_eq!(decode(b"i42e").unwrap(), (int(42), 4));
        assert_eq!(decode(b"i-42e").unwrap(), (int(-42), 5));
        assert_eq!(decode(b"i0e").unwrap(), (int(0), 3));
        assert_eq!(decode(b"5:hello").unwrap(), (b(b"hello"), 7));
        assert_eq!(decode(b"0:").unwrap(), (b(b""), 2));
    }

    #[test]
    fn decode_integer_extremes() {
        assert_eq!(
            decode(b"i9223372036854775807e").unwrap().0,
            int(i64::MAX)
        );
        assert_eq!(
            decode(b"i-9223372036854775808e").unwrap().0,
            int(i64::MIN)
        );
        // i64::MAX + 1 — переполнение.
        assert_eq!(
            decode(b"i9223372036854775808e").unwrap_err(),
            BencodeError::InvalidInteger
        );
        // i64::MIN - 1 — переполнение.
        assert_eq!(
            decode(b"i-9223372036854775809e").unwrap_err(),
            BencodeError::InvalidInteger
        );
        // Заведомо длинное целое не съедает память/время впустую.
        assert_eq!(
            decode(b"i000000000000000000000000000001e").unwrap_err(),
            BencodeError::InvalidInteger
        );
    }

    #[test]
    fn decode_nested_containers() {
        let value = Value::Dict(BTreeMap::from([
            (b"abc".to_vec(), Value::List(vec![b(b"a"), int(2)])),
            (b"xyz".to_vec(), int(5)),
        ]));
        let encoded = encode(&value);
        assert_eq!(encoded, b"d3:abcl1:ai2ee3:xyzi5ee");
        let (decoded, consumed) = decode(&encoded).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn encode_and_decode_roundtrip() {
        let value = Value::Dict(BTreeMap::from([
            (b"announce".to_vec(), b(b"http://tracker.example/announce")),
            (b"info".to_vec(), Value::Dict(BTreeMap::from([(b"name".to_vec(), b(b"file.bin"))]))),
            (b"list".to_vec(), Value::List(vec![int(1), b(b"two"), Value::List(vec![])])),
        ]));
        let encoded = encode(&value);
        assert_eq!(decode_all(&encoded).unwrap(), value);
        // Ключи словаря отсортированы по байтам.
        assert!(encoded.starts_with(b"d8:announce"));
        let s = String::from_utf8_lossy(&encoded);
        assert!(s.find("4:info").unwrap() < s.find("4:list").unwrap());
    }

    #[test]
    fn encode_int_is_canonical() {
        assert_eq!(encode(&int(0)), b"i0e");
        assert_eq!(encode(&int(-1)), b"i-1e");
        assert_eq!(encode(&int(i64::MIN)), b"i-9223372036854775808e");
    }

    #[test]
    fn encode_sorts_dict_keys_bytewise() {
        let mut map = BTreeMap::new();
        // "Z" (0x5A) < "a" (0x61) — именно побайтовый порядок, не регистронезависимый.
        map.insert(b"a".to_vec(), int(1));
        map.insert(b"Z".to_vec(), int(2));
        assert_eq!(encode(&Value::Dict(map)), b"d1:Zi2e1:ai1ee");
    }

    #[test]
    fn rejects_leading_zero_and_negative_zero() {
        assert_eq!(decode(b"i00e").unwrap_err(), BencodeError::InvalidInteger);
        assert_eq!(decode(b"i01e").unwrap_err(), BencodeError::InvalidInteger);
        assert_eq!(decode(b"i-0e").unwrap_err(), BencodeError::InvalidInteger);
        assert_eq!(decode(b"i-01e").unwrap_err(), BencodeError::InvalidInteger);
        assert_eq!(decode(b"i+1e").unwrap_err(), BencodeError::InvalidInteger);
        assert_eq!(decode(b"ie").unwrap_err(), BencodeError::InvalidInteger);
        assert_eq!(decode(b"i-e").unwrap_err(), BencodeError::InvalidInteger);
        assert_eq!(decode(b"i1x2e").unwrap_err(), BencodeError::InvalidInteger);
        // Длина строки тоже строгая.
        assert_eq!(decode(b"01:a").unwrap_err(), BencodeError::InvalidLength);
        assert_eq!(decode(b"00:").unwrap_err(), BencodeError::InvalidLength);
        assert_eq!(
            decode(b"-1:x").unwrap_err(),
            BencodeError::InvalidByte(b'-', 0)
        );
    }

    #[test]
    fn rejects_truncated_input_at_every_position() {
        let full = b"d8:announce3:foo4:infod4:name4:fileee";
        // Кодирование полного значения, затем обрезаем на каждом префиксе.
        let encoded = encode(&decode_all(full).unwrap());
        for cut in 0..encoded.len() {
            let result = decode(&encoded[..cut]);
            assert!(
                result.is_err(),
                "префикс длиной {cut} не должен разбираться: {:?}",
                result
            );
        }
        // Обрезанная строка — не UnexpectedEof, только ошибка.
        assert_eq!(
            decode(b"5:hell").unwrap_err(),
            BencodeError::UnexpectedEof
        );
        assert_eq!(decode(b"i12").unwrap_err(), BencodeError::UnexpectedEof);
        assert_eq!(decode(b"l1:a").unwrap_err(), BencodeError::UnexpectedEof);
        assert_eq!(decode(b"d1:a").unwrap_err(), BencodeError::UnexpectedEof);
        // А полное значение разбирается целиком.
        assert_eq!(decode(b"d1:a1:be").unwrap().1, 8);
    }

    #[test]
    fn rejects_huge_declared_string_length() {
        // Заявленная длина 9999999999999999999 не должна приводить к аллокации/панике:
        // цифр в пределах лимита, а байтов не хватает — честный UnexpectedEof.
        assert_eq!(
            decode(b"9999999999999999999:x").unwrap_err(),
            BencodeError::UnexpectedEof
        );
        // А вот 20 цифр — уже невалидная длина.
        assert_eq!(
            decode(b"12345678901234567890:x").unwrap_err(),
            BencodeError::InvalidLength
        );
        assert_eq!(decode(b"4:abc").unwrap_err(), BencodeError::UnexpectedEof);
    }

    #[test]
    fn rejects_trailing_data() {
        assert_eq!(
            decode_all(b"i1ei2e").unwrap_err(),
            BencodeError::TrailingData
        );
        assert_eq!(
            decode_all(b"i1e\n").unwrap_err(),
            BencodeError::TrailingData
        );
        // decode при этом отдаёт consumed, чтобы вызывающий мог дочитать поток.
        assert_eq!(decode(b"i1ei2e").unwrap(), (int(1), 3));
    }

    #[test]
    fn rejects_invalid_byte_and_bad_dict_key() {
        assert_eq!(decode(b"x").unwrap_err(), BencodeError::InvalidByte(b'x', 0));
        assert_eq!(decode(b"li1ee").unwrap(), (Value::List(vec![int(1)]), 5));
        // Ключ словаря не строка.
        assert!(matches!(
            decode(b"di1ei2ee").unwrap_err(),
            BencodeError::InvalidByte(_, 1)
        ));
        assert_eq!(decode(b"d1:ai1e").unwrap_err(), BencodeError::UnexpectedEof);
    }

    #[test]
    fn hostile_nesting_is_rejected_without_stack_overflow() {
        // 10 000 открытых списков без закрывающих — не должны уронить процесс.
        let mut payload = vec![b'l'; 10_000];
        payload.push(b'e');
        assert!(decode(&payload).is_err());
        // Ровно в пределах лимита глубина допустима.
        let mut ok = vec![b'l'; MAX_DEPTH];
        ok.extend(std::iter::repeat(b'e').take(MAX_DEPTH));
        assert!(decode(&ok).is_ok());
    }

    #[test]
    fn consumed_points_at_value_end() {
        let data = b"d1:ai1e1:bl1:xeeextraneous";
        let (_, consumed) = decode(data).unwrap();
        assert_eq!(consumed, 16);
        assert_eq!(&data[consumed..], b"extraneous");
    }

    #[test]
    fn dict_key_sorting_preserves_canonical_info_dict() {
        // Канонический порядок ключей info-словаря: length < name < piece length < pieces.
        let mut map = BTreeMap::new();
        map.insert(b"pieces".to_vec(), b(b"01234567890123456789"));
        map.insert(b"name".to_vec(), b(b"a.iso"));
        map.insert(b"piece length".to_vec(), int(16384));
        map.insert(b"length".to_vec(), int(99));
        let encoded = encode(&Value::Dict(map));
        assert_eq!(
            encoded,
            b"d6:lengthi99e4:name5:a.iso12:piece lengthi16384e6:pieces20:01234567890123456789e".to_vec()
        );
        // Ключи BTreeMap обходятся в нужном порядке, значения не потеряны.
        assert_eq!(decode_all(&encoded).unwrap().as_dict().unwrap().len(), 4);
    }
}
