//! MSE/PE — обфускация и шифрование соединений с пирами (1.9).
//!
//! Реализовано по спецификации Message Stream Encryption v1.0
//! (https://wiki.vuze.com/w/Message_Stream_Encryption) — той же, что используют
//! libtorrent и qBittorrent. Цель — не «секретность», а узнаваемость трафика:
//! пассивный наблюдатель видит случайные байты, а не `\x13BitTorrent protocol`.
//!
//! Три шага рукопожатия:
//!
//! 1. инициатор отправляет свой Diffie-Hellman ключ `Ya` и случайный паддинг;
//! 2. принимающая сторона отвечает `Yb` и паддингом;
//! 3. инициатор отправляет `HASH('req1', S)`, хэш-идентификатор раздачи
//!    `HASH('req2', SKEY) xor HASH('req3', S)` и зашифрованный блок с
//!    `VC`, `crypto_provide`, паддингом и начальными данными; принимающая
//!    сторона отвечает зашифрованным `VC`, `crypto_select` и паддингом.
//!    Дальше поток либо шифруется RC4 (0x02), либо идёт открытым (0x01).
//!
//! Ключи сторон: `HASH('keyA', S, SKEY)` для инициатора и
//! `HASH('keyB', S, SKEY)` для принимающего; первые 1024 байта RC4
//! отбрасываются. `SKEY` для BitTorrent — info-hash раздачи, поэтому
//! принимающая сторона узнаёт, к какому торренту относится соединение, ещё до
//! расшифровки полезной нагрузки.

use std::cmp::Ordering;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use rand::{Rng, RngCore};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::metainfo::InfoHash;

/// Сколько байт RC4 выбрасывается перед использованием (так в спецификации).
const RC4_DISCARD: usize = 1024;
/// Максимальная длина паддинга одной стороны.
const MAX_PAD: usize = 512;
/// Размер простого числа и ключей DH в байтах.
const DH_BYTES: usize = 96;
/// Длина приватного ключа DH: 160 бит — рекомендованный компромисс спецификации.
const DH_PRIVATE_BYTES: usize = 20;
/// Бит «открытый текст» в `crypto_provide`.
pub const PROVIDE_PLAINTEXT: u32 = 0x01;
/// Бит «RC4-шифрование» в `crypto_provide`.
pub const PROVIDE_RC4: u32 = 0x02;
/// Ограничение на любое чтение при рукопожатии.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Арифметика 768-битных чисел
// ---------------------------------------------------------------------------

/// 768-битное целое: 12 машинных слов по 64 бита, младшее — первым.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Big([u64; 12]);

/// То же число с одним запасным словом: нужно при сдвиге влево в остатке.
type Wide = [u64; 13];

impl Big {
    /// Разбор big-endian байтов ровно на 96 байт.
    fn from_be(bytes: &[u8]) -> Big {
        debug_assert_eq!(bytes.len(), DH_BYTES);
        let mut limbs = [0u64; 12];
        for (word, chunk) in limbs.iter_mut().rev().zip(bytes.chunks(8)) {
            let mut value = 0u64;
            for byte in chunk {
                value = (value << 8) | u64::from(*byte);
            }
            *word = value;
        }
        Big(limbs)
    }

    /// Число в big-endian виде без ведущих нулей (96 байт).
    fn to_be(self) -> [u8; DH_BYTES] {
        let mut out = [0u8; DH_BYTES];
        for (chunk, word) in out.chunks_mut(8).zip(self.0.iter().rev()) {
            let bytes = word.to_be_bytes();
            chunk.copy_from_slice(&bytes);
        }
        out
    }

    /// Разбор шестнадцатеричной строки (для константы простого числа).
    fn from_hex(text: &str) -> Big {
        let cleaned: Vec<u8> = text
            .chars()
            .filter(|ch| ch.is_ascii_hexdigit())
            .map(|ch| ch.to_digit(16).unwrap_or(0) as u8)
            .collect();
        let mut limbs = [0u64; 12];
        // Собираем с конца: каждая пара hex-цифр — байт, 8 байт — слово.
        let mut bit_index = 0usize;
        for digit in cleaned.iter().rev() {
            let limb = bit_index / 64;
            if limb >= 12 {
                break;
            }
            limbs[limb] |= u64::from(*digit) << ((bit_index % 64) as u32);
            bit_index += 4;
        }
        Big(limbs)
    }

