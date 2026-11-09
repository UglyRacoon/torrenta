//! Детерминированный «фаззинг» bencode (BEP 3) без внешних зависимостей.
//!
//! Идея: вместо `cargo-fuzz`/`libfuzzer` (новые зависимости и nightly-тулчейн)
//! используем линейный конгруэнтный генератор с фиксированным seed, поэтому
//! каждый запуск воспроизводим байт в байт.
//!
//! Проверяемые инварианты:
//! * парсер не паникует ни на каком входе (если тест упал — это баг, отдельный
//!   `catch_unwind` не нужен: паника и так валит тест);
//! * любой отказ — это `BencodeError`, а не «тихо вернул мусор»;
//! * успешный разбор не выходит за границы буфера и не съедает ноль байт;
//! * `encode → decode` возвращает ровно то же значение (round-trip);
//! * `decode_all` строже `decode`: либо ровно один элемент до конца входа, либо
//!   `TrailingData`;
//! * глубокая вложенность отвергается ошибкой, а не переполнением стека.

use std::collections::BTreeMap;

use torrent_core::bencode::{self, BencodeError, Value};

/// Сколько случайных входов генерировать.
const RANDOM_ITERS: usize = 4_000;

/// Сколько мутаций валидных входов прогонять.
const MUTATION_ITERS: usize = 3_000;

/// Сколько случайных деревьев проверять на round-trip.
const TREE_ITERS: usize = 500;

/// Валидные эталоны: числа во всех формах, строки (включая пустую), списки,
/// словари, вложенные структуры, похожие на настоящий `.torrent`.
const SEEDS: &[&[u8]] = &[
    b"i0e",
    b"i42e",
    b"i-42e",
    b"i9223372036854775807e",
    b"i-9223372036854775808e",
    b"0:",
    b"1:a",
    b"4:spam",
    b"11:hello world",
    b"le",
    b"de",
    b"li1ei2ei3ee",
    b"l1:ai2ee",
    b"d1:ai1ee",
    b"d3:bar4:spam3:fooi42ee",
    b"d4:infod4:name4:test6:lengthi1024eee",
    b"l1:al1:bl1:ceee",
    b"ld1:ai1eed1:bi2eee",
    b"d1:ad1:bd1:ci1eeee",
];

/// Детерминированный генератор псевдослучайных чисел (LCG из Numerical Recipes).
///
/// Константы — стандартные 64-битные множитель/приращение из Knuth MMIX.
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Следующее 32-битное число: старшие биты LCG качественнее младших.
    fn next_u32(&mut self) -> u32 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.state >> 32) as u32
    }

    /// Равномерное число в диапазоне `0..bound` (`bound == 0` даёт 0).
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        self.next_u32() as usize % bound
    }

    /// Произвольный байт `0..=255`.
    fn byte(&mut self) -> u8 {
        (self.next_u32() & 0xFF) as u8
    }
}

/// Байт, смещённый к алфавиту bencode: так случайные входы чаще доходят до
/// глубинных ветвей разбора, а не отваливаются на первом же символе.
fn biased_byte(rng: &mut Lcg) -> u8 {
    const ALPHABET: &[u8] = b"idl0123456789:+-ee";
    if rng.below(100) < 60 {
        ALPHABET[rng.below(ALPHABET.len())]
    } else {
        rng.byte()
    }
}

/// Случайный вход длиной `0..=max_len`.
fn random_input(rng: &mut Lcg, max_len: usize) -> Vec<u8> {
    let len = rng.below(max_len + 1);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(biased_byte(rng));
    }
    out
}

/// Случайная мутация эталона: инверсия бита, удаление, вставка, замена, усечение.
fn mutate(rng: &mut Lcg, seed: &[u8]) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }
    let ops = 1 + rng.below(3);
    for _ in 0..ops {
        if out.is_empty() {
            break;
        }
        match rng.below(5) {
            0 => {
                let index = rng.below(out.len());
                out[index] ^= 1 << rng.below(8);
            }
            1 => {
                let index = rng.below(out.len());
                out.remove(index);
            }
            2 => {
                let index = rng.below(out.len() + 1);
                out.insert(index, biased_byte(rng));
            }
            3 => {
                let index = rng.below(out.len());
                out[index] = biased_byte(rng);
            }
            _ => {
                let len = rng.below(out.len());
                out.truncate(len);
            }
        }
    }
    out
}

