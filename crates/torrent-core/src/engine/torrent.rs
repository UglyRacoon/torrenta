//! Рантайм одного торрента: циклы трекеров, кондуктор подключений,
//! сборка кусков и работа с диском.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use sha1::{Digest, Sha1};

use super::peer;
use super::{
    effective_limit, lk, rd, wr, EngineConfig, EngineEvent, Flags, Limits, Selection, Stats,
    TorrentHandle, TorrentState, BLOCK_SIZE,
};
use crate::bitfield::Bitfield;
use crate::metainfo::{InfoHash, Metainfo};
use crate::picker::{PiecePicker, PieceStatus};
use crate::storage::Storage;
use crate::tracker::{self, AnnounceEvent, AnnounceRequest, PeerAddr};

/// Период кондуктора: как часто проверяем, нужны ли новые пиры.
const DRIVE_TICK: Duration = Duration::from_millis(400);
/// Сколько новых соединений открываем за один такт кондуктора.
const CONNECT_BURST: usize = 8;

/// Окружение, общее для всех сессий одного торрента (без ссылки на менеджер,
/// чтобы не образовывать цикл `Arc`).
#[derive(Clone)]
pub(crate) struct Env {
    pub(crate) peer_id: [u8; 20],
    pub(crate) http: reqwest::Client,
    pub(crate) config: Arc<std::sync::RwLock<EngineConfig>>,
    pub(crate) port: u16,
    pub(crate) events: broadcast::Sender<EngineEvent>,
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) limits: Arc<std::sync::RwLock<Limits>>,
    /// Общие на сессию ведра лимитов скорости (одно на менеджер торрентов).
    pub(crate) shared: Arc<SharedLimiters>,
}

/// Запрос блока у пира.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) index: u32,
    pub(crate) begin: u32,
    pub(crate) length: u32,
}

/// Собираемый кусок: один владелец, набор уже полученных блоков.
pub(crate) struct PieceBuffer {
    pub(crate) owner: u64,
    pub(crate) data: Vec<u8>,
    pub(crate) received: Vec<bool>,
    pub(crate) total_blocks: u32,
    pub(crate) got: u32,
    pub(crate) inflight: HashSet<u32>,
}

impl PieceBuffer {
    fn new(owner: u64, size: usize) -> PieceBuffer {
        let total_blocks = size.div_ceil(BLOCK_SIZE as usize) as u32;
        PieceBuffer {
            owner,
            data: vec![0u8; size],
            received: vec![false; total_blocks as usize],
            total_blocks,
            got: 0,
            inflight: HashSet::new(),
        }
    }
}

/// Состояние подключённого пира.
pub(crate) struct PeerRecord {
    pub(crate) addr: SocketAddr,
    pub(crate) client: String,
    pub(crate) have: Bitfield,
    pub(crate) am_choking: bool,
    pub(crate) am_interested: bool,
    pub(crate) peer_choking: bool,
    pub(crate) owned_piece: Option<u32>,
    pub(crate) inflight: HashSet<u32>,
    pub(crate) down_bytes: u64,
    pub(crate) up_bytes: u64,
    pub(crate) last_down: u64,
    pub(crate) last_up: u64,
    pub(crate) down_rate: f64,
    pub(crate) up_rate: f64,
    pub(crate) sample_at: Instant,
}

impl PeerRecord {
    fn new(addr: SocketAddr, piece_count: usize) -> PeerRecord {
        PeerRecord {
            addr,
            client: "неизвестный".to_string(),
            have: Bitfield::new(piece_count),
            am_choking: false,
            am_interested: false,
            peer_choking: true,
            owned_piece: None,
            inflight: HashSet::new(),
            down_bytes: 0,
            up_bytes: 0,
            last_down: 0,
            last_up: 0,
            down_rate: 0.0,
            up_rate: 0.0,
            sample_at: Instant::now(),
        }
    }
}

/// Пауза перед повторной загрузкой куска, не прошедшего проверку SHA-1.
pub(crate) struct PieceCooldown {
    pub(crate) until: Instant,
    pub(crate) strikes: u32,
}

/// Изменяемое состояние торрента под одним мьютексом.
pub(crate) struct RuntimeState {
    pub(crate) picker: PiecePicker,
    pub(crate) buffers: HashMap<u32, PieceBuffer>,
    pub(crate) peers: HashMap<u64, PeerRecord>,
    pub(crate) pending: VecDeque<SocketAddr>,
    /// Адреса, которые трекер предложил и мы ещё не отбросили как мёртвые.
    pub(crate) seen: HashSet<SocketAddr>,
    /// Все адреса, когда-либо полученные от трекера. Не чистится при закрытии
    /// сессии: именно из него `requeue_peers` восстанавливает очередь после паузы.
    pub(crate) known: HashSet<SocketAddr>,
    pub(crate) connecting: usize,
    /// Куски, отложенные после неудачной проверки хэша: индекс -> срок паузы.
    pub(crate) cooldown: HashMap<u32, PieceCooldown>,
}

/// Простейший ограничитель скорости по блокам/байтам.
pub(crate) struct RateLimiter {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    fn new() -> RateLimiter {
        RateLimiter {
            tokens: 0.0,
            last: Instant::now(),
        }
    }

    fn refill(&mut self, per_second: f64, burst: f64) {
        let elapsed = self.last.elapsed().as_secs_f64();
        self.last = Instant::now();
        self.tokens = (self.tokens + per_second * elapsed).min(burst.max(1.0));
    }

    fn take(&mut self, amount: f64) -> bool {
        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }

    /// Сколько токенов доступно прямо сейчас; долив выполняется ровно один раз.
    fn available(&mut self, per_second: f64, burst: f64) -> f64 {
        self.refill(per_second, burst);
        self.tokens
    }
}

/// Ограничители скорости, общие на всю сессию (живут в `Inner` менеджера).
///
/// Ведро одно на сессию, поэтому при N торрентах глобальный лимит делится
/// между ними, а не умножается на N. Торрентный лимит проверяется сверх него.
pub(crate) struct SharedLimiters {
    pub(crate) down: Mutex<RateLimiter>,
    pub(crate) up: Mutex<RateLimiter>,
}

impl SharedLimiters {
    pub(crate) fn new() -> SharedLimiters {
        SharedLimiters {
            down: Mutex::new(RateLimiter::new()),
            up: Mutex::new(RateLimiter::new()),
        }
    }
}