    fn cmp_wide(self, other: Big) -> Ordering {
        for index in (0..12).rev() {
            match self.0[index].cmp(&other.0[index]) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        Ordering::Equal
    }

    fn is_zero(self) -> bool {
        self.0.iter().all(|word| *word == 0)
    }

    /// Умножение: 12 + 12 = 24 слова результата.
    fn mul(self, other: Big) -> [u64; 24] {
        let mut product = [0u64; 24];
        for (left_index, left) in self.0.iter().enumerate() {
            if *left == 0 {
                continue;
            }
            let mut carry: u128 = 0;
            for (right_index, right) in other.0.iter().enumerate() {
                let slot = left_index + right_index;
                let accumulated =
                    u128::from(product[slot]) + u128::from(*left) * u128::from(*right) + carry;
                product[slot] = accumulated as u64;
                carry = accumulated >> 64;
            }
            product[left_index + 12] = carry as u64;
        }
        product
    }

    /// Остаток от деления произведения на модуль (сдвиг и вычитание по битам).
    fn rem_product(product: [u64; 24], modulus: Big) -> Big {
        let mut remainder: Wide = [0u64; 13];
        let modulus_wide: Wide = {
            let mut wide = [0u64; 13];
            wide[..12].copy_from_slice(&modulus.0);
            wide
        };
        for bit in (0..1536).rev() {
            // remainder <<= 1 с переносом в тринадцатое слово
            let mut carry = 0u64;
            for word in remainder.iter_mut() {
                let next = *word >> 63;
                *word = (*word << 1) | carry;
                carry = next;
            }
            let source = product[bit / 64];
            remainder[0] |= (source >> (bit % 64)) & 1;
            if cmp_wide(&remainder, &modulus_wide) != Ordering::Less {
                sub_wide(&mut remainder, &modulus_wide);
            }
        }
        let mut limbs = [0u64; 12];
        limbs.copy_from_slice(&remainder[..12]);
        Big(limbs)
    }
}

fn cmp_wide(left: &Wide, right: &Wide) -> Ordering {
    for index in (0..13).rev() {
        match left[index].cmp(&right[index]) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// `left -= right` для 13-словных чисел; вызывать только когда `left >= right`.
fn sub_wide(left: &mut Wide, right: &Wide) {
    let mut borrow = 0u64;
    for index in 0..13 {
        let (value, first) = left[index].overflowing_sub(right[index]);
        let (value, second) = value.overflowing_sub(borrow);
        left[index] = value;
        borrow = u64::from(first || second);
    }
}

/// Модульное возведение в степень по big-endian показателю.
fn mod_exp(base: Big, exponent: &[u8], modulus: Big) -> Big {
    let mut result = Big::from_hex("01");
    for byte in exponent {
        for bit in (0..8).rev() {
            result = Big::rem_product(result.mul(result), modulus);
            if (byte >> bit) & 1 == 1 {
                result = Big::rem_product(result.mul(base), modulus);
            }
        }
    }
    result
}

/// Простое число DH из спецификации MSE (768-бит, генератор 2).
fn prime() -> Big {
    Big::from_hex(
        "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1\
         29024E088A67CC74020BBEA63B139B22514A08798E3404DD\
         EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245\
         E485B576625E7EC6F44C42E9A63A36210000000000090563",
    )
}

/// Пара DH: приватный ключ (20 байт) и открытый (96 байт).
fn dh_keypair() -> ([u8; DH_PRIVATE_BYTES], [u8; DH_BYTES]) {
    let mut private = [0u8; DH_PRIVATE_BYTES];
    rand::thread_rng().fill_bytes(&mut private);
    // Нулевой показатель дал бы открытый ключ 1 — такую пару не используем.
    private[0] |= 1;
    let generator = Big::from_hex("02");
    let public = mod_exp(generator, &private, prime()).to_be();
    (private, public)
}

// ---------------------------------------------------------------------------
// RC4 и хэши
// ---------------------------------------------------------------------------

/// RC4 с непрерывным потоком: состояние живёт столько же, сколько соединение.
///
/// Спецификация MSE требует отбросить первые 1024 байта ключевого потока —
/// это делает [`Rc4::new`]. Для тестов есть [`Rc4::new_without_discard`].
#[derive(Clone)]
struct Rc4 {
    table: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    fn new(key: &[u8]) -> Rc4 {
        let mut rc4 = Rc4::new_without_discard(key);
        let mut discard = [0u8; RC4_DISCARD];
        rc4.apply(&mut discard);
        rc4
    }

    fn new_without_discard(key: &[u8]) -> Rc4 {
        assert!(!key.is_empty(), "пустой ключ RC4 недопустим");
        let mut table = [0u8; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            *slot = index as u8;
        }
        let mut j = 0u8;
        for index in 0..256 {
            j = j
                .wrapping_add(table[index])
                .wrapping_add(key[index % key.len()]);
            table.swap(index, usize::from(j));
        }
        Rc4 { table, i: 0, j: 0 }
    }

    fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.table[usize::from(self.i)]);
            self.table.swap(usize::from(self.i), usize::from(self.j));
            let index =
                self.table[usize::from(self.i)].wrapping_add(self.table[usize::from(self.j)]);
            *byte ^= self.table[usize::from(index)];
        }
    }
}

