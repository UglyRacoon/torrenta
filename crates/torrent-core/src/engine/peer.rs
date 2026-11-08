//! Peer wire protocol: сессия с одним пиром и получение метаданных
//! magnet-ссылки по BEP 9.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use super::torrent::Runtime;
use super::{read_exact_timeout, REQUEST_WINDOW};
use crate::bencode::{self, Value};
use crate::metainfo::InfoHash;
use crate::wire::{Handshake, Message, HANDSHAKE_LEN};

/// Таймаут установки TCP-соединения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
/// Таймаут ожидания данных от пира.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Период служебных действий (запросы, keep-alive).
const TICK: Duration = Duration::from_millis(250);
/// Через сколько молчания отправляем keep-alive.
const KEEPALIVE: Duration = Duration::from_secs(100);
/// Максимальный размер сообщения, который мы готовы принять.
const MAX_MESSAGE: usize = 512 * 1024;
/// Локальный идентификатор ut_metadata в наших extended-сообщениях.
const OUR_UT_METADATA: u8 = 1;
/// Предельный размер метаданных, который принимаем от пира.
const MAX_METADATA: usize = 8 * 1024 * 1024;
/// Размер куска метаданных по BEP 9.
const METADATA_PIECE: usize = 16 * 1024;
/// Сколько запросов метаданных держим в полёте.
const METADATA_INFLIGHT: usize = 32;

/// Параметры получения метаданных.
pub(crate) struct MetadataFetch {
    pub(crate) info_hash: InfoHash,
    pub(crate) peer_id: [u8; 20],
    pub(crate) timeout: Duration,
}

// ---------------------------------------------------------------------------
// Подключение
// ---------------------------------------------------------------------------

/// Исходящее соединение с пиром из списка трекера.
pub(crate) async fn connect(runtime: Arc<Runtime>, addr: SocketAddr) {
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => stream,
        _ => {
            runtime.candidate_failed(addr);
            return;
        }
    };
    if let Err(message) = run(runtime.clone(), addr, stream, None).await {
        let _ = message;
    }
}

/// Входящее соединение: handshake уже прочитан при маршрутизации.
pub(crate) async fn accept(
    runtime: Arc<Runtime>,
    addr: SocketAddr,
    stream: TcpStream,
    remote: Handshake,
) {
    let _ = run(runtime, addr, stream, Some(remote)).await;
}

async fn run(
    runtime: Arc<Runtime>,
    addr: SocketAddr,
    stream: TcpStream,
    remote: Option<Handshake>,
) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();
    let ours = Handshake::with_extensions(runtime.meta.info_hash, runtime.env.peer_id);
    writer
        .write_all(&ours.encode())
        .await
        .map_err(|err| format!("handshake: {err}"))?;
    let remote = match remote {
        Some(handshake) => handshake,
        None => {
            let mut buf = [0u8; HANDSHAKE_LEN];
            read_exact_timeout(&mut reader, &mut buf, CONNECT_TIMEOUT)
                .await
                .map_err(|err| format!("handshake: {err}"))?;
            Handshake::parse(&buf).map_err(|err| format!("handshake: {err}"))?
        }
    };
    if remote.info_hash != runtime.meta.info_hash {
        return Err("пир относится к другому торренту".to_string());
    }
    let Some(id) = runtime.register(addr) else {
        return Ok(());
    };
    runtime.set_client(id, crate::peerid::client_name(&remote.peer_id));
    let have = runtime.have_bitfield();
    if have.count() > 0 {
        send(&mut writer, &Message::Bitfield(have)).await?;
    }
    if remote.supports_extended() {
        let payload = our_extended_handshake(&runtime);
        send(
            &mut writer,
            &Message::Extended {
                ext_id: 0,
                payload,
            },
        )
        .await?;
    }

    // Чтение вынесено в отдельную задачу: `read_exact` не переживает
    // отмену, поэтому в `select!` сообщения приходят уже целиком.
    let (tx, mut messages) = mpsc::channel::<Result<Message, String>>(64);
    let reader_task = tokio::spawn(async move {
        loop {
            match read_message(&mut reader, READ_TIMEOUT).await {
                Ok(message) => {
                    if tx.send(Ok(message)).await.is_err() {
                        break;
                    }
                }
                Err(message) => {
                    let _ = tx.send(Err(message)).await;
                    break;
                }
            }
        }
    });

    let result = match remote.supports_extended() {
        true => session_loop(&runtime, id, &mut messages, &mut writer, true).await,
        false => session_loop(&runtime, id, &mut messages, &mut writer, false).await,
    };
    reader_task.abort();
    runtime.unregister(id);
    result
}