/// Сколько блоков можно запросить с учётом общего и торрентного лимитов.
///
/// Сначала считается потолок торрента (действующая скорость = минимум из
/// глобальной и торрентной), затем из общего ведра сессии забирается не больше
/// доступного. Каждое ведро доливается один раз за вызов и списывается ровно на
/// выданное количество блоков, поэтому двойного списания нет.
fn grant_download(
    own: &Mutex<RateLimiter>,
    shared: &Mutex<RateLimiter>,
    global_kib: u32,
    local_kib: u32,
    want: usize,
) -> usize {
    if want == 0 || (global_kib == 0 && local_kib == 0) {
        return want;
    }
    let blocks_per_second =
        effective_limit(global_kib, local_kib) as f64 * 1024.0 / BLOCK_SIZE as f64;
    let mut own_limiter = lk(own);
    // 0 в торрентном лимите означает «без своего потолка»: тогда ограничивает
    // только общее ведро сессии.
    let allowed = if local_kib == 0 {
        want
    } else {
        own_limiter.available(blocks_per_second, 64.0).floor() as usize
    };
    let allowed = allowed.min(want);
    if allowed == 0 {
        return 0;
    }
    let granted = if global_kib == 0 {
        allowed
    } else {
        let global_blocks = global_kib as f64 * 1024.0 / BLOCK_SIZE as f64;
        let mut shared_limiter = lk(shared);
        let granted = (shared_limiter.available(global_blocks, 64.0).floor() as usize).min(allowed);
        if granted != 0 {
            shared_limiter.take(granted as f64);
        }
        granted
    };
    if granted == 0 {
        return 0;
    }
    if local_kib != 0 {
        own_limiter.take(granted as f64);
    }
    granted
}

/// Можно ли отдать `length` байт с учётом общего и торрентного лимитов отдачи.
fn grant_upload(
    own: &Mutex<RateLimiter>,
    shared: &Mutex<RateLimiter>,
    global_kib: u32,
    local_kib: u32,
    length: usize,
) -> bool {
    if length == 0 || (global_kib == 0 && local_kib == 0) {
        return true;
    }
    let length = length as f64;
    let effective = effective_limit(global_kib, local_kib) as f64 * 1024.0;
    let mut own_limiter = lk(own);
    if local_kib != 0 && own_limiter.available(effective, 512.0 * 1024.0) < length {
        return false;
    }
    if global_kib != 0 {
        let mut shared_limiter = lk(shared);
        let global_bytes = global_kib as f64 * 1024.0;
        if shared_limiter.available(global_bytes, 512.0 * 1024.0) < length {
            return false;
        }
        shared_limiter.take(length);
    }
    if local_kib != 0 {
        own_limiter.take(length);
    }
    true
}

/// Активный торрент: метаданные уже есть, идёт обмен с пирами.
pub(crate) struct Runtime {
    pub(crate) id: u64,
    pub(crate) meta: Metainfo,
    pub(crate) env: Env,
    pub(crate) storage: Mutex<Storage>,
    pub(crate) state: Mutex<RuntimeState>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) flags: Arc<Flags>,
    /// Текущий выбор файлов: какие куски качаем и сколько это байт.
    selection: std::sync::RwLock<Selection>,
    pub(crate) have_tx: broadcast::Sender<u32>,
    next_peer: AtomicU64,
    down_limiter: Mutex<RateLimiter>,
    up_limiter: Mutex<RateLimiter>,
    completed_announced: AtomicBool,
    /// Просит циклы трекеров объявиться раньше интервала, когда состояние
    /// изменилось не по расписанию (например, завершение из-за смены выбора
    /// файлов). Флаг, а не `Notify`, чтобы побудка не терялась, пока цикл
    /// занят запросом к трекеру.
    announce_pending: AtomicBool,
}

impl Runtime {
    pub(crate) fn new(
        id: u64,
        meta: Metainfo,
        storage: Storage,
        stats: Arc<Stats>,
        flags: Arc<Flags>,
        env: Env,
    ) -> Arc<Runtime> {
        let piece_count = meta.info.piece_count();
        let (have_tx, _) = broadcast::channel(512);
        Arc::new(Runtime {
            id,
            meta,
            env,
            storage: Mutex::new(storage),
            state: Mutex::new(RuntimeState {
                picker: PiecePicker::new(piece_count),
                buffers: HashMap::new(),
                peers: HashMap::new(),
                pending: VecDeque::new(),
                seen: HashSet::new(),
                known: HashSet::new(),
                connecting: 0,
                cooldown: HashMap::new(),
            }),
            stats,
            flags,
            selection: std::sync::RwLock::new(Selection::empty()),
            have_tx,
            next_peer: AtomicU64::new(1),
            down_limiter: Mutex::new(RateLimiter::new()),
            up_limiter: Mutex::new(RateLimiter::new()),
            completed_announced: AtomicBool::new(false),
            announce_pending: AtomicBool::new(false),
        })
    }

    /// Запускает фоновые задачи торрента (трекеры + кондуктор).
    pub(crate) fn start(self: &Arc<Self>) {
        let trackers = self.meta.trackers();
        *wr(&self.stats.trackers) = trackers.clone();
        for url in trackers.iter().take(6) {
            let runtime = self.clone();
            let url = url.clone();
            tokio::spawn(async move { runtime.tracker_loop(url).await });
        }
        let runtime = self.clone();
        tokio::spawn(async move { runtime.drive_loop().await });
    }

    pub(crate) fn stopping(&self) -> bool {
        self.flags.removed() || self.env.shutdown.load(Ordering::Relaxed)
    }

    pub(crate) fn have_bitfield(&self) -> Bitfield {
        lk(&self.state).picker.have().clone()
    }

    pub(crate) fn is_complete(&self) -> bool {
        lk(&self.state).picker.is_complete()
    }

    /// Закрывает файлы на запись, оставляя их читаемыми для других программ.
    ///
    /// Windows не даёт запустить `.exe` и открыть файл, пока на нём висит
    /// дескриптор с правом записи, поэтому завершённые, приостановленные и
    /// стоящие в очереди торренты держат файлы только для чтения.
    pub(crate) fn release_writes(&self) -> Result<(), String> {
        let mut storage = lk(&self.storage);
        storage
            .release_writes()
            .map_err(|err| format!("не удалось освободить файлы: {err}"))
    }

    /// Снова открывает файлы на запись: торрент продолжает загрузку.
    pub(crate) fn acquire_writes(&self) -> Result<(), String> {
        let mut storage = lk(&self.storage);
        storage
            .acquire_writes()
            .map_err(|err| format!("не удалось открыть файлы для записи: {err}"))
    }