/// `depth` вложенных списков: `lll...eee`.
fn nested_lists(depth: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(depth * 2);
    out.extend(std::iter::repeat(b'l').take(depth));
    out.extend(std::iter::repeat(b'e').take(depth));
    out
}

/// `depth` вложенных словарей с числом в самой глубине: `d1:ad1:a...i1ee...e`.
fn nested_dicts(depth: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(depth * 4 + 4);
    for _ in 0..depth {
        out.extend_from_slice(b"d1:a");
    }
    out.extend_from_slice(b"i1e");
    out.extend(std::iter::repeat(b'e').take(depth));
    out
}

/// Случайное корректное значение: листья — числа и строки, ветви — списки и словари.
fn random_value(rng: &mut Lcg, depth: usize) -> Value {
    let leaf = |rng: &mut Lcg| -> Value {
        match rng.below(4) {
            0 => Value::Int(rng.next_u32() as i64),
            1 => Value::Int(-(rng.next_u32() as i64) - 1),
            2 => Value::Int(rng.below(1_000_000) as i64),
            _ => {
                let len = rng.below(12);
                let mut bytes = Vec::with_capacity(len);
                for _ in 0..len {
                    bytes.push(biased_byte(rng));
                }
                Value::Bytes(bytes)
            }
        }
    };

    if depth == 0 {
        return leaf(rng);
    }
    match rng.below(4) {
        0 | 1 => leaf(rng),
        2 => {
            let len = rng.below(4);
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                items.push(random_value(rng, depth - 1));
            }
            Value::List(items)
        }
        _ => {
            let len = rng.below(4);
            let mut map = BTreeMap::new();
            for _ in 0..len {
                let key_len = rng.below(6);
                let mut key = Vec::with_capacity(key_len);
                for _ in 0..key_len {
                    key.push(biased_byte(rng));
                }
                map.insert(key, random_value(rng, depth - 1));
            }
            Value::Dict(map)
        }
    }
}

/// Инварианты, которые обязаны выполняться для ЛЮБОГО входа.
fn check_invariants(input: &[u8]) {
    // Любой отказ — это BencodeError (тип возвращаемого значения), поэтому
    // ветку `Err` отдельно проверять не нужно: важно лишь, что она не паникует.
    if let Ok((value, consumed)) = bencode::decode(input) {
        assert!(
            consumed <= input.len(),
            "парсер прочитал больше, чем ему дали: {consumed} > {} для {input:?}",
            input.len()
        );
        assert!(
            consumed > 0,
            "парсер вернул значение, не прочитав ни байта: {input:?}"
        );
        let encoded = bencode::encode(&value);
        let restored = match bencode::decode_all(&encoded) {
            Ok(restored) => restored,
            Err(err) => panic!("свой же encode({value:?}) не разбирается: {err} ({encoded:?})"),
        };
        assert_eq!(
            restored, value,
            "round-trip encode/decode изменил значение: {input:?}"
        );
        assert_eq!(
            bencode::encode(&restored),
            encoded,
            "кодирование нестабильно (не канонично): {input:?}"
        );
    }

    // decode_all должен быть согласован с decode: раз он вернул Ok, то мягкий
    // разбор обязан прочитать ровно весь вход и дать то же значение.
    if let Ok(value) = bencode::decode_all(input) {
        let (parsed, consumed) =
            bencode::decode(input).expect("decode_all вернул Ok, а decode — ошибку");
        assert_eq!(
            consumed,
            input.len(),
            "decode_all вернул Ok, но остались непрочитанные байты: {input:?}"
        );
        assert_eq!(parsed, value, "decode и decode_all разошлись: {input:?}");
    }
}