fn our_extended_handshake(runtime: &Runtime) -> Vec<u8> {
    let mut extensions = BTreeMap::new();
    extensions.insert(
        b"ut_metadata".to_vec(),
        Value::Int(OUR_UT_METADATA as i64),
    );
    let mut dict = BTreeMap::new();
    dict.insert(b"m".to_vec(), Value::Dict(extensions));
    dict.insert(
        b"metadata_size".to_vec(),
        Value::Int(runtime.meta.raw_info.len() as i64),
    );
    dict.insert(b"reqq".to_vec(), Value::Int(64));
    dict.insert(b"v".to_vec(), Value::string("Torrenta 1.0"));
    bencode::encode(&Value::Dict(dict))
}

// ---------------------------------------------------------------------------
// Цикл сессии
// ---------------------------------------------------------------------------

enum Ev {
    Message(Option<Result<Message, String>>),
    Have(Result<u32, RecvError>),
    Tick,
}

async fn session_loop(
    runtime: &Arc<Runtime>,
    id: u64,
    messages: &mut mpsc::Receiver<Result<Message, String>>,
    writer: &mut OwnedWriteHalf,
    extended: bool,
) -> Result<(), String> {
    let mut have_rx = runtime.have_tx.subscribe();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_write = Instant::now();
    let mut interested = false;

    loop {
        if runtime.stopping() || runtime.flags.paused() {
            return Ok(());
        }
        let event = tokio::select! {
            message = messages.recv() => Ev::Message(message),
            have = have_rx.recv() => Ev::Have(have),
            _ = ticker.tick() => Ev::Tick,
        };
        match event {
            Ev::Message(None) => return Ok(()),
            Ev::Message(Some(Err(message))) => return Err(message),
            Ev::Message(Some(Ok(Message::KeepAlive))) => {}
            Ev::Message(Some(Ok(message))) => {
                handle_message(runtime, id, writer, message, extended, &mut last_write).await?;
            }
            Ev::Have(Ok(index)) => {
                send(writer, &Message::Have(index)).await?;
                last_write = Instant::now();
            }
            Ev::Have(Err(_)) => {}
            Ev::Tick => {
                tick(runtime, id, writer, &mut interested, &mut last_write).await?;
            }
        }
    }
}