    /// Перепроверяет все куски на диске по SHA-1 и восстанавливает битовую карту.
    ///
    /// Вызывается ручной перепроверкой (`TorrentManager::recheck`): на время
    /// работы обмен с пирами остановлен, прогресс растёт в `pieces_have`, а по
    /// завершении состояние приводится в соответствие с тем, что реально
    /// лежит на диске.
    pub(crate) async fn verify_all(self: &Arc<Self>) {
        let piece_count = self.meta.info.piece_count();
        // Обмен остановлен, поэтому очереди запросов и недоделанные буферы
        // можно выбросить целиком: после проверки они всё равно не нужны.
        {
            let mut state = lk(&self.state);
            state.buffers.clear();
            state.pending.clear();
            state.cooldown.clear();
            for peer in state.peers.values_mut() {
                peer.owned_piece = None;
                peer.inflight.clear();
            }
            state.picker.reset_have();
        }
        self.stats.pieces_have.store(0, Ordering::Relaxed);
        self.stats.have_bytes.store(0, Ordering::Relaxed);
        let mut good_bytes = 0u64;
        for index in 0..piece_count {
            if self.stopping() {
                // Движок останавливается или торрент удаляют — прерываемся.
                return;
            }
            let size = self.meta.info.piece_size(index) as usize;
            let offset = self.meta.info.piece_offset(index);
            let read = {
                let storage = lk(&self.storage);
                storage.read_at(offset, size)
            };
            let valid = match read {
                // Длина должна совпасть: недлинное чтение означает дыру в файлах.
                Ok(data) if data.len() == size => {
                    let expected = self.meta.info.pieces[index];
                    let mut hasher = Sha1::new();
                    hasher.update(&data);
                    hasher.finalize().as_slice() == expected.as_bytes().as_slice()
                }
                // Файла или куска на диске нет — данных нет.
                _ => false,
            };
            if valid {
                good_bytes += size as u64;
                lk(&self.state).picker.mark_have(index);
                self.stats.pieces_have.fetch_add(1, Ordering::Relaxed);
                let _ = self.have_tx.send(index as u32);
            }
            // Уступаем поток другим задачам: проверка большого торрента не
            // должна держать рантайм на несколько секунд.
            tokio::task::yield_now().await;
        }
        // Байт «на диске» — сумма проверенных кусков: это правда, а не сессия.
        self.stats.have_bytes.store(good_bytes, Ordering::Relaxed);
        let complete = self.is_complete();
        let seed = rd(&self.env.config).seed_after_complete;
        let mut next = match (complete, seed) {
            (true, true) => TorrentState::Seeding,
            (true, false) => TorrentState::Completed,
            (false, _) => TorrentState::Downloading,
        };
        // Пользовательская пауза сильнее результата проверки: торрент так и
        // остаётся на паузе, пока его не снимут с неё.
        if self.flags.user_paused() {
            next = TorrentState::Paused;
        }
        if next == TorrentState::Downloading {
            if let Err(message) = self.acquire_writes() {
                let _ = self.env.events.send(EngineEvent::Error {
                    id: self.id,
                    message,
                });
            }
        } else if let Err(message) = self.release_writes() {
            let _ = self.env.events.send(EngineEvent::Error {
                id: self.id,
                message,
            });
        }
        *lk(&self.stats.state) = next;
        let _ = self.env.events.send(EngineEvent::StateChanged {
            id: self.id,
            state: next,
        });
        if complete {
            // Трекеру мог понадобиться `event=completed` — сообщаем сразу.
            self.wake_announcer();
        }
    }

    /// Будит циклы объявлений, не теряя побудку.
    ///
    /// Одного `notify_waiters()` мало: он не оставляет permit, поэтому побудка
    /// пропадает, если цикл в этот момент не ждёт (например, ответ трекера ещё
    /// в пути) — и `event=completed` уходил бы только через интервал. Флаг
    /// переживает такую гонку.
    pub(crate) fn wake_announcer(&self) {
        self.announce_pending.store(true, Ordering::Relaxed);
    }

    pub(crate) fn max_peers(&self) -> usize {
        rd(&self.env.config).max_peers_per_torrent.clamp(1, 200)
    }

    // -- подключения --------------------------------------------------------

    /// Кладёт адреса пиров в очередь подключения.
    pub(crate) fn add_candidates(&self, peers: Vec<PeerAddr>) {
        let mut state = lk(&self.state);
        for peer in peers {
            if peer.port == 0 || peer.ip.is_unspecified() {
                continue;
            }
            let addr = SocketAddr::new(peer.ip, peer.port);
            if state.seen.len() > 4096 {
                state.seen.clear();
            }
            if state.known.len() > 8192 {
                state.known.clear();
            }
            state.known.insert(addr);
            if state.seen.insert(addr) {
                state.pending.push_back(addr);
            }
        }
        while state.pending.len() > 256 {
            state.pending.pop_back();
        }
    }

    pub(crate) fn take_candidate(&self) -> Option<SocketAddr> {
        let mut state = lk(&self.state);
        let addr = state.pending.pop_front()?;
        state.connecting += 1;
        Some(addr)
    }

    /// Снимает неудачное соединение со счётчика и возвращает адрес в оборот:
    /// трекер вправе предложить его снова, если пир ещё жив.
    pub(crate) fn candidate_failed(&self, addr: SocketAddr) {
        let mut state = lk(&self.state);
        state.connecting = state.connecting.saturating_sub(1);
        state.seen.remove(&addr);
    }

    /// Возвращает в очередь подключения известные адреса, с которыми сейчас
    /// нет соединения. Нужно после паузы: сессии закрылись, а новых
    /// объявлений трекера пришлось бы ждать десятки секунд.
    pub(crate) fn requeue_peers(&self) {
        let mut state = lk(&self.state);
        let active: HashSet<SocketAddr> = state.peers.values().map(|peer| peer.addr).collect();
        // Источник — `known`, а не `seen`: `unregister` вычищает `seen`, и после
        // паузы он пуст, хотя адреса пиров мы уже знаем.
        let idle: Vec<SocketAddr> = state
            .known
            .iter()
            .copied()
            .filter(|addr| !active.contains(addr))
            .collect();
        for addr in idle {
            if !state.pending.contains(&addr) {
                state.pending.push_front(addr);
            }
        }
    }

    pub(crate) fn register(&self, addr: SocketAddr) -> Option<u64> {
        let max = self.max_peers();
        let mut state = lk(&self.state);
        state.connecting = state.connecting.saturating_sub(1);
        if state.peers.len() >= max {
            return None;
        }
        let id = self.next_peer.fetch_add(1, Ordering::Relaxed);
        let peer = PeerRecord::new(addr, self.meta.info.piece_count());
        state.peers.insert(id, peer);
        Some(id)
    }

    pub(crate) fn unregister(&self, id: u64) {
        let mut state = lk(&self.state);
        if let Some(peer) = state.peers.remove(&id) {
            state.picker.remove_peer(&peer.have);
            // Адрес снова доступен для подключения: сессия закрылась штатно
            // (пауза, смена торрента), и трекер может предложить его повторно.
            state.seen.remove(&peer.addr);
            if let Some(index) = peer.owned_piece {
                state.buffers.remove(&index);
                state.picker.release(index as usize);
            }
        }
    }

    pub(crate) fn set_client(&self, id: u64, client: String) {
        if let Some(peer) = lk(&self.state).peers.get_mut(&id) {
            peer.client = client;
        }
    }

    // -- приём данных -------------------------------------------------------

    pub(crate) fn on_bitfield(&self, id: u64, have: Bitfield) {
        let mut state = lk(&self.state);
        let previous = match state.peers.get(&id) {
            Some(peer) => peer.have.clone(),
            None => return,
        };
        if previous.count() > 0 {
            state.picker.remove_peer(&previous);
        }
        state.picker.add_peer(&have);
        if let Some(peer) = state.peers.get_mut(&id) {
            peer.have = have;
        }
    }