/// SHA-1 от конкатенации частей (так считаются все константы MSE).
fn hash(parts: &[&[u8]]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

// ---------------------------------------------------------------------------
// Поток с шифрованием
// ---------------------------------------------------------------------------

/// Соединение после рукопожатия MSE: читает/пишет через RC4 (или открыто).
pub struct MseStream<S = TcpStream> {
    inner: S,
    /// Входящий поток: ключ собеседника. `None` — выбран открытый режим.
    inbound: Option<Rc4>,
    /// Исходящий поток: наш ключ.
    outbound: Option<Rc4>,
    /// Уже расшифрованные байты, полученные в рукопожатии (начальные данные).
    pending_in: Vec<u8>,
    pending_in_pos: usize,
    /// Зашифрованные, но ещё не отправленные байты (частичная запись).
    pending_out: Vec<u8>,
    pending_out_pos: usize,
}

impl<S> MseStream<S> {
    fn new(inner: S, inbound: Option<Rc4>, outbound: Option<Rc4>) -> MseStream<S> {
        MseStream {
            inner,
            inbound,
            outbound,
            pending_in: Vec::new(),
            pending_in_pos: 0,
            pending_out: Vec::new(),
            pending_out_pos: 0,
        }
    }

    fn with_pending_in(mut self, pending: Vec<u8>) -> MseStream<S> {
        self.pending_in = pending;
        self
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for MseStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pending_in_pos < self.pending_in.len() {
            let available = &self.pending_in[self.pending_in_pos..];
            let count = available.len().min(buf.remaining());
            buf.put_slice(&available[..count]);
            self.pending_in_pos += count;
            return Poll::Ready(Ok(()));
        }
        let this = &mut *self;
        let mut inner_buf = ReadBuf::new(buf.initialize_unfilled());
        match Pin::new(&mut this.inner).poll_read(cx, &mut inner_buf) {
            Poll::Ready(Ok(())) => {
                let filled = inner_buf.filled().len();
                if filled > 0 {
                    let data = inner_buf.filled_mut();
                    if let Some(rc4) = this.inbound.as_mut() {
                        rc4.apply(data);
                    }
                    buf.advance(filled);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for MseStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = &mut *self;
        // Сначала досылаем хвост предыдущей записи: RC4 обязан идти по порядку.
        if this.pending_out_pos < this.pending_out.len() {
            match flush_pending(this, cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            }
        }
        let mut encrypted = Vec::with_capacity(buf.len());
        encrypted.extend_from_slice(buf);
        if let Some(rc4) = this.outbound.as_mut() {
            rc4.apply(&mut encrypted);
        }
        this.pending_out = encrypted;
        this.pending_out_pos = 0;
        match flush_pending(this, cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        match flush_pending(this, cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        match flush_pending(this, cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_shutdown(cx),
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    }
}

/// Досылает `pending_out`, сохраняя смещение при частичной записи.
fn flush_pending<S: AsyncWrite + Unpin>(
    this: &mut MseStream<S>,
    cx: &mut Context<'_>,
) -> Poll<std::io::Result<()>> {
    while this.pending_out_pos < this.pending_out.len() {
        let chunk = &this.pending_out[this.pending_out_pos..];
        match Pin::new(&mut this.inner).poll_write(cx, chunk) {
            Poll::Ready(Ok(0)) => {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "соединение не принимает данные",
                )))
            }
            Poll::Ready(Ok(written)) => this.pending_out_pos += written,
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Pending => return Poll::Pending,
        }
    }
    this.pending_out.clear();
    this.pending_out_pos = 0;
    Poll::Ready(Ok(()))
}

// ---------------------------------------------------------------------------
// Рукопожатие
// ---------------------------------------------------------------------------

/// Итог рукопожатия: поток и выбранный способ защиты.
pub struct Handshake<S> {
    pub stream: MseStream<S>,
    /// `true` — полезная нагрузка шифруется RC4, `false` — открытый режим.
    pub rc4: bool,
}

/// Читает ровно `buf.len()` байт с таймаутом.
async fn read_exact<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<(), String> {
    tokio::time::timeout(timeout, tokio::io::AsyncReadExt::read_exact(stream, buf))
        .await
        .map_err(|_| "таймаут чтения при шифрованном рукопожатии".to_string())?
        .map_err(|err| format!("шифрованное рукопожатие: {err}"))
        .map(|_| ())
}

/// Дочитывает в буфер хотя бы до `target` байт (буфер может уже быть длиннее).
async fn read_until<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    target: usize,
    timeout: Duration,
) -> Result<(), String> {
    while buf.len() < target {
        let mut chunk = [0u8; 256];
        let want = (target - buf.len()).min(chunk.len());
        read_exact(stream, &mut chunk[..want], timeout).await?;
        buf.extend_from_slice(&chunk[..want]);
    }
    Ok(())
}

/// Готовит исходящий RC4-ключ стороны A/B.
fn outbound_key(salt: &[u8], secret: &[u8], skey: &[u8]) -> [u8; 20] {
    hash(&[salt, secret, skey])
}

/// Ищет начало зашифрованного блока (8 нулевых байт после расшифровки).
///
/// Паддинг собеседника имеет неизвестную длину, поэтому кандидаты
/// перебираются по мере поступления байтов: каждый сдвиг проверяется один раз
/// новым RC4 (он обязан начинаться с того же ключевого потока).
async fn scan_vc<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    key: &[u8; 20],
) -> Result<(usize, Rc4), String> {
    let mut offset = DH_BYTES;
    loop {
        while offset + 8 <= buf.len() && offset <= DH_BYTES + MAX_PAD {
            let mut candidate = Rc4::new(key);
            let mut block = [0u8; 8];
            block.copy_from_slice(&buf[offset..offset + 8]);
            candidate.apply(&mut block);
            if block == [0u8; 8] {
                return Ok((offset, candidate));
            }
            offset += 1;
        }
        if offset > DH_BYTES + MAX_PAD {
            return Err("пир не подтвердил шифрованное рукопожатие".to_string());
        }
        read_more(stream, buf, 64).await?;
    }
}

/// Ищет `HASH('req1', S)` после DH-ключа инициатора.
async fn scan_req1<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    req1: &[u8; 20],
) -> Result<usize, String> {
    let mut offset = DH_BYTES;
    loop {
        while offset + 20 <= buf.len() && offset <= DH_BYTES + MAX_PAD {
            if buf[offset..offset + 20] == *req1 {
                return Ok(offset);
            }
            offset += 1;
        }
        if offset > DH_BYTES + MAX_PAD {
            return Err("пир не прислал хэш DH-секрета".to_string());
        }
        read_more(stream, buf, 64).await?;
    }
}

/// Читает сколько пришло (не больше `want`) и добавляет это в буфер.
///
/// Именно частичное чтение, а не `read_exact`: собеседник мог уже закончить
/// рукопожатие и ждать полезную нагрузку, поэтому требовать сразу `want` байт
/// нельзя — это дало бы ложный таймаут.
async fn read_more<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    want: usize,
) -> Result<(), String> {
    let mut chunk = vec![0u8; want];
    let read = tokio::time::timeout(
        READ_TIMEOUT,
        tokio::io::AsyncReadExt::read(stream, &mut chunk),
    )
    .await
    .map_err(|_| "таймаут чтения при шифрованном рукопожатии".to_string())?
    .map_err(|err| format!("шифрованное рукопожатие: {err}"))?;
    if read == 0 {
        return Err("соединение закрыто во время рукопожатия".to_string());
    }
    buf.extend_from_slice(&chunk[..read]);
    Ok(())
}