async fn handle_message(
    runtime: &Arc<Runtime>,
    id: u64,
    writer: &mut OwnedWriteHalf,
    message: Message,
    extended: bool,
    last_write: &mut Instant,
) -> Result<(), String> {
    match message {
        Message::KeepAlive => {}
        Message::Choke => runtime.set_peer_choking(id, true),
        Message::Unchoke => runtime.set_peer_choking(id, false),
        Message::Interested => runtime.set_peer_interested(id, true),
        Message::NotInterested => runtime.set_peer_interested(id, false),
        Message::Have(index) => runtime.on_have(id, index),
        Message::Bitfield(bitfield) => runtime.on_bitfield(id, bitfield),
        Message::Request {
            index,
            begin,
            length,
        } => {
            // Отказ в отдаче (кусок не скачан, пир не разблокирован) — не ошибка
            // протокола: запрос просто игнорируется, соединение остаётся живым.
            if let Ok(data) = runtime.upload(index, begin, length) {
                let sent = data.len();
                send(
                    writer,
                    &Message::Piece {
                        index,
                        begin,
                        data,
                    },
                )
                .await?;
                *last_write = Instant::now();
                runtime.add_uploaded(id, sent);
            }
        }
        Message::Piece {
            index,
            begin,
            data,
        } => {
            let ready = runtime.on_block(id, index, begin, data)?;
            if let Some((index, full)) = ready {
                if runtime.complete_piece(index, &full)? {
                    send(writer, &Message::Have(index)).await?;
                    *last_write = Instant::now();
                }
            }
        }
        Message::Cancel { .. } => {}
        Message::Port(_) => {}
        Message::Extended { ext_id, payload } => {
            if !extended {
                return Ok(());
            }
            match ext_id {
                0 => {
                    // Приветствие пира: нам достаточно факта поддержки расширений.
                }
                OUR_UT_METADATA => {
                    serve_metadata(runtime, writer, &payload).await?;
                    *last_write = Instant::now();
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Отвечает на запрос метаданных по BEP 9.
async fn serve_metadata(
    runtime: &Arc<Runtime>,
    writer: &mut OwnedWriteHalf,
    payload: &[u8],
) -> Result<(), String> {
    let (value, _) = match bencode::decode(payload) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(()),
    };
    let Some(dict) = value.as_dict() else {
        return Ok(());
    };
    if dict.get(b"msg_type".as_slice()).and_then(Value::as_int) != Some(0) {
        return Ok(());
    }
    let Some(index) = dict.get(b"piece".as_slice()).and_then(Value::as_int) else {
        return Ok(());
    };
    let raw = &runtime.meta.raw_info;
    let start = match usize::try_from(index) {
        Ok(index) => index.saturating_mul(METADATA_PIECE),
        Err(_) => return Ok(()),
    };
    if start >= raw.len() {
        return Ok(());
    }
    let end = (start + METADATA_PIECE).min(raw.len());
    let mut header = BTreeMap::new();
    header.insert(b"msg_type".to_vec(), Value::Int(1));
    header.insert(b"piece".to_vec(), Value::Int(index));
    header.insert(b"total_size".to_vec(), Value::Int(raw.len() as i64));
    let mut body = bencode::encode(&Value::Dict(header));
    body.extend_from_slice(&raw[start..end]);
    send(
        writer,
        &Message::Extended {
            ext_id: OUR_UT_METADATA,
            payload: body,
        },
    )
    .await
}

/// Периодические действия: интерес, запросы блоков, keep-alive.
async fn tick(
    runtime: &Arc<Runtime>,
    id: u64,
    writer: &mut OwnedWriteHalf,
    interested: &mut bool,
    last_write: &mut Instant,
) -> Result<(), String> {
    let complete = runtime.is_complete();
    if complete {
        if *interested {
            send(writer, &Message::NotInterested).await?;
            runtime.set_am_interested(id, false);
            *interested = false;
            *last_write = Instant::now();
        }
    } else if !*interested && runtime.should_be_interested(id) {
        send(writer, &Message::Interested).await?;
        runtime.set_am_interested(id, true);
        *interested = true;
        *last_write = Instant::now();
    }

    let inflight = runtime.peer_inflight(id);
    let window = REQUEST_WINDOW.saturating_sub(inflight);
    let budget = runtime.download_budget(window);
    let requests = runtime.next_requests(id, budget);
    let requested = !requests.is_empty();
    for request in requests {
        send(
            writer,
            &Message::Request {
                index: request.index,
                begin: request.begin,
                length: request.length,
            },
        )
        .await?;
    }
    if requested {
        *last_write = Instant::now();
    } else if last_write.elapsed() >= KEEPALIVE {
        send(writer, &Message::KeepAlive).await?;
        *last_write = Instant::now();
    }
    Ok(())
}

/// Отправляет сообщение и сбрасывает буфер.
async fn send(writer: &mut OwnedWriteHalf, message: &Message) -> Result<(), String> {
    writer
        .write_all(&message.encode())
        .await
        .map_err(|err| format!("запись: {err}"))
}

async fn read_message(
    reader: &mut OwnedReadHalf,
    timeout: Duration,
) -> Result<Message, String> {
    let mut head = [0u8; 4];
    read_exact_timeout(reader, &mut head, timeout)
        .await
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::UnexpectedEof => "пир закрыл соединение".to_string(),
            _ => format!("чтение: {err}"),
        })?;
    let length = u32::from_be_bytes(head) as usize;
    if length > MAX_MESSAGE {
        return Err("слишком большое сообщение".to_string());
    }
    if length == 0 {
        return Ok(Message::KeepAlive);
    }
    let mut payload = vec![0u8; length];
    read_exact_timeout(reader, &mut payload, timeout)
        .await
        .map_err(|err| format!("чтение: {err}"))?;
    crate::wire::decode(&payload).map_err(|err| err.to_string())
}

// ---------------------------------------------------------------------------
// Метаданные magnet-ссылки (BEP 9/10)
// ---------------------------------------------------------------------------

/// Скачивает info-словарь у пира и проверяет его SHA-1.
pub(crate) async fn fetch_metadata(
    fetch: &MetadataFetch,
    addr: SocketAddr,
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + fetch.timeout;
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| "таймаут подключения".to_string())?
        .map_err(|err| format!("соединение: {err}"))?;
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();
    let ours = Handshake::with_extensions(fetch.info_hash, fetch.peer_id);
    writer
        .write_all(&ours.encode())
        .await
        .map_err(|err| format!("handshake: {err}"))?;
    let mut buf = [0u8; HANDSHAKE_LEN];
    read_exact_timeout(&mut reader, &mut buf, remaining(deadline)?)
        .await
        .map_err(|err| format!("handshake: {err}"))?;
    let remote = Handshake::parse(&buf).map_err(|err| format!("handshake: {err}"))?;
    if remote.info_hash != fetch.info_hash {
        return Err("пир относится к другому торренту".to_string());
    }
    if !remote.supports_extended() {
        return Err("пир не поддерживает расширения".to_string());
    }

    let mut extensions = BTreeMap::new();
    extensions.insert(b"ut_metadata".to_vec(), Value::Int(OUR_UT_METADATA as i64));
    let mut hello = BTreeMap::new();
    hello.insert(b"m".to_vec(), Value::Dict(extensions));
    hello.insert(b"v".to_vec(), Value::string("Torrenta 1.0"));
    writer
        .write_all(
            &Message::Extended {
                ext_id: 0,
                payload: bencode::encode(&Value::Dict(hello)),
            }
            .encode(),
        )
        .await
        .map_err(|err| format!("запись: {err}"))?;

    // Ждём расширенный handshake пира: он сообщает свой id ut_metadata
    // и размер info-словаря.
    let (their_ut, size) = loop {
        let message = read_message(&mut reader, remaining(deadline)?).await?;
        let Message::Extended { ext_id: 0, payload } = message else {
            continue;
        };
        let (value, _) = bencode::decode(&payload).map_err(|err| format!("bencode: {err}"))?;
        let dict = value
            .as_dict()
            .ok_or_else(|| "некорректный extended handshake".to_string())?;
        let size = dict
            .get(b"metadata_size".as_slice())
            .and_then(Value::as_int)
            .unwrap_or(0);
        let their_ut = dict
            .get(b"m".as_slice())
            .and_then(Value::as_dict)
            .and_then(|m| m.get(b"ut_metadata".as_slice()))
            .and_then(Value::as_int)
            .unwrap_or(0);
        if size <= 0 || size as usize > MAX_METADATA || their_ut <= 0 {
            return Err("пир не может отдать метаданные".to_string());
        }
        break (their_ut as u8, size as usize);
    };

    let count = size.div_ceil(METADATA_PIECE);
    let mut data = vec![0u8; size];
    let mut got = vec![false; count];
    let mut wanted = 0usize;
    let mut received = 0usize;
    let mut outstanding = 0usize;

    // Первичная порция запросов.
    while outstanding < METADATA_INFLIGHT && wanted < count {
        request_metadata(&mut writer, their_ut, wanted).await?;
        wanted += 1;
        outstanding += 1;
    }

    while received < count {
        let message = read_message(&mut reader, remaining(deadline)?).await?;
        let Message::Extended { ext_id, payload } = message else {
            continue;
        };
        if ext_id != OUR_UT_METADATA {
            continue;
        }
        let (value, consumed) =
            bencode::decode(&payload).map_err(|err| format!("bencode: {err}"))?;
        let Some(dict) = value.as_dict() else {
            continue;
        };
        match dict.get(b"msg_type".as_slice()).and_then(Value::as_int) {
            Some(1) => {}
            Some(2) => return Err("пир отверг запрос метаданных".to_string()),
            _ => continue,
        }
        let Some(index) = dict.get(b"piece".as_slice()).and_then(Value::as_int) else {
            continue;
        };
        let Ok(index) = usize::try_from(index) else {
            return Err("некорректный номер куска метаданных".to_string());
        };
        if index >= count || got[index] {
            continue;
        }
        let body = &payload[consumed.min(payload.len())..];
        let start = index * METADATA_PIECE;
        let expected = METADATA_PIECE.min(size - start);
        if body.len() != expected {
            return Err("некорректная длина куска метаданных".to_string());
        }
        data[start..start + expected].copy_from_slice(body);
        got[index] = true;
        received += 1;
        outstanding = outstanding.saturating_sub(1);
        while outstanding < METADATA_INFLIGHT && wanted < count {
            request_metadata(&mut writer, their_ut, wanted).await?;
            wanted += 1;
            outstanding += 1;
        }
    }

    let mut hasher = Sha1::new();
    hasher.update(&data);
    let digest = hasher.finalize();
    if digest.as_slice() != fetch.info_hash.as_bytes().as_slice() {
        return Err("метаданные не совпали с info-hash".to_string());
    }
    Ok(data)
}

async fn request_metadata(
    writer: &mut OwnedWriteHalf,
    their_ut: u8,
    index: usize,
) -> Result<(), String> {
    let mut dict = BTreeMap::new();
    dict.insert(b"msg_type".to_vec(), Value::Int(0));
    dict.insert(b"piece".to_vec(), Value::Int(index as i64));
    writer
        .write_all(
            &Message::Extended {
                ext_id: their_ut,
                payload: bencode::encode(&Value::Dict(dict)),
            }
            .encode(),
        )
        .await
        .map_err(|err| format!("запись: {err}"))
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    let now = Instant::now();
    if now >= deadline {
        return Err("таймаут обмена метаданными".to_string());
    }
    Ok(deadline - now)
}