    pub(crate) fn on_have(&self, id: u64, index: u32) {
        let mut state = lk(&self.state);
        if index as usize >= self.meta.info.piece_count() {
            return;
        }
        match state
            .peers
            .get(&id)
            .map(|peer| peer.have.get(index as usize))
        {
            Some(false) => {}
            _ => return,
        }
        if let Some(peer) = state.peers.get_mut(&id) {
            peer.have.set(index as usize);
        }
        state.picker.add_have(index as usize);
    }

    pub(crate) fn set_peer_choking(&self, id: u64, choking: bool) {
        let mut state = lk(&self.state);
        let owned = match state.peers.get(&id) {
            Some(peer) => peer.owned_piece,
            None => return,
        };
        if let Some(peer) = state.peers.get_mut(&id) {
            peer.peer_choking = choking;
            if choking {
                peer.owned_piece = None;
                peer.inflight.clear();
            }
        }
        if choking {
            if let Some(index) = owned {
                state.buffers.remove(&index);
                state.picker.release(index as usize);
            }
        }
    }

    pub(crate) fn set_peer_interested(&self, id: u64, interested: bool) {
        if let Some(peer) = lk(&self.state).peers.get_mut(&id) {
            peer.am_choking = !interested;
        }
    }

    /// Ставит или снимает флаг «мы заинтересованы в пире».
    pub(crate) fn set_am_interested(&self, id: u64, value: bool) {
        if let Some(peer) = lk(&self.state).peers.get_mut(&id) {
            peer.am_interested = value;
        }
    }

    /// Сколько блоков уже запрошено у пира.
    pub(crate) fn peer_inflight(&self, id: u64) -> usize {
        lk(&self.state)
            .peers
            .get(&id)
            .map(|peer| peer.inflight.len())
            .unwrap_or(0)
    }

    pub(crate) fn should_be_interested(&self, id: u64) -> bool {
        let state = lk(&self.state);
        let Some(peer) = state.peers.get(&id) else {
            return false;
        };
        if peer.am_interested || state.picker.is_complete() {
            return false;
        }
        let have = state.picker.have();
        (0..have.len()).any(|index| peer.have.get(index) && !have.get(index))
    }

    /// Сколько блоков можно запросить сейчас (с учётом лимита скорости).
    pub(crate) fn download_budget(&self, want: usize) -> usize {
        // Действует более строгий из двух лимитов: глобального и своего,
        // при этом глобальное ведро общее для всей сессии.
        let global = rd(&self.env.config).download_limit_kib;
        let local = rd(&self.env.limits).down_kib;
        grant_download(
            &self.down_limiter,
            &self.env.shared.down,
            global,
            local,
            want,
        )
    }

    /// Подбирает блоки, которые надо запросить у пира.
    pub(crate) fn next_requests(&self, id: u64, want: usize) -> Vec<Request> {
        if want == 0 {
            return Vec::new();
        }
        let mut state = lk(&self.state);
        let (peer_choking, am_interested, mut owned, peer_have) = match state.peers.get(&id) {
            Some(peer) => (
                peer.peer_choking,
                peer.am_interested,
                peer.owned_piece,
                peer.have.clone(),
            ),
            None => return Vec::new(),
        };
        if peer_choking || !am_interested {
            return Vec::new();
        }
        if owned.is_none() {
            // Куски, недавно не прошедшие проверку, временно не берём: иначе
            // на испорченном куске зацикливается всё, а остальные голодают.
            let now = Instant::now();
            state.cooldown.retain(|_, entry| entry.until > now);
            let mut exclude: HashSet<usize> =
                state.cooldown.keys().map(|index| *index as usize).collect();
            let mut picked = state.picker.pick(&peer_have, &exclude);
            if picked.is_none() && !exclude.is_empty() {
                // Все доступные куски на паузе — лучше попробовать снова, чем
                // стоять без дела (например, в торренте из одного куска).
                exclude.clear();
                picked = state.picker.pick(&peer_have, &exclude);
            }
            if let Some(index) = picked {
                let size = self.meta.info.piece_size(index) as usize;
                state
                    .buffers
                    .entry(index as u32)
                    .or_insert_with(|| PieceBuffer::new(id, size));
                owned = Some(index as u32);
                if let Some(peer) = state.peers.get_mut(&id) {
                    peer.owned_piece = Some(index as u32);
                }
            }
        }
        let Some(index) = owned else {
            return Vec::new();
        };
        let mut requests = Vec::with_capacity(want);
        if let Some(buffer) = state.buffers.get_mut(&index) {
            for block in 0..buffer.total_blocks {
                if requests.len() >= want {
                    break;
                }
                if buffer.received[block as usize] || buffer.inflight.contains(&block) {
                    continue;
                }
                let begin = block * BLOCK_SIZE;
                let length = BLOCK_SIZE.min(buffer.data.len() as u32 - begin);
                buffer.inflight.insert(block);
                requests.push(Request {
                    index,
                    begin,
                    length,
                });
            }
        }
        if let Some(peer) = state.peers.get_mut(&id) {
            for request in &requests {
                peer.inflight.insert(request.begin);
            }
        }
        requests
    }

    /// Принимает блок; если кусок собран — возвращает его данные.
    pub(crate) fn on_block(
        &self,
        id: u64,
        index: u32,
        begin: u32,
        data: Vec<u8>,
    ) -> Result<Option<(u32, Vec<u8>)>, String> {
        // Торрент на паузе или в очереди: файлы открыты только для чтения,
        // поэтому поздний блок просто выбрасываем (кусок докачаем позже).
        if self.flags.paused() {
            return Ok(None);
        }
        let mut state = lk(&self.state);
        if let Some(peer) = state.peers.get_mut(&id) {
            peer.down_bytes += data.len() as u64;
            peer.inflight.remove(&begin);
        }
        let Some(buffer) = state.buffers.get_mut(&index) else {
            return Ok(None);
        };
        if buffer.owner != id {
            return Ok(None);
        }
        if begin % BLOCK_SIZE != 0 {
            return Err("блок не выровнен по размеру".to_string());
        }
        let block = begin / BLOCK_SIZE;
        if block as usize >= buffer.received.len() {
            return Err("блок за пределами куска".to_string());
        }
        let offset = begin as usize;
        if offset + data.len() > buffer.data.len() {
            return Err("блок больше куска".to_string());
        }
        buffer.inflight.remove(&block);
        if buffer.received[block as usize] {
            return Ok(None);
        }
        buffer.data[offset..offset + data.len()].copy_from_slice(&data);
        buffer.received[block as usize] = true;
        buffer.got += 1;
        if buffer.got < buffer.total_blocks {
            return Ok(None);
        }
        let full = std::mem::take(&mut buffer.data);
        state.buffers.remove(&index);
        if let Some(peer) = state.peers.get_mut(&id) {
            if peer.owned_piece == Some(index) {
                peer.owned_piece = None;
            }
        }
        Ok(Some((index, full)))
    }