/// Исходящее рукопожатие (мы инициатор)./// Исходящее рукопожатие (мы инициатор).
///
/// `provide` — какие способы мы поддерживаем; для обычного клиента это
/// `PROVIDE_RC4` (в режиме «только шифрование») или `PROVIDE_RC4 | PROVIDE_PLAINTEXT`.
pub async fn initiate<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    info_hash: &InfoHash,
    provide: u32,
) -> Result<Handshake<S>, String> {
    let (private, public) = dh_keypair();

    // Шаг 1: Ya и случайный паддинг.
    let mut step1 = Vec::with_capacity(DH_BYTES + MAX_PAD);
    step1.extend_from_slice(&public);
    let pad_len = rand::thread_rng().gen_range(0..=MAX_PAD);
    let mut pad_a = vec![0u8; pad_len];
    rand::thread_rng().fill_bytes(&mut pad_a);
    step1.extend_from_slice(&pad_a);
    tokio::time::timeout(
        timeout_write(),
        tokio::io::AsyncWriteExt::write_all(&mut stream, &step1),
    )
    .await
    .map_err(|_| "таймаут записи при шифрованном рукопожатии".to_string())?
    .map_err(|err| format!("шифрованное рукопожатие: {err}"))?;

    // Шаг 2: Yb (паддинг пропускаем поиском начала зашифрованного блока).
    let mut buf = Vec::with_capacity(1024);
    read_until(&mut stream, &mut buf, DH_BYTES, READ_TIMEOUT).await?;
    let mut remote_public = [0u8; DH_BYTES];
    remote_public.copy_from_slice(&buf[..DH_BYTES]);
    let secret = {
        let yb = Big::from_be(&remote_public);
        let modulus = prime();
        if yb.is_zero() || yb.cmp_wide(modulus) != Ordering::Less {
            return Err("пир прислал некорректный DH-ключ".to_string());
        }
        mod_exp(yb, &private, modulus).to_be()
    };
    let skey = info_hash.0;
    let key_a = outbound_key(b"keyA", &secret, &skey);
    let key_b = outbound_key(b"keyB", &secret, &skey);

    // Шаг 3: хэш секрета, идентификатор раздачи и зашифрованный блок с
    // нашими возможностями. Паддинги и начальные данные не нужны — оставляем
    // их нулевой длины, как разрешает спецификация.
    let req1 = hash(&[b"req1", &secret]);
    let req3 = hash(&[b"req3", &secret]);
    let req2 = hash(&[b"req2", &skey]);
    let req2_xor: Vec<u8> = req2.iter().zip(req3.iter()).map(|(a, b)| a ^ b).collect();
    let mut outbound = Rc4::new(&key_a);
    let mut block = Vec::with_capacity(16);
    block.extend_from_slice(&[0u8; 8]);
    block.extend_from_slice(&provide.to_be_bytes());
    block.extend_from_slice(&0u16.to_be_bytes());
    block.extend_from_slice(&0u16.to_be_bytes());
    outbound.apply(&mut block);
    let mut step3 = Vec::with_capacity(40 + block.len());
    step3.extend_from_slice(&req1);
    step3.extend_from_slice(&req2_xor);
    step3.extend_from_slice(&block);
    tokio::time::timeout(
        timeout_write(),
        tokio::io::AsyncWriteExt::write_all(&mut stream, &step3),
    )
    .await
    .map_err(|_| "таймаут записи при шифрованном рукопожатии".to_string())?
    .map_err(|err| format!("шифрованное рукопожатие: {err}"))?;

    // Ищем VC: 8 нулевых байт после расшифровки.
    let (vc_offset, mut inbound) = scan_vc(&mut stream, &mut buf, &key_b).await?;
    // Досчитываем хвост блока: select(4), lenPadD(2), PadD.
    read_until(&mut stream, &mut buf, vc_offset + 8 + 4 + 2, READ_TIMEOUT).await?;
    let mut select_bytes = [0u8; 4];
    select_bytes.copy_from_slice(&buf[vc_offset + 8..vc_offset + 12]);
    inbound.apply(&mut select_bytes);
    let select = u32::from_be_bytes(select_bytes);
    let mut len_bytes = [0u8; 2];
    len_bytes.copy_from_slice(&buf[vc_offset + 12..vc_offset + 14]);
    inbound.apply(&mut len_bytes);
    let pad_d = usize::from(u16::from_be_bytes(len_bytes));
    if pad_d > MAX_PAD {
        return Err("слишком длинный паддинг в ответе пира".to_string());
    }
    read_until(&mut stream, &mut buf, vc_offset + 14 + pad_d, READ_TIMEOUT).await?;
    // PadD расшифровываем только чтобы продвинуть RC4-поток: данные паддинга
    // не нужны.
    let mut pad_d_bytes = buf[vc_offset + 14..vc_offset + 14 + pad_d].to_vec();
    inbound.apply(&mut pad_d_bytes);
    // Всё, что пришло после паддинга, — уже полезная нагрузка (например,
    // handshake BitTorrent), и она зашифрована тем же потоком.
    let already_read = buf.len().saturating_sub(vc_offset + 14 + pad_d);
    let mut pending_tail = buf[vc_offset + 14 + pad_d..].to_vec();
    if already_read > 0 {
        inbound.apply(&mut pending_tail);
    }

    if select != 0 && select & !provide != 0 {
        return Err("пир выбрал неподдерживаемый способ шифрования".to_string());
    }
    if select == 0 {
        return Err("пир не выбрал способ шифрования".to_string());
    }
    let rc4 = select & PROVIDE_RC4 != 0;
    let outbound = if rc4 { Some(outbound) } else { None };
    let inbound = if rc4 { Some(inbound) } else { None };
    let stream = MseStream::new(stream, inbound, outbound).with_pending_in(pending_tail);
    Ok(Handshake { stream, rc4 })
}