#[test]
fn random_bytes_never_panic_and_keep_invariants() {
    // Фиксированные seed'ы: запуск воспроизводим, каждый прогон идентичен.
    let mut rng = Lcg::new(0x5EED_1234_ABCD_9876);
    let mut accepted = 0usize;
    let mut rejected = 0usize;

    for _ in 0..RANDOM_ITERS {
        let input = random_input(&mut rng, 64);
        match bencode::decode_all(&input) {
            Ok(_) => accepted += 1,
            Err(err) => {
                // Ошибка — это именно BencodeError, ничего другого парсер не отдаёт.
                assert!(
                    !matches!(err, BencodeError::TrailingData) || input.len() > 1,
                    "TrailingData на входе из одного байта: {input:?}"
                );
                rejected += 1;
            }
        }
        check_invariants(&input);
    }

    for seed in SEEDS {
        assert!(
            bencode::decode_all(seed).is_ok(),
            "эталон обязан разбираться: {seed:?}"
        );
        accepted += 1;
    }

    assert!(accepted > 0, "ни один вход не принят — фаззер бесполезен");
    assert!(
        rejected > RANDOM_ITERS / 2,
        "случайные байты почти не отвергаются ({rejected} из {RANDOM_ITERS}) — тест вырожден"
    );
}

#[test]
fn mutations_of_valid_inputs_never_panic_and_keep_invariants() {
    let mut rng = Lcg::new(0xDEAD_BEEF_0BAD_F00D);
    let mut accepted = 0usize;
    let mut rejected = 0usize;

    for _ in 0..MUTATION_ITERS {
        let seed = SEEDS[rng.below(SEEDS.len())];
        let input = mutate(&mut rng, seed);
        match bencode::decode_all(&input) {
            Ok(_) => accepted += 1,
            Err(_) => rejected += 1,
        }
        check_invariants(&input);
    }

    assert!(
        accepted > 0,
        "мутации не дают ни одного валидного входа — тест вырожден"
    );
    assert!(
        rejected > 0,
        "мутации не дают ни одного отказа — тест вырожден"
    );
}

#[test]
fn every_truncation_of_valid_input_is_rejected() {
    // Усечение одиночного значения не может оказаться полным значением, поэтому
    // любая длина меньше исходной обязана давать ошибку (и никогда TrailingData).
    for seed in SEEDS {
        for cut in 0..seed.len() {
            let truncated = &seed[..cut];
            let err = match bencode::decode_all(truncated) {
                Ok(value) => panic!("усечение {truncated:?} неожиданно разобралось в {value:?}"),
                Err(err) => err,
            };
            assert_ne!(
                err,
                BencodeError::TrailingData,
                "усечение {truncated:?} вернуло TrailingData вместо ошибки незавершённости"
            );
            check_invariants(truncated);
        }
    }
}

#[test]
fn appended_bytes_are_reported_as_trailing_data() {
    for seed in SEEDS {
        let expected = bencode::decode_all(seed).expect("эталон обязан разбираться");
        for extra in [&b"x"[..], b"e", b"i1e", b"0:", b"\x00", b"eeee"] {
            let mut input = seed.to_vec();
            input.extend_from_slice(extra);
            let err = match bencode::decode_all(&input) {
                Ok(value) => panic!("лишние {extra:?} после значения не замечены: {value:?}"),
                Err(err) => err,
            };
            assert_eq!(
                err,
                BencodeError::TrailingData,
                "вход {input:?} (эталон {expected:?})"
            );

            // Мягкий `decode` обязан вернуть ровно исходный префикс.
            let (value, consumed) =
                bencode::decode(&input).expect("валидный префикс должен разбираться");
            assert_eq!(consumed, seed.len(), "вход {input:?}");
            assert_eq!(value, expected, "вход {input:?}");
        }
    }
}