    /// Проверяет SHA-1 куска, пишет его на диск и обновляет состояние.
    pub(crate) fn complete_piece(
        self: &Arc<Self>,
        index: u32,
        data: &[u8],
    ) -> Result<bool, String> {
        let expected = match self.meta.info.pieces.get(index as usize) {
            Some(hash) => *hash,
            None => return Err("кусок за пределами торрента".to_string()),
        };
        let mut hasher = Sha1::new();
        hasher.update(data);
        let digest = hasher.finalize();
        let valid = digest.as_slice() == expected.as_bytes().as_slice();
        {
            let mut state = lk(&self.state);
            for peer in state.peers.values_mut() {
                if peer.owned_piece == Some(index) {
                    peer.owned_piece = None;
                    peer.inflight.clear();
                }
            }
            if valid {
                state.picker.mark_have(index as usize);
                state.cooldown.remove(&index);
            } else {
                state.picker.release(index as usize);
                // Кусок испорчен: откладываем его с растущей паузой, чтобы не
                // зациклиться на нём и успеть скачать остальные.
                let entry = state
                    .cooldown
                    .entry(index)
                    .or_insert_with(|| PieceCooldown {
                        until: Instant::now(),
                        strikes: 0,
                    });
                entry.strikes = entry.strikes.saturating_add(1);
                let pause = 1500u64.saturating_mul(entry.strikes as u64).min(10_000);
                entry.until = Instant::now() + Duration::from_millis(pause);
            }
        }
        if !valid {
            return Ok(false);
        }
        let offset = self.meta.info.piece_offset(index as usize);
        {
            let mut storage = lk(&self.storage);
            if storage.writes_released() {
                // Файлы успели закрыть на запись (пауза, завершение, перепроверка):
                // перед записью куска возвращаем дескрипторы в режим записи.
                storage
                    .acquire_writes()
                    .map_err(|err| format!("не удалось открыть файлы для записи: {err}"))?;
            }
            storage
                .write_at(offset, data)
                .map_err(|err| format!("не удалось записать кусок {index}: {err}"))?;
        }
        // Счётчик скачанного растёт только за проверенные куски: иначе при
        // повторных загрузках он уходит далеко за размер торрента.
        self.stats
            .downloaded
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.stats
            .have_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.stats.pieces_have.fetch_add(1, Ordering::Relaxed);
        let _ = self.have_tx.send(index);
        // Готовность считается только по нужным кускам: невыбранные файлы
        // могут остаться на диске недокачанными и торренту не мешают.
        if self.is_complete() {
            self.mark_complete();
        }
        Ok(true)
    }

    fn mark_complete(self: &Arc<Self>) {
        // Всё нужное скачано: файлы больше не пишутся, поэтому отпускаем их,
        // чтобы Windows разрешила запускать `.exe` и открывать их в других
        // программах. Раздача продолжает работать — чтение не блокируется.
        if let Err(message) = self.release_writes() {
            let _ = self.env.events.send(EngineEvent::Error {
                id: self.id,
                message,
            });
        }
        let seed = rd(&self.env.config).seed_after_complete;
        let state = if seed {
            TorrentState::Seeding
        } else {
            TorrentState::Completed
        };
        *lk(&self.stats.state) = state;
        let _ = self
            .env
            .events
            .send(EngineEvent::StateChanged { id: self.id, state });
        let name = rd(&self.stats.name).clone();
        let _ = self
            .env
            .events
            .send(EngineEvent::Completed { id: self.id, name });
    }

    // -- выбор файлов -------------------------------------------------------

    /// Применяет новый набор нужных кусков без уведомления о состоянии.
    pub(crate) fn set_wanted(&self, selection: &Selection) {
        *wr(&self.selection) = selection.clone();
        let mut state = lk(&self.state);
        let wanted = selection.pieces();
        state.picker.set_wanted(wanted);
        // Приоритеты файлов задают порядок загрузки кусков: высокий приоритет
        // качается первым, низкий — когда другого выбора нет.
        state.picker.set_levels(selection.levels());
        // Ненужные куски возвращаем в пул и выбрасываем недоделанные буферы;
        // уже полученные данные при этом остаются на диске.
        let mut dropped: Vec<u32> = Vec::new();
        for index in 0..wanted.len() {
            if wanted.get(index) {
                continue;
            }
            if state.picker.status(index) != PieceStatus::Have {
                state.picker.release(index);
                state.buffers.remove(&(index as u32));
                dropped.push(index as u32);
            }
        }
        if dropped.is_empty() {
            return;
        }
        for peer in state.peers.values_mut() {
            if let Some(index) = peer.owned_piece {
                if dropped.contains(&index) {
                    peer.owned_piece = None;
                    peer.inflight.clear();
                }
            }
        }
    }

    /// Применяет новый выбор и пересчитывает состояние (Downloading/Completed).
    pub(crate) fn apply_selection(self: &Arc<Self>, selection: &Selection) {
        self.set_wanted(selection);
        self.refresh_after_selection();
    }

    /// Сколько байт нужных файлов ещё не скачано (для объявления трекеру).
    pub(crate) fn wanted_left(&self) -> u64 {
        let wanted_bytes = self.stats.wanted_bytes.load(Ordering::Relaxed);
        let selection = rd(&self.selection);
        let state = lk(&self.state);
        let have_wanted =
            crate::selection::wanted_downloaded(&self.meta.info, state.picker.have(), &selection);
        drop(state);
        wanted_bytes.saturating_sub(have_wanted)
    }

    /// Пересчитывает состояние после смены выбора файлов.
    fn refresh_after_selection(self: &Arc<Self>) {
        if self.flags.paused() {
            return;
        }
        let complete = self.is_complete();
        let seed = rd(&self.env.config).seed_after_complete;
        let next = match (complete, seed) {
            (true, true) => TorrentState::Seeding,
            (true, false) => TorrentState::Completed,
            (false, _) => TorrentState::Downloading,
        };
        // Запись нужна только пока идёт загрузка. Завершённый или раздающий
        // торрент держит файлы только для чтения (см. `release_writes`).
        if next == TorrentState::Downloading {
            if let Err(message) = self.acquire_writes() {
                let _ = self.env.events.send(EngineEvent::Error {
                    id: self.id,
                    message,
                });
            }
        } else if let Err(message) = self.release_writes() {
            let _ = self.env.events.send(EngineEvent::Error {
                id: self.id,
                message,
            });
        }
        // Всё нужное уже на диске: будим цикл объявлений, чтобы трекер
        // получил актуальный `left` (и `event=completed` при первом выходе в
        // готовность) сразу, а не через интервал. Побудка нужна и тогда, когда
        // состояние не меняется: раздающий торрент после сужения выбора
        // объявляет новый остаток тем же состоянием `Seeding`.
        if complete {
            self.wake_announcer();
        }
        let current = *lk(&self.stats.state);
        if current == next {
            return;
        }
        *lk(&self.stats.state) = next;
        let _ = self.env.events.send(EngineEvent::StateChanged {
            id: self.id,
            state: next,
        });
    }