/// Входящее рукопожатие (мы принимающая сторона).
///
/// `known` — info-hash всех раздач, которые мы обслуживаем: по ним находится
/// `SKEY`. Если ни один не подошёл, соединение отклоняется.
pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    known: &[InfoHash],
    provide: u32,
    require_rc4: bool,
) -> Result<Handshake<S>, String> {
    // Шаг 1: Ya и паддинг до HASH('req1', S).
    let mut buf = Vec::with_capacity(1024);
    read_until(&mut stream, &mut buf, DH_BYTES, READ_TIMEOUT).await?;
    let mut remote_public = [0u8; DH_BYTES];
    remote_public.copy_from_slice(&buf[..DH_BYTES]);
    let (private, public) = dh_keypair();
    let secret = {
        let ya = Big::from_be(&remote_public);
        let modulus = prime();
        if ya.is_zero() || ya.cmp_wide(modulus) != Ordering::Less {
            return Err("пир прислал некорректный DH-ключ".to_string());
        }
        mod_exp(ya, &private, modulus).to_be()
    };
    // Шаг 2: наш DH-ключ и случайный паддинг.
    let mut step2 = Vec::with_capacity(DH_BYTES + MAX_PAD);
    step2.extend_from_slice(&public);
    let pad_len = rand::thread_rng().gen_range(0..=MAX_PAD);
    let mut pad_b = vec![0u8; pad_len];
    rand::thread_rng().fill_bytes(&mut pad_b);
    step2.extend_from_slice(&pad_b);
    tokio::time::timeout(
        timeout_write(),
        tokio::io::AsyncWriteExt::write_all(&mut stream, &step2),
    )
    .await
    .map_err(|_| "таймаут записи при шифрованном рукопожатии".to_string())?
    .map_err(|err| format!("шифрованное рукопожатие: {err}"))?;

    let req1 = hash(&[b"req1", &secret]);

    // Ищем req1 после паддинга A.
    let req_offset = scan_req1(&mut stream, &mut buf, &req1).await?;
    read_until(&mut stream, &mut buf, req_offset + 40, READ_TIMEOUT).await?;
    let mut skey_hash = [0u8; 20];
    skey_hash.copy_from_slice(&buf[req_offset + 20..req_offset + 40]);
    let req3 = hash(&[b"req3", &secret]);
    let matched = known.iter().find(|hash_candidate| {
        let req2 = hash(&[b"req2", &hash_candidate.0]);
        let expected: Vec<u8> = req2.iter().zip(req3.iter()).map(|(a, b)| a ^ b).collect();
        expected == skey_hash
    });
    let Some(info_hash) = matched else {
        return Err("шифрованное соединение относится к неизвестной раздаче".to_string());
    };
    let key_a = outbound_key(b"keyA", &secret, &info_hash.0);
    let key_b = outbound_key(b"keyB", &secret, &info_hash.0);

    // Блок инициатора: VC(8), provide(4), lenPadC(2), PadC, lenIA(2), IA.
    let mut inbound = Rc4::new(&key_a);
    read_until(&mut stream, &mut buf, req_offset + 40 + 8, READ_TIMEOUT).await?;
    let mut vc = [0u8; 8];
    vc.copy_from_slice(&buf[req_offset + 40..req_offset + 48]);
    inbound.apply(&mut vc);
    if vc != [0u8; 8] {
        return Err("пир не знает DH-секрет (неверный VC)".to_string());
    }
    read_until(&mut stream, &mut buf, req_offset + 48 + 4, READ_TIMEOUT).await?;
    let mut provide_bytes = [0u8; 4];
    provide_bytes.copy_from_slice(&buf[req_offset + 48..req_offset + 52]);
    inbound.apply(&mut provide_bytes);
    let remote_provide = u32::from_be_bytes(provide_bytes);
    if remote_provide & PROVIDE_RC4 == 0 && remote_provide & PROVIDE_PLAINTEXT == 0 {
        return Err("пир не предложил ни одного способа шифрования".to_string());
    }
    read_until(&mut stream, &mut buf, req_offset + 52 + 2, READ_TIMEOUT).await?;
    let mut len_c = [0u8; 2];
    len_c.copy_from_slice(&buf[req_offset + 52..req_offset + 54]);
    inbound.apply(&mut len_c);
    let pad_c = usize::from(u16::from_be_bytes(len_c));
    if pad_c > MAX_PAD {
        return Err("слишком длинный паддинг в запросе пира".to_string());
    }
    read_until(
        &mut stream,
        &mut buf,
        req_offset + 54 + pad_c + 2,
        READ_TIMEOUT,
    )
    .await?;
    let mut pad_bytes = buf[req_offset + 54..req_offset + 54 + pad_c].to_vec();
    inbound.apply(&mut pad_bytes);
    let mut len_ia = [0u8; 2];
    len_ia.copy_from_slice(&buf[req_offset + 54 + pad_c..req_offset + 54 + pad_c + 2]);
    inbound.apply(&mut len_ia);
    let ia_len = usize::from(u16::from_be_bytes(len_ia));
    read_until(
        &mut stream,
        &mut buf,
        req_offset + 54 + pad_c + 2 + ia_len,
        READ_TIMEOUT,
    )
    .await?;
    let mut ia = buf[req_offset + 54 + pad_c + 2..req_offset + 54 + pad_c + 2 + ia_len].to_vec();
    inbound.apply(&mut ia);

    // Выбираем способ: RC4, если он предложен и разрешён; иначе открытый.
    let rc4 = if require_rc4 {
        if remote_provide & PROVIDE_RC4 == 0 {
            return Err("пир не поддерживает шифрование, а оно обязательно".to_string());
        }
        true
    } else if remote_provide & PROVIDE_RC4 != 0 && provide & PROVIDE_RC4 != 0 {
        true
    } else if remote_provide & PROVIDE_PLAINTEXT != 0 && provide & PROVIDE_PLAINTEXT != 0 {
        false
    } else {
        return Err("нет общего способа шифрования с пиром".to_string());
    };
    let select = if rc4 { PROVIDE_RC4 } else { PROVIDE_PLAINTEXT };

    // Шаг 4: VC, select, пустой PadD — всё зашифровано ключом B.
    let mut outbound = Rc4::new(&key_b);
    let mut reply = Vec::with_capacity(14);
    reply.extend_from_slice(&[0u8; 8]);
    reply.extend_from_slice(&select.to_be_bytes());
    reply.extend_from_slice(&0u16.to_be_bytes());
    outbound.apply(&mut reply);
    tokio::time::timeout(
        timeout_write(),
        tokio::io::AsyncWriteExt::write_all(&mut stream, &reply),
    )
    .await
    .map_err(|_| "таймаут записи при шифрованном рукопожатии".to_string())?
    .map_err(|err| format!("шифрованное рукопожатие: {err}"))?;

    let outbound = if rc4 { Some(outbound) } else { None };
    let inbound = if rc4 { Some(inbound) } else { None };
    Ok(Handshake {
        stream: MseStream::new(stream, inbound, outbound).with_pending_in(ia),
        rc4,
    })
}