#[test]
fn invalid_length_prefixes_are_rejected() {
    // Ведущий ноль в длине строки.
    assert_eq!(
        bencode::decode_all(b"03:abc").unwrap_err(),
        BencodeError::InvalidLength
    );
    assert_eq!(
        bencode::decode_all(b"00:").unwrap_err(),
        BencodeError::InvalidLength
    );
    // Слишком много цифр в длине (больше 19 — u64 не влезает).
    assert_eq!(
        bencode::decode_all(b"99999999999999999999:x").unwrap_err(),
        BencodeError::InvalidLength
    );
    assert_eq!(
        bencode::decode_all(b"18446744073709551616:").unwrap_err(),
        BencodeError::InvalidLength
    );
    // Отрицательная и «плюсовая» длина: это не цифра, значит недопустимый байт.
    assert_eq!(
        bencode::decode_all(b"-1:a").unwrap_err(),
        BencodeError::InvalidByte(b'-', 0)
    );
    assert_eq!(
        bencode::decode_all(b"+1:a").unwrap_err(),
        BencodeError::InvalidByte(b'+', 0)
    );
    // Длина больше остатка буфера.
    assert_eq!(
        bencode::decode_all(b"10:short").unwrap_err(),
        BencodeError::UnexpectedEof
    );
    assert_eq!(
        bencode::decode_all(b"9223372036854775808:").unwrap_err(),
        BencodeError::UnexpectedEof
    );
    // А ровная длина — валидна.
    assert_eq!(
        bencode::decode_all(b"5:abcde").expect("ровная длина валидна"),
        Value::Bytes(b"abcde".to_vec())
    );
}

#[test]
fn invalid_integer_forms_are_rejected() {
    for input in [
        &b"i-0e"[..],
        b"i03e",
        b"i00e",
        b"i+1e",
        b"ie",
        b"i-e",
        b"i--1e",
        b"i 1e",
        b"i1 e",
        b"i1.5e",
        b"i1xe",
        b"i9223372036854775808e",
        b"i-9223372036854775809e",
    ] {
        assert_eq!(
            bencode::decode_all(input).unwrap_err(),
            BencodeError::InvalidInteger,
            "вход {input:?}"
        );
    }

    // Границы i64 разбираются ровно.
    assert_eq!(
        bencode::decode_all(b"i9223372036854775807e").unwrap(),
        Value::Int(i64::MAX)
    );
    assert_eq!(
        bencode::decode_all(b"i-9223372036854775808e").unwrap(),
        Value::Int(i64::MIN)
    );

    // Нет закрывающей `e` — это недобор данных.
    for input in [&b"i1"[..], b"i-", b"i", b"i42x"] {
        assert_eq!(
            bencode::decode_all(input).unwrap_err(),
            BencodeError::UnexpectedEof,
            "вход {input:?}"
        );
    }
}

#[test]
fn missing_closing_markers_are_unexpected_eof() {
    for input in [
        &b"l"[..],
        b"l1:a",
        b"l1:ai1e",
        b"d",
        b"d1:a",
        b"d1:ai1e",
        b"d1:ai1e1:b",
        b"ll",
        b"lli1e",
        // `d1:ald1:b`: у самого глубокого словаря ключ «b» есть, а значения и
        // закрывающих `e` нет — именно недобор данных (а не недопустимый байт).
        b"d1:ald1:b",
    ] {
        assert_eq!(
            bencode::decode_all(input).unwrap_err(),
            BencodeError::UnexpectedEof,
            "вход {input:?}"
        );
    }
}