    // -- отдача -------------------------------------------------------------

    /// Читает запрошенный блок с диска для отправки пиру.
    pub(crate) fn upload(&self, index: u32, begin: u32, length: u32) -> Result<Vec<u8>, String> {
        if length == 0 || length > 256 * 1024 {
            return Err("некорректная длина запроса".to_string());
        }
        let piece_size = self.meta.info.piece_size(index as usize);
        if begin.saturating_add(length) > piece_size {
            return Err("запрос выходит за границы куска".to_string());
        }
        {
            let state = lk(&self.state);
            if state.picker.status(index as usize) != crate::picker::PieceStatus::Have {
                return Err("куска нет".to_string());
            }
        }
        if !self.allow_upload(length as usize) {
            return Err("превышен лимит отдачи".to_string());
        }
        let offset = self.meta.info.piece_offset(index as usize) + begin as u64;
        let storage = lk(&self.storage);
        storage
            .read_at(offset, length as usize)
            .map_err(|err| format!("не удалось прочитать кусок {index}: {err}"))
    }

    fn allow_upload(&self, length: usize) -> bool {
        let global = rd(&self.env.config).upload_limit_kib;
        let local = rd(&self.env.limits).up_kib;
        grant_upload(&self.up_limiter, &self.env.shared.up, global, local, length)
    }

    pub(crate) fn add_uploaded(&self, id: u64, length: usize) {
        self.stats
            .uploaded
            .fetch_add(length as u64, Ordering::Relaxed);
        if let Some(peer) = lk(&self.state).peers.get_mut(&id) {
            peer.up_bytes += length as u64;
        }
    }

    // -- статистика ---------------------------------------------------------