fn timeout_write() -> Duration {
    Duration::from_secs(10)
}

impl MseStream<TcpStream> {
    /// Отключает буферизацию TCP: для peer wire это заметно снижает задержки.
    pub fn set_nodelay(&self, value: bool) -> std::io::Result<()> {
        self.inner.set_nodelay(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_hash(byte: u8) -> InfoHash {
        InfoHash([byte; 20])
    }

    #[test]
    fn rc4_matches_known_vector() {
        // Классический тестовый вектор RC4 (без отбрасывания 1024 байт).
        let mut rc4 = Rc4::new_without_discard(b"Key");
        let mut data = b"Plaintext".to_vec();
        rc4.apply(&mut data);
        assert_eq!(data, hex("BBF316E8D940AF0AD3"));
    }

    #[test]
    fn mod_exp_matches_small_numbers() {
        // 5^6 mod 23 = 8 (учебный пример DH).
        let base = Big::from_hex("05");
        let modulus = Big::from_hex("17");
        let result = mod_exp(base, &[6], modulus);
        assert_eq!(result.to_be()[DH_BYTES - 1], 8);
    }

    #[test]
    fn prime_has_expected_top_bytes() {
        let bytes = prime().to_be();
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[DH_BYTES - 1], 0x63);
    }

    #[test]
    fn dh_shared_secret_is_symmetric() {
        let (private_a, public_a) = dh_keypair();
        let (private_b, public_b) = dh_keypair();
        let shared_a = mod_exp(Big::from_be(&public_b), &private_a, prime());
        let shared_b = mod_exp(Big::from_be(&public_a), &private_b, prime());
        assert_eq!(shared_a.to_be(), shared_b.to_be());
    }

    /// Полный обмен рукопожатием на двух «сокетах» в памяти.
    async fn handshake_pair(
        require_rc4: bool,
    ) -> (
        Handshake<tokio::io::DuplexStream>,
        Handshake<tokio::io::DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(4096);
        let known = [info_hash(7)];
        let server_task = tokio::spawn(async move {
            accept(server, &known, PROVIDE_RC4 | PROVIDE_PLAINTEXT, require_rc4).await
        });
        let client_result = initiate(client, &info_hash(7), PROVIDE_RC4 | PROVIDE_PLAINTEXT)
            .await
            .expect("инициатор должен завершить рукопожатие");
        let server_result = server_task.await.expect("задача не упала");
        (
            client_result,
            server_result.expect("принимающая сторона должна завершить рукопожатие"),
        )
    }

    #[tokio::test]
    async fn encrypted_stream_roundtrips_data() {
        let (mut client, mut server) = handshake_pair(true).await;
        assert!(client.rc4 && server.rc4, "должен выбраться RC4");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let payload = b"\x13BitTorrent protocol" as &[u8];
        client
            .stream
            .write_all(payload)
            .await
            .expect("запись шифрованного потока");
        let mut received = vec![0u8; payload.len()];
        server
            .stream
            .read_exact(&mut received)
            .await
            .expect("чтение шифрованного потока");
        assert_eq!(received, payload);

        let answer = b"0123456789";
        server
            .stream
            .write_all(answer)
            .await
            .expect("ответ по шифрованному потоку");
        let mut back = vec![0u8; answer.len()];
        client
            .stream
            .read_exact(&mut back)
            .await
            .expect("чтение ответа");
        assert_eq!(back, answer);
    }

    #[tokio::test]
    async fn plaintext_mode_stays_readable() {
        // Открытый режим выбирается только если RC4 никто не предложил: при
        // возможности шифрования клиент предпочитает его (как qBittorrent).
        let (client, server) = tokio::io::duplex(4096);
        let known = [info_hash(5)];
        let server_task = tokio::spawn(async move {
            accept(server, &known, PROVIDE_RC4 | PROVIDE_PLAINTEXT, false).await
        });
        let client_result = initiate(client, &info_hash(5), PROVIDE_PLAINTEXT)
            .await
            .expect("инициатор должен завершить рукопожатие");
        let server_result = server_task
            .await
            .expect("задача не упала")
            .expect("принимающая сторона должна завершить рукопожатие");
        assert!(
            !client_result.rc4 && !server_result.rc4,
            "открытый режим не шифрует"
        );
    }

    #[tokio::test]
    async fn unknown_info_hash_is_rejected() {
        let (client, server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            // Другая раздача: SKEY не совпадёт.
            accept(server, &[info_hash(9)], PROVIDE_RC4, true).await
        });
        let _ = initiate(client, &info_hash(7), PROVIDE_RC4).await;
        let result = server_task.await.expect("задача не упала");
        assert!(result.is_err(), "соединение к чужой раздаче отклоняется");
    }

    #[tokio::test]
    async fn required_encryption_rejects_plaintext_only_peer() {
        let (client, server) = tokio::io::duplex(4096);
        let known = [info_hash(3)];
        let server_task = tokio::spawn(async move {
            accept(server, &known, PROVIDE_RC4 | PROVIDE_PLAINTEXT, true).await
        });
        // Инициатор предлагает только открытый режим.
        let _ = initiate(client, &info_hash(3), PROVIDE_PLAINTEXT).await;
        let result = server_task.await.expect("задача не упала");
        assert!(
            result.is_err(),
            "обязательное шифрование не пропускает plaintext"
        );
    }

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks(2)
            .map(|pair| {
                let high = (pair[0] as char).to_digit(16).expect("hex") as u8;
                let low = (pair[1] as char).to_digit(16).expect("hex") as u8;
                (high << 4) | low
            })
            .collect()
    }
}