#[test]
fn deep_nesting_is_rejected_without_stack_overflow() {
    // 10 000 уровней: парсер обязан вернуть ошибку, а не рекурсировать до падения.
    // Если сломать ограничение глубины, тест упадёт по stack overflow всего
    // тестового бинарника — это и есть сигнал о баге.
    let deep_lists = nested_lists(10_000);
    assert!(
        bencode::decode_all(&deep_lists).is_err(),
        "10 000 вложенных списков должны отвергаться"
    );
    assert!(bencode::decode(&deep_lists).is_err());

    let deep_dicts = nested_dicts(2_000);
    assert!(
        bencode::decode_all(&deep_dicts).is_err(),
        "2 000 вложенных словарей должны отвергаться"
    );

    // Смешанная вложенность.
    let mut mixed = Vec::new();
    for _ in 0..5_000 {
        mixed.extend_from_slice(b"ld1:a");
    }
    assert!(bencode::decode_all(&mixed).is_err());

    // Обратная совместимость: разумная глубина по-прежнему разбирается.
    // Точное значение лимита не фиксируем (это внутренняя константа парсера),
    // но проверяем, что он не «схлопнулся» и что за ним всегда ошибка.
    let mut max_accepted = 0usize;
    for depth in 1..=512 {
        if bencode::decode_all(&nested_lists(depth)).is_ok() {
            max_accepted = depth;
        } else {
            break;
        }
    }
    assert!(
        max_accepted >= 32,
        "предел вложенности слишком мал: {max_accepted}"
    );
    assert!(
        bencode::decode_all(&nested_lists(max_accepted + 1)).is_err(),
        "за пределом вложенности должна быть ошибка"
    );

    // Усечение глубокого входа тоже не паника, а ошибка.
    let cut = &deep_lists[..deep_lists.len() / 2];
    assert!(bencode::decode_all(cut).is_err());
}

#[test]
fn random_values_round_trip_through_encode_and_decode() {
    let mut rng = Lcg::new(0x0123_4567_89AB_CDEF);
    for iteration in 0..TREE_ITERS {
        let value = random_value(&mut rng, 4);
        let encoded = bencode::encode(&value);
        let decoded = match bencode::decode_all(&encoded) {
            Ok(decoded) => decoded,
            Err(err) => panic!("итерация {iteration}: encode({value:?}) не разбирается: {err}"),
        };
        assert_eq!(
            decoded, value,
            "итерация {iteration}: round-trip изменил значение (bencode: {encoded:?})"
        );
        assert_eq!(
            bencode::encode(&decoded),
            encoded,
            "итерация {iteration}: повторное кодирование не совпало"
        );
    }
}

#[test]
fn handcrafted_torrent_like_structure_round_trips() {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), Value::Int(1024));
    info.insert(b"name".to_vec(), Value::string("тестовый.iso"));
    info.insert(b"piece length".to_vec(), Value::Int(16_384));
    info.insert(b"pieces".to_vec(), Value::Bytes((0u8..=31).collect()));
    info.insert(b"private".to_vec(), Value::Int(1));

    let mut dict = BTreeMap::new();
    dict.insert(
        b"announce".to_vec(),
        Value::string("http://tracker.example/announce"),
    );
    dict.insert(
        b"announce-list".to_vec(),
        Value::List(vec![
            Value::List(vec![Value::string("http://tracker.example/announce")]),
            Value::List(vec![
                Value::string("udp://tracker.example:6969"),
                Value::string("https://tracker.example/announce"),
            ]),
        ]),
    );
    dict.insert(b"comment".to_vec(), Value::Bytes(vec![0, 0xFF, 0x80]));
    dict.insert(b"empty".to_vec(), Value::Bytes(Vec::new()));
    dict.insert(b"info".to_vec(), Value::Dict(info));
    dict.insert(
        b"limits".to_vec(),
        Value::List(vec![
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::Int(0),
            Value::Int(-1),
            Value::List(Vec::new()),
            Value::Dict(BTreeMap::new()),
        ]),
    );
    let value = Value::Dict(dict);

    let encoded = bencode::encode(&value);
    let decoded = bencode::decode_all(&encoded).expect("свой же bencode обязан разбираться");
    assert_eq!(
        decoded, value,
        "словарь-«торрент» прошёл round-trip не точно"
    );
    assert_eq!(
        bencode::encode(&decoded),
        encoded,
        "кодирование нестабильно"
    );

    // Ключи верхнего уровня идут в лексикографическом порядке — это канонично.
    let first_level: Vec<&[u8]> = decoded
        .as_dict()
        .expect("верхний уровень — словарь")
        .keys()
        .map(Vec::as_slice)
        .collect();
    let mut sorted = first_level.clone();
    sorted.sort_unstable();
    assert_eq!(
        first_level, sorted,
        "ключи словаря не в каноническом порядке"
    );
}