    /// Пересчитывает скорости пиров и собирает снимок списка пиров.
    pub(crate) fn sync_peer_view(&self) {
        let mut state = lk(&self.state);
        let now = Instant::now();
        let piece_count = self.meta.info.piece_count().max(1);
        let mut views = Vec::with_capacity(state.peers.len());
        for peer in state.peers.values_mut() {
            let elapsed = now.saturating_duration_since(peer.sample_at).as_secs_f64();
            let instant_down = (peer.down_bytes - peer.last_down) as f64 / elapsed.max(0.05);
            let instant_up = (peer.up_bytes - peer.last_up) as f64 / elapsed.max(0.05);
            peer.down_rate = peer.down_rate * 0.5 + instant_down * 0.5;
            peer.up_rate = peer.up_rate * 0.5 + instant_up * 0.5;
            peer.last_down = peer.down_bytes;
            peer.last_up = peer.up_bytes;
            peer.sample_at = now;
            let have = peer.have.count();
            views.push(super::PeerView {
                addr: peer.addr.to_string(),
                client: peer.client.clone(),
                down_rate: peer.down_rate,
                up_rate: peer.up_rate,
                progress: have as f64 / piece_count as f64,
                is_seed: have >= piece_count,
            });
        }
        views.sort_by(|left, right| {
            right
                .down_rate
                .partial_cmp(&left.down_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let connected = views.len() as u32;
        // «Известно» — сколько адресов мы получили от трекера и ещё можем
        // использовать, а не только те, что сейчас в очереди.
        let known = state.known.len().max(state.seen.len()) as u32;
        drop(state);
        *wr(&self.stats.peers) = views;
        self.stats
            .peers_connected
            .store(connected, Ordering::Relaxed);
        self.stats.peers_known.store(known, Ordering::Relaxed);
    }

    // -- ошибки обмена ------------------------------------------------------

    /// Переводит торрент в `Error`, если обмен ещё не начался. Если пиры уже
    /// подключены, объявление трекеру не критично — ошибку только запоминаем
    /// как текст последней неудачи и не мешаем загрузке.
    pub(crate) fn report_tracker_failure(&self, message: &str) {
        if self.stats.have_bytes.load(Ordering::Relaxed) > 0 {
            return;
        }
        let connected = lk(&self.state).peers.len();
        if connected > 0 {
            return;
        }
        if *lk(&self.stats.state) == TorrentState::Error {
            return;
        }
        let text = format!("трекер недоступен: {message}");
        *wr(&self.stats.error) = Some(text.clone());
        *lk(&self.stats.state) = TorrentState::Error;
        let _ = self.env.events.send(EngineEvent::StateChanged {
            id: self.id,
            state: TorrentState::Error,
        });
        let _ = self.env.events.send(EngineEvent::Error {
            id: self.id,
            message: text,
        });
    }

    /// Снимает ошибку трекера, когда объявление снова прошло.
    pub(crate) fn clear_tracker_failure(&self) {
        if rd(&self.stats.error).is_none() {
            return;
        }
        *wr(&self.stats.error) = None;
        if *lk(&self.stats.state) != TorrentState::Error {
            return;
        }
        let complete = self.is_complete();
        let seed = rd(&self.env.config).seed_after_complete;
        let next = match (complete, seed) {
            (true, true) => TorrentState::Seeding,
            (true, false) => TorrentState::Completed,
            (false, _) => TorrentState::Downloading,
        };
        *lk(&self.stats.state) = next;
        let _ = self.env.events.send(EngineEvent::StateChanged {
            id: self.id,
            state: next,
        });
    }

    // -- фоновые циклы ------------------------------------------------------

    async fn drive_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(DRIVE_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if self.stopping() {
                break;
            }
            if self.flags.paused() {
                continue;
            }
            let max = self.max_peers();
            let (active, connecting) = {
                let state = lk(&self.state);
                (state.peers.len(), state.connecting)
            };
            let free = max.saturating_sub(active + connecting).min(CONNECT_BURST);
            for _ in 0..free {
                let Some(addr) = self.take_candidate() else {
                    break;
                };
                let runtime = self.clone();
                tokio::spawn(async move { peer::connect(runtime, addr).await });
            }
        }
    }

    async fn tracker_loop(self: Arc<Self>, url: String) {
        let mut first = true;
        let mut failures: u32 = 0;
        loop {
            if self.stopping() {
                break;
            }
            if self.flags.paused() {
                tokio::time::sleep(Duration::from_millis(700)).await;
                continue;
            }
            // `left` и признак завершения — по нужным файлам: трекер ждёт
            // остаток именно того, что пользователь выбрал скачать.
            let left = self.wanted_left();
            let complete = self.is_complete();
            let event = if first {
                Some(AnnounceEvent::Started)
            } else if complete && !self.completed_announced.swap(true, Ordering::Relaxed) {
                Some(AnnounceEvent::Completed)
            } else {
                None
            };
            let request = AnnounceRequest {
                info_hash: self.meta.info_hash,
                peer_id: self.env.peer_id,
                port: self.env.port,
                uploaded: self.stats.uploaded.load(Ordering::Relaxed),
                downloaded: self.stats.downloaded.load(Ordering::Relaxed),
                left,
                event,
                num_want: Some(50),
                key: None,
            };
            match tracker::announce(&self.env.http, &url, &request, Duration::from_secs(25)).await {
                Ok(response) => {
                    first = false;
                    failures = 0;
                    self.clear_tracker_failure();
                    self.add_candidates(response.peers);
                    let interval = response.interval.clamp(30, 1800);
                    // Завершение из-за смены выбора файлов будит нас раньше,
                    // чтобы `event=completed` ушёл сразу, а не через интервал.
                    wait_wake(&self.announce_pending, Duration::from_secs(interval)).await;
                }
                Err(err) => {
                    failures = failures.saturating_add(1);
                    // Явный отказ трекера (`failure reason`) — сразу сообщаем
                    // пользователю; сетевые сбои требуют пары попыток подряд.
                    let refusal = matches!(err, tracker::TrackerError::Failure(_));
                    if refusal || failures >= 2 {
                        self.report_tracker_failure(&err.to_string());
                    }
                    let wait = (30u64 * failures as u64).min(300);
                    // Побудка (завершение, смена выбора) сокращает и эту паузу:
                    // `event=completed` не должен ждать конца серии ошибок.
                    wait_wake(&self.announce_pending, Duration::from_secs(wait)).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ожидание побудки
// ---------------------------------------------------------------------------

/// Ждёт побудки не дольше `limit`, не теряя её, если она пришла раньше.
///
/// `Notify::notify_waiters()` — не буфер: если адресат в этот момент не ждёт
/// (`notified()` ещё не создан), уведомление просто пропадает. Цикл объявлений
/// как раз часто занят: пока ответ трекера в пути, ожидание не создано, и
/// `event=completed` задерживался бы на весь интервал (30–1800 с). Поэтому
/// единственный канал побудки — флаг, который проверяется не реже, чем раз в
/// [`WAKE_POLL`]: уведомление не может потеряться, а цена — не более одного
/// короткого пробуждения таймера на этот интервал.
pub(crate) async fn wait_wake(pending: &AtomicBool, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        if pending.swap(false, Ordering::Relaxed) {
            return;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        tokio::time::sleep(left.min(WAKE_POLL)).await;
    }
}

/// Как часто перепроверяем флаг побудки, пока ждём интервал трекера.
const WAKE_POLL: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Установка торрента и получение метаданных magnet
// ---------------------------------------------------------------------------

/// Готовит хранилище, создаёт рантайм и запускает его задачи.
pub(crate) fn install(handle: &Arc<TorrentHandle>, meta: Metainfo) -> Result<(), String> {
    // Выбор файлов: заданный заранее (для magnet — после метаданных) или
    // полный по умолчанию.
    let pending_files = lk(&handle.pending_files).take();
    let pending_priorities = lk(&handle.pending_priorities).take();
    let selection = match (pending_files, pending_priorities) {
        // Явный список файлов важнее приоритетов, которые могли задать раньше.
        (Some(files), _) => Selection::from_files(&meta.info, Some(&files))
            .ok_or_else(|| "не выбран ни один файл".to_string())?,
        // Приоритеты задают и набор нужных файлов, и порядок их загрузки.
        (None, Some(priorities)) => Selection::from_priorities(&meta.info, &priorities)
            .ok_or_else(|| "неверное число приоритетов файлов".to_string())?,
        (None, None) => {
            let existing = rd(&handle.selection).clone();
            if existing.is_empty() {
                Selection::all(&meta.info)
            } else {
                existing
            }
        }
    };
    let storage = Storage::prepare(&handle.save_dir, &meta.info)
        .map_err(|err| format!("не удалось подготовить файлы: {err}"))?;
    handle.stats.set_meta(&meta);
    *wr(&handle.selection) = selection.clone();
    handle.stats.set_selection(&selection);
    let runtime = Runtime::new(
        handle.id,
        meta,
        storage,
        handle.stats.clone(),
        handle.flags.clone(),
        handle.env.clone(),
    );
    runtime.set_wanted(&selection);
    *wr(&handle.meta) = Some(runtime.meta.clone());
    *lk(&handle.runtime) = Some(runtime.clone());
    runtime.start();
    handle.set_state(TorrentState::Downloading);
    Ok(())
}

/// Запускает задачу получения метаданных magnet-ссылки.
pub(crate) fn start_metadata_fetch(handle: &Arc<TorrentHandle>) {
    let handle = handle.clone();
    tokio::spawn(async move { metadata_task(handle).await });
}

async fn metadata_task(handle: Arc<TorrentHandle>) {
    let info_hash: InfoHash = handle.info_hash;
    let trackers = handle
        .magnet
        .as_ref()
        .map(|magnet| magnet.trackers.clone())
        .unwrap_or_default();
    let display_name = handle
        .magnet
        .as_ref()
        .and_then(|magnet| magnet.display_name.clone());
    if trackers.is_empty() {
        handle.fail("в magnet-ссылке нет трекеров — метаданные взять негде");
        return;
    }
    let fetch = peer::MetadataFetch {
        info_hash,
        peer_id: handle.env.peer_id,
        timeout: Duration::from_secs(25),
    };
    let mut candidates: Vec<SocketAddr> = Vec::new();
    while !handle.flags.removed() && !handle.env.shutdown.load(Ordering::Relaxed) {
        if handle.flags.paused() {
            tokio::time::sleep(Duration::from_millis(700)).await;
            continue;
        }
        if handle.runtime_clone().is_some() {
            return;
        }
        if candidates.is_empty() {
            for url in trackers.iter().take(5) {
                if handle.flags.removed() {
                    return;
                }
                let request = AnnounceRequest {
                    info_hash,
                    peer_id: handle.env.peer_id,
                    port: handle.env.port,
                    uploaded: 0,
                    downloaded: 0,
                    left: 16 * 1024,
                    event: Some(AnnounceEvent::Started),
                    num_want: Some(50),
                    key: None,
                };
                if let Ok(response) =
                    tracker::announce(&handle.env.http, url, &request, Duration::from_secs(20))
                        .await
                {
                    for peer in response.peers {
                        let addr = SocketAddr::new(peer.ip, peer.port);
                        if peer.port != 0 && !candidates.contains(&addr) {
                            candidates.push(addr);
                        }
                    }
                }
            }
            candidates.truncate(64);
        }
        let mut received: Option<Vec<u8>> = None;
        while let Some(addr) = candidates.pop() {
            if handle.flags.removed() || handle.flags.paused() {
                break;
            }
            if let Ok(raw) = peer::fetch_metadata(&fetch, addr).await {
                received = Some(raw);
                break;
            }
        }
        if let Some(raw) = received {
            match Metainfo::from_raw_info(&raw, trackers.clone(), display_name.as_deref()) {
                Ok(meta) if meta.info_hash == info_hash => match install(&handle, meta) {
                    Ok(()) => return,
                    Err(message) => {
                        handle.fail(message);
                        return;
                    }
                },
                Ok(_) => {
                    handle.fail("метаданные не совпали с info-hash");
                    return;
                }
                Err(err) => {
                    handle.fail(format!("некорректные метаданные: {err}"));
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piece_buffer_splits_into_blocks() {
        let buffer = PieceBuffer::new(1, 16 * 1024 + 100);
        assert_eq!(buffer.total_blocks, 2);
        assert_eq!(buffer.data.len(), 16 * 1024 + 100);
        let buffer = PieceBuffer::new(1, 16 * 1024);
        assert_eq!(buffer.total_blocks, 1);
    }

    #[test]
    fn rate_limiter_accumulates_tokens() {
        let mut limiter = RateLimiter::new();
        limiter.refill(1000.0, 10.0);
        // Пустой бак: лимит начинает действовать строго, без подарков.
        assert!(!limiter.take(10.0));
        limiter.last = Instant::now() - Duration::from_secs(1);
        limiter.refill(1000.0, 10.0);
        // За секунду накоплен полный burst, но не больше него.
        assert!(limiter.take(10.0));
        assert!(!limiter.take(5.0));
        // Дробное накопление: 5 мс при 1000/с дают ровно 5 единиц.
        limiter.last = Instant::now() - Duration::from_millis(5);
        limiter.refill(1000.0, 10.0);
        assert!(limiter.take(5.0));
        assert!(!limiter.take(1.0));
    }

    /// Глобальное ведро одно на сессию: два торрента делят потолок, а не
    /// умножают его на своё количество.
    #[test]
    fn shared_limiter_caps_aggregate_download() {
        let shared = Mutex::new(RateLimiter::new());
        let first = Mutex::new(RateLimiter::new());
        let second = Mutex::new(RateLimiter::new());
        // Прошла секунда: при 64 КиБ/с в общем ведре четыре блока на всех.
        lk(&shared).last = Instant::now() - Duration::from_secs(1);
        let granted_first = grant_download(&first, &shared, 64, 0, 8);
        let granted_second = grant_download(&second, &shared, 64, 0, 8);
        assert_eq!(granted_first + granted_second, 4);
        // Ведро не доливается повторно в том же такте.
        assert_eq!(grant_download(&first, &shared, 64, 0, 8), 0);
    }

    /// Свой лимит торрента строже глобального: общее ведро его не разгоняет.
    #[test]
    fn torrent_limit_has_priority_over_global() {
        let shared = Mutex::new(RateLimiter::new());
        let own = Mutex::new(RateLimiter::new());
        // Глобально 1 МиБ/с (64 блока в секунду), торренту — 32 КиБ/с (2 блока).
        lk(&shared).last = Instant::now() - Duration::from_secs(10);
        lk(&own).last = Instant::now() - Duration::from_secs(10);
        let mut granted = 0;
        for _ in 0..10 {
            granted += grant_download(&own, &shared, 1024, 32, 64);
        }
        // За десять секунд торренту можно двадцать блоков, хотя общее ведро
        // готово отдать шестьдесят четыре.
        assert!((19..=20).contains(&granted), "granted = {granted}");
    }

    /// Отдача ограничивается тем же общим ведром сессии.
    #[test]
    fn shared_limiter_caps_aggregate_upload() {
        let shared = Mutex::new(RateLimiter::new());
        let first = Mutex::new(RateLimiter::new());
        let second = Mutex::new(RateLimiter::new());
        lk(&shared).last = Instant::now() - Duration::from_secs(1);
        // 64 КиБ/с — четыре блока по 16 КиБ в секунду на всю сессию.
        let block = BLOCK_SIZE as usize;
        let mut allowed = 0;
        for _ in 0..6 {
            if grant_upload(&first, &shared, 64, 0, block) {
                allowed += 1;
            }
            if grant_upload(&second, &shared, 64, 0, block) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 4);
    }

    /// Нулевые лимиты — без ограничения; торрентный работает и без глобального.
    #[test]
    fn zero_limits_mean_no_limit() {
        let shared = Mutex::new(RateLimiter::new());
        let own = Mutex::new(RateLimiter::new());
        assert_eq!(grant_download(&own, &shared, 0, 0, 16), 16);
        assert!(grant_upload(&own, &shared, 0, 0, 512 * 1024));
        let shared = Mutex::new(RateLimiter::new());
        let own = Mutex::new(RateLimiter::new());
        lk(&own).last = Instant::now() - Duration::from_secs(1);
        // Без глобального потолка каждый торрент упирается в свой: 4 блока/с.
        assert_eq!(grant_download(&own, &shared, 0, 64, 16), 4);
    }

    #[test]
    fn peer_record_starts_choked_and_unchoked_by_us() {
        let peer = PeerRecord::new("127.0.0.1:6881".parse().unwrap(), 10);
        assert!(peer.peer_choking);
        assert!(!peer.am_choking);
        assert!(!peer.am_interested);
        assert_eq!(peer.have.len(), 10);
    }

    /// Побудка не теряется, если пришла до того, как цикл начал ждать.
    ///
    /// Ровно эта гонка роняла `selection` на медленном CI: `notify_waiters()`
    /// в такой ситуации пропадает, и `event=completed` ждал бы полный интервал
    /// трекера (30 с), хотя всё нужное уже на диске.
    #[tokio::test]
    async fn wake_before_waiting_is_not_lost() {
        let pending = AtomicBool::new(true);
        let started = Instant::now();
        wait_wake(&pending, Duration::from_secs(60)).await;
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(2),
            "побудка потерялась: {waited:?}"
        );
        assert!(!pending.load(Ordering::Relaxed), "флаг побудки не снят");
    }

    /// Побудка из другой задачи будит уже ожидающий цикл, а не ждёт лимит.
    #[tokio::test]
    async fn wake_from_other_task_wakes_waiter() {
        let pending = Arc::new(AtomicBool::new(false));
        let flag = pending.clone();
        let started = Instant::now();
        let waiter = tokio::spawn(async move {
            wait_wake(&flag, Duration::from_secs(60)).await;
        });
        // Даём циклу встать в ожидание, затем ставим флаг.
        tokio::time::sleep(Duration::from_millis(300)).await;
        pending.store(true, Ordering::Relaxed);
        waiter.await.expect("цикл ожидания упал");
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(3),
            "флаг не разбудил цикл: {waited:?}"
        );
    }

    /// Без побудки ожидание честно длится весь лимит и не виснет дольше.
    #[tokio::test]
    async fn wait_returns_after_limit() {
        let pending = AtomicBool::new(false);
        let started = Instant::now();
        wait_wake(&pending, Duration::from_millis(120)).await;
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(100),
            "проснулись раньше: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(2),
            "переждали лимит: {waited:?}"
        );
    }
}
