//! Движок торрент-клиента: менеджер торрентов, статистика и события.
//!
//! Слой собран из трёх частей:
//!
//! * `engine::mod` — [`TorrentManager`], [`EngineConfig`], DTO снимков,
//!   [`Stats`] и общие вспомогательные функции;
//! * `engine::torrent` — [`Runtime`] одного торрента: циклы трекеров,
//!   кондуктор подключений, приём/отдача кусков, проверка SHA-1;
//! * `engine::peer` — одна TCP-сессия с пиром (handshake, wire-протокол,
//!   `ut_metadata` для magnet-ссылок).
//!
//! Все блокирующие операции (хэширование, диск) выполняются вне мьютексов,
//! снимки состояния читают только атомики и короткие `RwLock`.

pub(crate) mod peer;
pub(crate) mod torrent;

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::broadcast;

pub(crate) use torrent::{Env, SharedLimiters};

use crate::magnet::Magnet;
use crate::metainfo::{InfoHash, Metainfo};
use crate::peerid;
pub(crate) use crate::selection::{effective_limit, Selection};
use crate::tracker::{self, AnnounceEvent, AnnounceRequest};
use crate::wire::Handshake;

/// Сколько байт в блоке запроса (`request`/`piece`).
pub(crate) const BLOCK_SIZE: u32 = 16 * 1024;
/// Сколько одновременно «в полёте» блоков у одного пира.
pub(crate) const REQUEST_WINDOW: usize = 24;
/// Период тика статистики.
const STATS_TICK: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Вспомогательные функции для локов
// ---------------------------------------------------------------------------

fn rd<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn wr<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn lk<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

fn store_f64(atom: &AtomicU64, value: f64) {
    atom.store(value.to_bits(), Ordering::Relaxed);
}

fn load_f64(atom: &AtomicU64) -> f64 {
    f64::from_bits(atom.load(Ordering::Relaxed))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Читает ровно `buf.len()` байт с таймаутом.
pub(crate) async fn read_exact_timeout<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
    limit: Duration,
) -> std::io::Result<()> {
    match tokio::time::timeout(limit, reader.read_exact(buf)).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => Err(std::io::Error::new(ErrorKind::TimedOut, "таймаут чтения")),
    }
}

// ---------------------------------------------------------------------------
// Конфигурация
// ---------------------------------------------------------------------------

/// Настройки движка, которые меняются из интерфейса.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Каталог, куда по умолчанию сохраняются торренты.
    pub download_dir: PathBuf,
    /// Максимум подключённых пиров на торрент.
    pub max_peers_per_torrent: usize,
    /// Ограничение скорости загрузки, КиБ/с (0 — без ограничения).
    pub download_limit_kib: u32,
    /// Ограничение скорости отдачи, КиБ/с (0 — без ограничения).
    pub upload_limit_kib: u32,
    /// Порт для входящих подключений (0 — свободный).
    pub listen_port: u16,
    /// Продолжать раздачу после завершения.
    pub seed_after_complete: bool,
    /// Сколько торрентов могут одновременно скачиваться (0 — без ограничения).
    ///
    /// Торренты сверх лимита стоят в очереди (`TorrentState::Queued`) и не
    /// занимают подключения. Скачиванием считается и ожидание метаданных.
    pub max_active_downloads: usize,
    /// Сколько торрентов могут одновременно раздаваться (0 — без ограничения).
    pub max_active_seeds: usize,
}

/// Предел лимитов активных торрентов: больше всё равно не имеет смысла.
const MAX_ACTIVE_LIMIT: usize = 500;

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            download_dir: default_download_dir(),
            max_peers_per_torrent: 50,
            download_limit_kib: 0,
            upload_limit_kib: 0,
            listen_port: 0,
            seed_after_complete: true,
            max_active_downloads: 0,
            max_active_seeds: 0,
        }
    }
}

impl EngineConfig {
    /// Приводит значения к безопасным диапазонам.
    pub(crate) fn sanitized(mut self) -> EngineConfig {
        if self.download_dir.as_os_str().is_empty() {
            self.download_dir = default_download_dir();
        }
        self.max_peers_per_torrent = self.max_peers_per_torrent.clamp(1, 200);
        // 0 — «без ограничения», поэтому нижняя граница именно 0.
        self.max_active_downloads = self.max_active_downloads.min(MAX_ACTIVE_LIMIT);
        self.max_active_seeds = self.max_active_seeds.min(MAX_ACTIVE_LIMIT);
        self
    }
}

fn default_download_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(dir) => dir.join("Downloads"),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

// ---------------------------------------------------------------------------
// DTO для интерфейса
// ---------------------------------------------------------------------------

/// Состояние торрента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TorrentState {
    /// Ждёт слота в очереди: лимит активных торрентов исчерпан.
    Queued,
    /// Ждёт метаданные magnet-ссылки.
    Metadata,
    /// Сверяет хэши кусков на диске.
    Checking,
    Downloading,
    Seeding,
    Paused,
    Completed,
    Error,
}

/// Приоритет файла внутри торрента.
///
/// `Skip` выключает файл из загрузки, остальные уровни задают порядок, в котором
/// пикер отдаёт куски: сначала `High`, затем `Normal`, в конце `Low`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilePriority {
    Skip,
    Low,
    #[default]
    Normal,
    High,
}

/// Файл внутри торрента.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileView {
    pub path: String,
    pub length: u64,
    pub downloaded: u64,
    /// Выбран ли файл к загрузке; всегда согласован с `priority != Skip`.
    pub selected: bool,
    /// Приоритет загрузки файла.
    pub priority: FilePriority,
}

/// Подключённый пир в снимке состояния.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerView {
    pub addr: String,
    pub client: String,
    pub down_rate: f64,
    pub up_rate: f64,
    pub progress: f64,
    pub is_seed: bool,
}

/// Полный снимок торрента для UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentSnapshot {
    pub id: u64,
    pub name: String,
    pub info_hash: String,
    pub state: TorrentState,
    pub error: Option<String>,
    pub total_bytes: u64,
    /// Сколько байт занимают выбранные файлы.
    pub wanted_bytes: u64,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    pub progress: f64,
    pub piece_count: u32,
    pub piece_length: u32,
    pub pieces_have: u32,
    pub peers_connected: u32,
    pub peers_known: u32,
    pub down_rate: f64,
    pub up_rate: f64,
    /// Действующий лимит скачивания, КиБ/с (0 — без ограничения).
    pub down_limit_kib: u32,
    /// Действующий лимит отдачи, КиБ/с (0 — без ограничения).
    pub up_limit_kib: u32,
    pub eta_seconds: Option<u64>,
    pub save_path: String,
    pub added_at: i64,
    pub files: Vec<FileView>,
    pub trackers: Vec<String>,
    pub peers: Vec<PeerView>,
}

/// Суммарные показатели по всем торрентам.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalTotals {
    pub torrents: u32,
    pub active: u32,
    pub down_rate: f64,
    pub up_rate: f64,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
}

/// Источник добавления торрента.
#[derive(Debug, Clone)]
pub enum AddSource {
    /// Содержимое `.torrent`-файла.
    File(Vec<u8>),
    /// Magnet-ссылка.
    Magnet(String),
}

/// Дополнительные параметры добавления торрента.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Куда сохранять файлы (по умолчанию — `download_dir` из конфигурации).
    pub save_path: Option<PathBuf>,
    /// Индексы выбранных файлов; `None` — все файлы.
    pub files: Option<Vec<usize>>,
}

/// Файл в предварительном просмотре торрента.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewFile {
    pub index: usize,
    pub path: String,
    pub length: u64,
}

/// Метаданные торрента до его добавления в список.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentPreview {
    pub name: String,
    pub info_hash: String,
    pub total_bytes: u64,
    pub piece_length: u32,
    pub piece_count: u32,
    pub files: Vec<PreviewFile>,
    pub trackers: Vec<String>,
}

impl TorrentPreview {
    /// Собирает превью из разобранных метаданных.
    pub(crate) fn from_metainfo(meta: &Metainfo) -> TorrentPreview {
        TorrentPreview {
            name: meta.info.name.clone(),
            info_hash: meta.info_hash.to_hex(),
            total_bytes: meta.info.total_length,
            piece_length: meta.info.piece_length,
            piece_count: meta.info.piece_count() as u32,
            files: meta
                .info
                .files
                .iter()
                .enumerate()
                .map(|(index, file)| PreviewFile {
                    index,
                    path: file.path.clone(),
                    length: file.length,
                })
                .collect(),
            trackers: meta.trackers(),
        }
    }
}

/// Ошибки движка (сообщения уже на русском — их можно показывать в UI).
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("некорректный .torrent: {0}")]
    Metainfo(String),
    #[error("некорректная magnet-ссылка: {0}")]
    Magnet(String),
    #[error("ошибка ввода-вывода: {0}")]
    Io(String),
    #[error("торрент {0} не найден")]
    NotFound(u64),
    #[error("торрент уже добавлен: {0}")]
    Duplicate(String),
    #[error("не выбран ни один файл")]
    Selection,
    #[error("торрент ещё не готов к перепроверке")]
    NotReady,
}

/// События движка (транслируются подписчикам).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineEvent {
    Added { id: u64, name: String },
    StateChanged { id: u64, state: TorrentState },
    Completed { id: u64, name: String },
    Error { id: u64, message: String },
    Removed { id: u64 },
}

// ---------------------------------------------------------------------------
// Флаги рантайма
// ---------------------------------------------------------------------------

/// Разделяемые флаги торрента: пауза, проверка и удаление.
#[derive(Debug, Default)]
pub(crate) struct Flags {
    /// Работа остановлена: пауза пользователя или очередь планировщика.
    paused: AtomicBool,
    /// Пользователь нажал «пауза»; такая пауза важнее лимитов очереди.
    user_paused: AtomicBool,
    /// Идёт перепроверка кусков на диске.
    checking: AtomicBool,
    removed: AtomicBool,
}

impl Flags {
    pub(crate) fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub(crate) fn user_paused(&self) -> bool {
        self.user_paused.load(Ordering::Relaxed)
    }

    pub(crate) fn set_user_paused(&self, paused: bool) {
        self.user_paused.store(paused, Ordering::Relaxed);
    }

    pub(crate) fn checking(&self) -> bool {
        self.checking.load(Ordering::Relaxed)
    }

    pub(crate) fn set_checking(&self, checking: bool) {
        self.checking.store(checking, Ordering::Relaxed);
    }

    pub(crate) fn removed(&self) -> bool {
        self.removed.load(Ordering::Relaxed)
    }

    pub(crate) fn set_removed(&self) {
        self.removed.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Статистика торрента
// ---------------------------------------------------------------------------

/// Атомарная статистика одного торрента.
pub(crate) struct Stats {
    pub(crate) name: RwLock<String>,
    pub(crate) error: RwLock<Option<String>>,
    pub(crate) state: Mutex<TorrentState>,
    pub(crate) total_bytes: AtomicU64,
    /// Сколько байт занимают выбранные файлы.
    pub(crate) wanted_bytes: AtomicU64,
    pub(crate) have_bytes: AtomicU64,
    pub(crate) downloaded: AtomicU64,
    pub(crate) uploaded: AtomicU64,
    pub(crate) down_rate: AtomicU64,
    pub(crate) up_rate: AtomicU64,
    pub(crate) pieces_have: AtomicU32,
    pub(crate) piece_count: AtomicU32,
    pub(crate) piece_length: AtomicU32,
    pub(crate) peers_connected: AtomicU32,
    pub(crate) peers_known: AtomicU32,
    pub(crate) files: RwLock<Vec<FileView>>,
    pub(crate) trackers: RwLock<Vec<String>>,
    pub(crate) peers: RwLock<Vec<PeerView>>,
    last_down: AtomicU64,
    last_up: AtomicU64,
    last_sample: Mutex<Instant>,
}

impl Stats {
    pub(crate) fn new(name: String, state: TorrentState) -> Stats {
        Stats {
            name: RwLock::new(name),
            error: RwLock::new(None),
            state: Mutex::new(state),
            total_bytes: AtomicU64::new(0),
            wanted_bytes: AtomicU64::new(0),
            have_bytes: AtomicU64::new(0),
            downloaded: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            down_rate: AtomicU64::new(0f64.to_bits()),
            up_rate: AtomicU64::new(0f64.to_bits()),
            pieces_have: AtomicU32::new(0),
            piece_count: AtomicU32::new(0),
            piece_length: AtomicU32::new(0),
            peers_connected: AtomicU32::new(0),
            peers_known: AtomicU32::new(0),
            files: RwLock::new(Vec::new()),
            trackers: RwLock::new(Vec::new()),
            peers: RwLock::new(Vec::new()),
            last_down: AtomicU64::new(0),
            last_up: AtomicU64::new(0),
            last_sample: Mutex::new(Instant::now()),
        }
    }

    /// Заполняет метаданные после разбора `.torrent`.
    pub(crate) fn set_meta(&self, meta: &Metainfo) {
        *wr(&self.name) = meta.info.name.clone();
        self.total_bytes
            .store(meta.info.total_length, Ordering::Relaxed);
        self.wanted_bytes
            .store(meta.info.total_length, Ordering::Relaxed);
        self.piece_count
            .store(meta.info.piece_count() as u32, Ordering::Relaxed);
        self.piece_length
            .store(meta.info.piece_length, Ordering::Relaxed);
        *wr(&self.files) = meta
            .info
            .files
            .iter()
            .map(|file| FileView {
                path: file.path.clone(),
                length: file.length,
                downloaded: 0,
                selected: true,
                priority: FilePriority::Normal,
            })
            .collect();
    }

    /// Обновляет отметки выбранных файлов, приоритеты и объём нужных данных.
    pub(crate) fn set_selection(&self, selection: &Selection) {
        self.wanted_bytes
            .store(selection.bytes(), Ordering::Relaxed);
        let flags = selection.files();
        let priorities = selection.priorities();
        for (index, file) in wr(&self.files).iter_mut().enumerate() {
            if let Some(selected) = flags.get(index) {
                file.selected = *selected;
            }
            if let Some(priority) = priorities.get(index) {
                file.priority = *priority;
                // Приоритет — источник истины для выбора файла.
                file.selected = *priority != FilePriority::Skip;
            }
        }
    }

    /// Пересчитывает скорости (вызывается тикером раз в 500 мс).
    pub(crate) fn tick(&self) {
        let mut last = lk(&self.last_sample);
        let dt = last.elapsed().as_secs_f64().max(0.05);
        *last = Instant::now();
        let down = self.downloaded.load(Ordering::Relaxed);
        let up = self.uploaded.load(Ordering::Relaxed);
        let delta_down = down.saturating_sub(self.last_down.swap(down, Ordering::Relaxed));
        let delta_up = up.saturating_sub(self.last_up.swap(up, Ordering::Relaxed));
        smooth_rate(&self.down_rate, delta_down as f64 / dt);
        smooth_rate(&self.up_rate, delta_up as f64 / dt);
    }
}

/// Сглаживает мгновенную скорость, чтобы цифры в UI не «дрожали».
fn smooth_rate(atom: &AtomicU64, instant: f64) {
    let previous = load_f64(atom);
    let next = if instant > previous {
        previous * 0.4 + instant * 0.6
    } else {
        previous * 0.6 + instant * 0.4
    };
    store_f64(atom, if next < 1.0 { 0.0 } else { next });
}

// ---------------------------------------------------------------------------
// Торрент-хэндл
// ---------------------------------------------------------------------------

/// Локальные лимиты скорости одного торрента, КиБ/с (0 — без ограничения).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Limits {
    pub(crate) down_kib: u32,
    pub(crate) up_kib: u32,
}

/// Вид торрента для планировщика очереди.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanKind {
    /// Недокачанный торрент (включая ожидание метаданных magnet).
    Download,
    /// Всё нужное уже на диске, торрент раздаёт.
    Seed,
    /// В очередь не встаёт: пауза пользователя, проверка, ошибка.
    Idle,
}

/// Строка планировщика: торрент и его вид.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlanEntry {
    pub(crate) id: u64,
    pub(crate) kind: PlanKind,
    pub(crate) user_paused: bool,
}

/// Выбирает торренты, которые должны быть активны при заданных лимитах.
///
/// `entries` идут в порядке очереди (сначала добавленные). `limits` —
/// (сколько качать, сколько раздавать); 0 означает «без ограничения».
/// Пауза пользователя важнее лимитов и не занимает слот, поэтому такие
/// торренты в результат не попадают.
pub(crate) fn plan(entries: &[PlanEntry], limits: (usize, usize)) -> Vec<u64> {
    let (max_downloads, max_seeds) = limits;
    let mut downloads = 0usize;
    let mut seeds = 0usize;
    let mut active = Vec::new();
    for entry in entries {
        if entry.user_paused {
            continue;
        }
        match entry.kind {
            PlanKind::Idle => {}
            PlanKind::Download => {
                if max_downloads == 0 || downloads < max_downloads {
                    downloads += 1;
                    active.push(entry.id);
                }
            }
            PlanKind::Seed => {
                if max_seeds == 0 || seeds < max_seeds {
                    seeds += 1;
                    active.push(entry.id);
                }
            }
        }
    }
    active
}

/// Приводит набор активных торрентов в соответствие с лимитами очереди.
///
/// Каркас планировщика: лишние торренты паркуются (состояние `Queued`), а
/// освободившиеся слоты достаются тем, кого добавили раньше. Вызывается после
/// любого изменения, влияющего на очередь, и каждый тик статистики — поэтому
/// ограничения работают, даже если состояние изменилось само (например,
/// торрент докачался и хочет стать раздачей).
fn schedule(inner: &Arc<Inner>) {
    let limits = {
        let config = rd(&inner.config);
        (config.max_active_downloads, config.max_active_seeds)
    };
    let mut handles: Vec<Arc<TorrentHandle>> = rd(&inner.handles).clone();
    // Очередь — в порядке добавления: `added_at`, затем id.
    handles.sort_by_key(|handle| (handle.added_at, handle.id));
    let kinds: Vec<PlanKind> = handles.iter().map(|handle| handle.plan_kind()).collect();
    let entries: Vec<PlanEntry> = handles
        .iter()
        .zip(kinds.iter())
        .map(|(handle, kind)| PlanEntry {
            id: handle.id,
            kind: *kind,
            user_paused: handle.flags.user_paused(),
        })
        .collect();
    let active: std::collections::HashSet<u64> = plan(&entries, limits).into_iter().collect();
    for (handle, kind) in handles.iter().zip(kinds.iter()) {
        // `Idle` — проверка, ошибка, пауза пользователя или выключенная раздача:
        // слот такому торренту не нужен, и в очередь его ставить нельзя.
        if *kind == PlanKind::Idle {
            continue;
        }
        if active.contains(&handle.id) {
            handle.activate(*kind);
        } else {
            handle.park();
        }
    }
}

/// Применяет готовый выбор файлов к снимку и рантайму торрента.
///
/// Общий путь для [`TorrentManager::set_file_selection`] и
/// [`TorrentManager::set_file_priorities`]: оба меняют только набор нужных
/// кусков и их приоритеты.
fn apply_selection(handle: &Arc<TorrentHandle>, selection: Selection) {
    *wr(&handle.selection) = selection.clone();
    handle.stats.set_selection(&selection);
    if let Some(runtime) = handle.runtime_clone() {
        runtime.apply_selection(&selection);
    }
}

/// Синхронная часть торрента: живёт с момента добавления до удаления,
/// даже пока метаданные magnet-ссылки ещё не получены.
pub(crate) struct TorrentHandle {
    pub(crate) id: u64,
    pub(crate) info_hash: InfoHash,
    pub(crate) magnet: Option<Magnet>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) flags: Arc<Flags>,
    pub(crate) env: Env,
    pub(crate) added_at: i64,
    pub(crate) save_dir: PathBuf,
    pub(crate) meta: RwLock<Option<Metainfo>>,
    pub(crate) runtime: Mutex<Option<Arc<torrent::Runtime>>>,
    /// Текущий выбор файлов (нужные куски).
    pub(crate) selection: RwLock<Selection>,
    /// Локальные лимиты скорости.
    pub(crate) limits: Arc<RwLock<Limits>>,
    /// Выбор, заданный при добавлении magnet: применится после метаданных.
    pub(crate) pending_files: Mutex<Option<Vec<usize>>>,
    /// Приоритеты, заданные до получения метаданных (важнее `pending_files`).
    pub(crate) pending_priorities: Mutex<Option<Vec<FilePriority>>>,
}

impl TorrentHandle {
    pub(crate) fn runtime_clone(&self) -> Option<Arc<torrent::Runtime>> {
        lk(&self.runtime).clone()
    }

    pub(crate) fn effective_state(&self) -> TorrentState {
        // Проверка хэшей важнее паузы: пользователь должен видеть прогресс.
        if self.flags.checking() {
            return TorrentState::Checking;
        }
        let state = *lk(&self.stats.state);
        if !self.flags.paused() {
            return state;
        }
        // Очередь планировщика и пауза пользователя выглядят по-разному.
        if state == TorrentState::Queued {
            TorrentState::Queued
        } else {
            TorrentState::Paused
        }
    }

    /// Вид торрента для планировщика очереди.
    pub(crate) fn plan_kind(&self) -> PlanKind {
        if self.flags.removed() || self.flags.checking() || self.flags.user_paused() {
            return PlanKind::Idle;
        }
        if *lk(&self.stats.state) == TorrentState::Error {
            return PlanKind::Idle;
        }
        match self.runtime_clone() {
            Some(runtime) => {
                if !runtime.is_complete() {
                    PlanKind::Download
                } else if rd(&self.env.config).seed_after_complete {
                    PlanKind::Seed
                } else {
                    // Раздача выключена: слот не нужен.
                    PlanKind::Idle
                }
            }
            // Метаданные ещё не получены — это тоже активная загрузка.
            None if self.magnet.is_some() => PlanKind::Download,
            None => PlanKind::Idle,
        }
    }

    /// Возвращает торрент в работу: планировщик выдал ему слот.
    fn activate(self: &Arc<Self>, kind: PlanKind) {
        let target = match kind {
            PlanKind::Download if self.runtime_clone().is_some() => TorrentState::Downloading,
            PlanKind::Download => TorrentState::Metadata,
            PlanKind::Seed => TorrentState::Seeding,
            PlanKind::Idle => return,
        };
        let was_paused = self.flags.paused();
        self.flags.set_paused(false);
        if *lk(&self.stats.state) != target {
            self.set_state(target);
        }
        if !was_paused {
            return;
        }
        match self.runtime_clone() {
            Some(runtime) => {
                // Файлы могли быть закрыты на запись во время паузы или
                // проверки — перед обменом снова открываем их на запись.
                if kind == PlanKind::Download {
                    if let Err(message) = runtime.acquire_writes() {
                        self.fail(message);
                        return;
                    }
                }
                runtime.requeue_peers();
            }
            None => {
                if self.magnet.is_some() {
                    torrent::start_metadata_fetch(self);
                }
            }
        }
    }

    /// Ставит торрент в очередь: работа замирает, файлы закрываются на запись.
    fn park(&self) {
        if self.flags.paused() && *lk(&self.stats.state) == TorrentState::Queued {
            return;
        }
        self.flags.set_paused(true);
        self.release_writes();
        if *lk(&self.stats.state) != TorrentState::Queued {
            self.set_state(TorrentState::Queued);
        }
    }

    /// Закрывает файлы на запись, оставляя их доступными для чтения.
    ///
    /// Так Windows отпускает `.exe` и другие скачанные файлы: их можно запускать
    /// и открывать, пока торрент стоит на паузе или в очереди.
    pub(crate) fn release_writes(&self) {
        if let Some(runtime) = self.runtime_clone() {
            if let Err(message) = runtime.release_writes() {
                let _ = self.env.events.send(EngineEvent::Error {
                    id: self.id,
                    message,
                });
            }
        }
    }

    /// Пауза по требованию пользователя: в очередь за слотом такой торрент
    /// не встаёт, пока его не снимут с паузы.
    pub(crate) fn pause_for_user(&self) {
        self.flags.set_user_paused(true);
        self.flags.set_paused(true);
        self.release_writes();
        if *lk(&self.stats.state) != TorrentState::Paused {
            self.set_state(TorrentState::Paused);
        }
    }

    pub(crate) fn set_state(&self, state: TorrentState) {
        *lk(&self.stats.state) = state;
        let _ = self
            .env
            .events
            .send(EngineEvent::StateChanged { id: self.id, state });
    }

    /// Переводит торрент в состояние ошибки и уведомляет подписчиков.
    pub(crate) fn fail(&self, message: impl Into<String>) {
        let message = message.into();
        *wr(&self.stats.error) = Some(message.clone());
        *lk(&self.stats.state) = TorrentState::Error;
        let _ = self.env.events.send(EngineEvent::StateChanged {
            id: self.id,
            state: TorrentState::Error,
        });
        let _ = self.env.events.send(EngineEvent::Error {
            id: self.id,
            message,
        });
    }

    pub(crate) fn tick(&self) {
        self.stats.tick();
        if let Some(runtime) = self.runtime_clone() {
            runtime.sync_peer_view();
        }
    }

    /// Доля скачанных байт по каждому файлу (для колонки «прогресс файла»).
    fn file_bytes(&self) -> Vec<u64> {
        let meta = rd(&self.meta).clone();
        let Some(meta) = meta else {
            return Vec::new();
        };
        let runtime = self.runtime_clone();
        let have = match runtime {
            Some(runtime) => runtime.have_bitfield(),
            None => crate::bitfield::Bitfield::new(meta.info.piece_count()),
        };
        let files = &meta.info.files;
        let mut out = vec![0u64; files.len()];
        for index in 0..meta.info.piece_count() {
            if !have.get(index) {
                continue;
            }
            let start = meta.info.piece_offset(index);
            let end = start + meta.info.piece_size(index) as u64;
            let mut pos = files.partition_point(|file| file.offset + file.length <= start);
            while pos < files.len() && files[pos].offset < end {
                let from = start.max(files[pos].offset);
                let to = end.min(files[pos].offset + files[pos].length);
                out[pos] += to.saturating_sub(from);
                pos += 1;
            }
        }
        out
    }

    pub(crate) fn snapshot(&self) -> TorrentSnapshot {
        let state = self.effective_state();
        let total = self.stats.total_bytes.load(Ordering::Relaxed);
        let wanted_bytes = self.stats.wanted_bytes.load(Ordering::Relaxed);
        let mut files = rd(&self.stats.files).clone();
        let per_file = self.file_bytes();
        if per_file.len() == files.len() {
            for (file, bytes) in files.iter_mut().zip(per_file) {
                file.downloaded = bytes;
            }
        }
        // Прогресс и ETA считаются по выбранным файлам: скачанные байты чужих
        // файлов в зачёт не идут, хотя и остаются на диске.
        let wanted_have: u64 = files
            .iter()
            .filter(|file| file.selected)
            .map(|file| file.downloaded)
            .sum();
        let down_rate = load_f64(&self.stats.down_rate);
        let up_rate = load_f64(&self.stats.up_rate);
        let remaining = wanted_bytes.saturating_sub(wanted_have);
        let eta = if remaining > 0 && down_rate > 1.0 {
            Some((remaining as f64 / down_rate).ceil() as u64)
        } else {
            None
        };
        let limits = *rd(&self.limits);
        let config = rd(&self.env.config).clone();
        TorrentSnapshot {
            id: self.id,
            name: rd(&self.stats.name).clone(),
            info_hash: self.info_hash.to_hex(),
            state,
            error: rd(&self.stats.error).clone(),
            total_bytes: total,
            wanted_bytes,
            downloaded_bytes: self.stats.downloaded.load(Ordering::Relaxed),
            uploaded_bytes: self.stats.uploaded.load(Ordering::Relaxed),
            progress: if wanted_bytes == 0 {
                0.0
            } else {
                (wanted_have as f64 / wanted_bytes as f64).clamp(0.0, 1.0)
            },
            piece_count: self.stats.piece_count.load(Ordering::Relaxed),
            piece_length: self.stats.piece_length.load(Ordering::Relaxed),
            pieces_have: self.stats.pieces_have.load(Ordering::Relaxed),
            peers_connected: self.stats.peers_connected.load(Ordering::Relaxed),
            peers_known: self.stats.peers_known.load(Ordering::Relaxed),
            down_rate,
            up_rate,
            down_limit_kib: effective_limit(config.download_limit_kib, limits.down_kib),
            up_limit_kib: effective_limit(config.upload_limit_kib, limits.up_kib),
            eta_seconds: eta,
            save_path: self.save_dir.to_string_lossy().into_owned(),
            added_at: self.added_at,
            files,
            trackers: rd(&self.stats.trackers).clone(),
            peers: rd(&self.stats.peers).clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Менеджер торрентов
// ---------------------------------------------------------------------------

struct Inner {
    config: Arc<RwLock<EngineConfig>>,
    handles: RwLock<Vec<Arc<TorrentHandle>>>,
    next_id: AtomicU64,
    peer_id: [u8; 20],
    http: reqwest::Client,
    events: broadcast::Sender<EngineEvent>,
    listener: Mutex<Option<std::net::TcpListener>>,
    listen_port: u16,
    rt: RwLock<Option<tokio::runtime::Handle>>,
    background: AtomicBool,
    shutdown: Arc<AtomicBool>,
    /// Общие на сессию ведра лимитов скорости: глобальный потолок один на всех.
    limiters: Arc<SharedLimiters>,
}

/// Менеджер торрентов — единственная точка входа движка для Tauri-бэкенда.
pub struct TorrentManager {
    inner: Arc<Inner>,
}

impl TorrentManager {
    /// Создаёт менеджер: HTTP-клиент, слушающий TCP-сокет и тикер статистики.
    ///
    /// Может вызываться как внутри tokio-рантайма, так и из синхронного кода:
    /// фоновые задачи стартуют при первом обращении к async-методам.
    pub fn new(config: EngineConfig) -> Arc<TorrentManager> {
        let config = config.sanitized();
        let (listener, listen_port) = bind_listener(config.listen_port);
        let http = reqwest::Client::builder()
            .user_agent(peerid::client_name(&peerid::generate_peer_id()))
            .connect_timeout(Duration::from_secs(12))
            .timeout(Duration::from_secs(40))
            .build()
            .unwrap_or_default();
        let (events, _) = broadcast::channel(256);
        let manager = Arc::new(TorrentManager {
            inner: Arc::new(Inner {
                config: Arc::new(RwLock::new(config)),
                handles: RwLock::new(Vec::new()),
                next_id: AtomicU64::new(1),
                peer_id: peerid::generate_peer_id(),
                http,
                events,
                listener: Mutex::new(listener),
                listen_port,
                rt: RwLock::new(None),
                background: AtomicBool::new(false),
                shutdown: Arc::new(AtomicBool::new(false)),
                limiters: Arc::new(SharedLimiters::new()),
            }),
        });
        manager.ensure_background();
        manager
    }

    /// Текущая конфигурация.
    pub fn config(&self) -> EngineConfig {
        rd(&self.inner.config).clone()
    }

    /// Пересобирает очередь активных торрентов по текущим лимитам.
    fn schedule(&self) {
        schedule(&self.inner);
    }

    /// Применяет новую конфигурацию (каталог, лимиты, порт для новых сессий).
    pub fn set_config(&self, config: EngineConfig) {
        *wr(&self.inner.config) = config.sanitized();
        // Лимиты активных торрентов могли поменяться — сразу пересобираем очередь.
        self.schedule();
    }

    /// Порт, на котором движок слушает входящие подключения (0 — не слушает).
    pub fn listen_port(&self) -> u16 {
        self.inner.listen_port
    }

    /// Добавляет торрент из содержимого `.torrent`.
    pub async fn add_file(
        &self,
        data: Vec<u8>,
        save_path: Option<PathBuf>,
    ) -> Result<u64, EngineError> {
        self.add_file_with(
            data,
            AddOptions {
                save_path,
                files: None,
            },
        )
        .await
    }

    /// Добавляет торрент из содержимого `.torrent` с выбором файлов.
    pub async fn add_file_with(&self, data: Vec<u8>, opts: AddOptions) -> Result<u64, EngineError> {
        self.ensure_background();
        let meta = Metainfo::parse(&data).map_err(|err| EngineError::Metainfo(err.to_string()))?;
        // Выбор проверяем до добавления: пустой или кривой список индексов
        // не должен оставлять «полуторрент» в списке.
        let selection = Selection::from_files(&meta.info, opts.files.as_deref())
            .ok_or(EngineError::Selection)?;
        let hash = meta.info_hash;
        if self.find_by_hash(hash).is_some() {
            return Err(EngineError::Duplicate(hash.to_hex()));
        }
        let dir = opts.save_path.unwrap_or_else(|| self.config().download_dir);
        let name = meta.info.name.clone();
        let handle = self.make_handle(hash, None, name.clone(), dir, TorrentState::Queued);
        *wr(&handle.selection) = selection;
        self.push_handle(handle.clone())?;
        if let Err(message) = torrent::install(&handle, meta) {
            self.drop_handle(handle.id);
            return Err(EngineError::Io(message));
        }
        let _ = self.inner.events.send(EngineEvent::Added {
            id: handle.id,
            name,
        });
        // Лимит активных торрентов мог быть уже исчерпан — новый встанет в очередь.
        self.schedule();
        Ok(handle.id)
    }

    /// Добавляет торрент по magnet-ссылке; метаданные подтягиваются у пиров.
    pub async fn add_magnet(
        &self,
        link: &str,
        save_path: Option<PathBuf>,
    ) -> Result<u64, EngineError> {
        self.add_magnet_with(
            link,
            AddOptions {
                save_path,
                files: None,
            },
        )
        .await
    }

    /// Добавляет торрент по magnet-ссылке с выбором файлов.
    ///
    /// Список файлов применится, когда придут метаданные; пустой список —
    /// сразу ошибка [`EngineError::Selection`].
    pub async fn add_magnet_with(&self, link: &str, opts: AddOptions) -> Result<u64, EngineError> {
        self.ensure_background();
        if matches!(&opts.files, Some(files) if files.is_empty()) {
            return Err(EngineError::Selection);
        }
        let magnet =
            crate::magnet::parse(link).map_err(|err| EngineError::Magnet(err.to_string()))?;
        let hash = magnet.info_hash;
        if self.find_by_hash(hash).is_some() {
            return Err(EngineError::Duplicate(hash.to_hex()));
        }
        let dir = opts.save_path.unwrap_or_else(|| self.config().download_dir);
        let name = magnet.display_name.clone().unwrap_or_else(|| hash.to_hex());
        let handle = self.make_handle(
            hash,
            Some(magnet),
            name.clone(),
            dir,
            TorrentState::Metadata,
        );
        *lk(&handle.pending_files) = opts.files;
        self.push_handle(handle.clone())?;
        let _ = self.inner.events.send(EngineEvent::Added {
            id: handle.id,
            name,
        });
        // Очередь решает, искать ли метаданные прямо сейчас.
        self.schedule();
        if !handle.flags.paused() {
            torrent::start_metadata_fetch(&handle);
        }
        Ok(handle.id)
    }

    /// Меняет набор выбранных файлов уже добавленного торрента.
    ///
    /// Пересчитываются нужные куски, невыбранные возвращаются в пул, статус и
    /// событие `StateChanged` обновляются. Скачанные файлы остаются на диске.
    pub fn set_file_selection(&self, id: u64, files: Vec<usize>) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        if files.is_empty() {
            return Err(EngineError::Selection);
        }
        let meta = rd(&handle.meta).clone();
        match meta {
            Some(meta) => {
                let selection = Selection::from_files(&meta.info, Some(&files))
                    .ok_or(EngineError::Selection)?;
                apply_selection(&handle, selection);
            }
            None => {
                // Метаданные ещё не получены: запоминаем выбор до установки.
                *lk(&handle.pending_files) = Some(files);
                *lk(&handle.pending_priorities) = None;
            }
        }
        // Новый набор файлов может завершить торрент (или снова его запустить).
        self.schedule();
        Ok(())
    }

    /// Задаёт приоритеты загрузки для всех файлов торрента.
    ///
    /// Длина списка должна совпадать с числом файлов. Приоритет `Skip` исключает
    /// файл из загрузки так же, как снятая галочка в [`Self::set_file_selection`];
    /// высокий приоритет заставляет качать куски этого файла первыми.
    ///
    /// Метаданные magnet-ссылки могли ещё не прийти: тогда приоритеты
    /// сохраняются и применяются сразу после их получения.
    pub fn set_file_priorities(
        &self,
        id: u64,
        priorities: Vec<FilePriority>,
    ) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        let meta = rd(&handle.meta).clone();
        match meta {
            Some(meta) => {
                let selection = Selection::from_priorities(&meta.info, &priorities)
                    .ok_or(EngineError::Selection)?;
                apply_selection(&handle, selection);
            }
            None => {
                // Приоритеты применятся при установке метаданных и важнее
                // списка файлов, заданного при добавлении.
                *lk(&handle.pending_files) = None;
                *lk(&handle.pending_priorities) = Some(priorities);
            }
        }
        self.schedule();
        Ok(())
    }

    /// Задаёт локальные лимиты скорости торрента, КиБ/с (0 — без ограничения).
    ///
    /// Действующий лимит — минимум из глобального и локального.
    pub fn set_limits(&self, id: u64, down_kib: u32, up_kib: u32) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        *wr(&handle.limits) = Limits { down_kib, up_kib };
        Ok(())
    }

    /// Разбирает `.torrent` и отдаёт метаданные для окна добавления.
    ///
    /// Ничего не добавляет в список торрентов.
    pub fn preview_file(data: &[u8]) -> Result<TorrentPreview, EngineError> {
        let meta = Metainfo::parse(data).map_err(|err| EngineError::Metainfo(err.to_string()))?;
        Ok(TorrentPreview::from_metainfo(&meta))
    }

    /// Достаёт метаданные magnet-ссылки у пиров, не добавляя торрент в список.
    ///
    /// Укладывается в `timeout`; по завершении не остаётся ни задач, ни
    /// подписок — все подключения живут внутри вызова.
    pub async fn preview_magnet(
        &self,
        link: &str,
        timeout: Duration,
    ) -> Result<TorrentPreview, EngineError> {
        let magnet =
            crate::magnet::parse(link).map_err(|err| EngineError::Magnet(err.to_string()))?;
        if magnet.trackers.is_empty() {
            return Err(EngineError::Magnet(
                "в magnet-ссылке нет трекеров — метаданные взять негде".to_string(),
            ));
        }
        let deadline = Instant::now() + timeout;
        let fetch = peer::MetadataFetch {
            info_hash: magnet.info_hash,
            peer_id: self.inner.peer_id,
            timeout,
        };
        let mut candidates: Vec<SocketAddr> = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if candidates.is_empty() {
                for url in magnet.trackers.iter().take(5) {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    let request = AnnounceRequest {
                        info_hash: magnet.info_hash,
                        peer_id: self.inner.peer_id,
                        port: self.inner.listen_port,
                        uploaded: 0,
                        downloaded: 0,
                        left: 16 * 1024,
                        event: Some(AnnounceEvent::Started),
                        num_want: Some(50),
                        key: None,
                    };
                    let response = tokio::time::timeout(
                        left,
                        tracker::announce(&self.inner.http, url, &request, left),
                    )
                    .await;
                    if let Ok(Ok(response)) = response {
                        for peer in response.peers {
                            let addr = SocketAddr::new(peer.ip, peer.port);
                            if peer.port != 0
                                && !peer.ip.is_unspecified()
                                && !candidates.contains(&addr)
                            {
                                candidates.push(addr);
                            }
                        }
                    }
                }
                candidates.truncate(64);
            }
            let mut received: Option<Vec<u8>> = None;
            while let Some(addr) = candidates.pop() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let fetched = tokio::time::timeout(left, peer::fetch_metadata(&fetch, addr)).await;
                if let Ok(Ok(raw)) = fetched {
                    received = Some(raw);
                    break;
                }
            }
            if let Some(raw) = received {
                let meta = Metainfo::from_raw_info(
                    &raw,
                    magnet.trackers.clone(),
                    magnet.display_name.as_deref(),
                )
                .map_err(|err| EngineError::Metainfo(err.to_string()))?;
                if meta.info_hash != magnet.info_hash {
                    return Err(EngineError::Metainfo(
                        "метаданные не совпали с info-hash".to_string(),
                    ));
                }
                return Ok(TorrentPreview::from_metainfo(&meta));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            tokio::time::sleep(left.min(Duration::from_secs(3))).await;
        }
        Err(EngineError::Io(
            "не удалось получить метаданные за отведённое время".to_string(),
        ))
    }

    /// Ставит торрент на паузу (соединения закрываются, данные сохраняются).
    pub fn pause(&self, id: u64) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        handle.pause_for_user();
        // Слот освободился — его получит следующий в очереди.
        self.schedule();
        Ok(())
    }

    /// Снимает торрент с паузы.
    pub fn resume(&self, id: u64) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        handle.flags.set_user_paused(false);
        // Дальше решает планировщик: активных торрентов может быть уже достаточно,
        // и тогда торрент подождёт в очереди.
        self.schedule();
        Ok(())
    }

    /// Пауза для всех торрентов.
    pub fn pause_all(&self) {
        self.ensure_background();
        for handle in self.handles() {
            handle.pause_for_user();
        }
        self.schedule();
    }

    /// Снимает с паузы все торренты.
    pub fn resume_all(&self) {
        self.ensure_background();
        for handle in self.handles() {
            handle.flags.set_user_paused(false);
        }
        // Очередь распределит слоты по тем же правилам, что и при старте.
        self.schedule();
    }

    /// Перепроверяет скачанные данные торрента по SHA-1.
    ///
    /// На время проверки торрент переходит в [`TorrentState::Checking`], обмен с
    /// пирами останавливается, прогресс виден в `pieces_have`. По завершении
    /// состояние выбирается по фактически найденным кускам: недостающие нужные
    /// данные докачиваются, полностью готовый торрент раздаётся или попадает в
    /// завершённые. Пользовательская пауза при этом не снимается.
    pub fn recheck(&self, id: u64) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        let Some(runtime) = handle.runtime_clone() else {
            // Метаданные magnet ещё не получены — проверять нечего.
            return Err(EngineError::NotReady);
        };
        if handle.flags.checking() {
            // Проверка уже идёт: повторный вызов ничего не ломает.
            return Ok(());
        }
        handle.flags.set_checking(true);
        // Обмен останавливаем: проверяем то, что уже лежит на диске. Файлы при
        // этом остаются открытыми только для чтения.
        handle.flags.set_paused(true);
        handle.release_writes();
        handle.stats.pieces_have.store(0, Ordering::Relaxed);
        handle.set_state(TorrentState::Checking);
        let inner = self.inner.clone();
        tokio::spawn(async move {
            runtime.verify_all().await;
            handle.flags.set_checking(false);
            if handle.flags.removed() || inner.shutdown.load(Ordering::Relaxed) {
                // Торрент удалили или движок останавливается — очередь не нужна.
                return;
            }
            // Проверка выставила состояние и режим файлов; очередь решает,
            // получит ли торрент слот или подождёт в `Queued`.
            schedule(&inner);
        });
        Ok(())
    }

    /// Удаляет торрент; при `delete_files` чистит скачанные файлы в каталоге.
    pub async fn remove(&self, id: u64, delete_files: bool) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        handle.flags.set_removed();
        handle.flags.set_paused(true);
        if let Some(runtime) = handle.runtime_clone() {
            let storage = lk(&runtime.storage);
            let _ = storage.flush();
            if delete_files {
                for path in storage.paths() {
                    let _ = std::fs::remove_file(&path);
                }
            }
        } else if delete_files {
            if let Some(meta) = rd(&handle.meta).clone() {
                for file in &meta.info.files {
                    let _ = std::fs::remove_file(handle.save_dir.join(&file.path));
                }
            }
        }
        self.drop_handle(id);
        let _ = self.inner.events.send(EngineEvent::Removed { id });
        // Освободившийся слот достаётся следующему в очереди.
        self.schedule();
        Ok(())
    }

    /// Снимок всех торрентов (сортировка по времени добавления).
    pub fn snapshot(&self) -> Vec<TorrentSnapshot> {
        self.ensure_background();
        let mut handles = self.handles();
        handles.sort_by_key(|handle| (handle.added_at, handle.id));
        handles.iter().map(|handle| handle.snapshot()).collect()
    }

    /// Суммарные показатели.
    pub fn totals(&self) -> GlobalTotals {
        self.ensure_background();
        let mut totals = GlobalTotals::default();
        for handle in self.handles() {
            totals.torrents += 1;
            let state = handle.effective_state();
            if matches!(
                state,
                TorrentState::Downloading | TorrentState::Seeding | TorrentState::Metadata
            ) {
                totals.active += 1;
            }
            totals.down_rate += load_f64(&handle.stats.down_rate);
            totals.up_rate += load_f64(&handle.stats.up_rate);
            totals.downloaded_bytes += handle.stats.downloaded.load(Ordering::Relaxed);
            totals.uploaded_bytes += handle.stats.uploaded.load(Ordering::Relaxed);
        }
        totals
    }

    /// Останавливает все задачи и сбрасывает буферы на диск.
    pub async fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        for handle in self.handles() {
            handle.flags.set_paused(true);
            if let Some(runtime) = handle.runtime_clone() {
                let storage = lk(&runtime.storage);
                let _ = storage.flush();
            }
        }
        // Ждём, пока циклы заметят флаг: приём входящих опрашивает его раз в
        // 100 мс, после выхода сокет закрывается и порт снова свободен.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    /// Подписка на события движка.
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    // -- внутреннее ---------------------------------------------------------

    fn handles(&self) -> Vec<Arc<TorrentHandle>> {
        rd(&self.inner.handles).clone()
    }

    fn find(&self, id: u64) -> Option<Arc<TorrentHandle>> {
        self.handles().into_iter().find(|handle| handle.id == id)
    }

    fn find_by_hash(&self, hash: InfoHash) -> Option<Arc<TorrentHandle>> {
        self.handles()
            .into_iter()
            .find(|handle| handle.info_hash == hash)
    }

    fn make_handle(
        &self,
        hash: InfoHash,
        magnet: Option<Magnet>,
        name: String,
        save_dir: PathBuf,
        state: TorrentState,
    ) -> Arc<TorrentHandle> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let stats = Arc::new(Stats::new(name, state));
        if let Some(magnet) = &magnet {
            *wr(&stats.trackers) = magnet.trackers.clone();
        }
        let limits = Arc::new(RwLock::new(Limits::default()));
        Arc::new(TorrentHandle {
            id,
            info_hash: hash,
            magnet,
            stats,
            flags: Arc::new(Flags::default()),
            env: Env {
                peer_id: self.inner.peer_id,
                http: self.inner.http.clone(),
                config: self.inner.config.clone(),
                port: self.inner.listen_port,
                events: self.inner.events.clone(),
                shutdown: self.inner.shutdown.clone(),
                limits: limits.clone(),
                shared: self.inner.limiters.clone(),
            },
            added_at: now_unix(),
            save_dir,
            meta: RwLock::new(None),
            runtime: Mutex::new(None),
            selection: RwLock::new(Selection::empty()),
            limits,
            pending_files: Mutex::new(None),
            pending_priorities: Mutex::new(None),
        })
    }

    fn push_handle(&self, handle: Arc<TorrentHandle>) -> Result<(), EngineError> {
        wr(&self.inner.handles).push(handle);
        Ok(())
    }

    fn drop_handle(&self, id: u64) {
        wr(&self.inner.handles).retain(|handle| handle.id != id);
    }

    fn current_handle(&self) -> Option<tokio::runtime::Handle> {
        if let Some(handle) = rd(&self.inner.rt).clone() {
            return Some(handle);
        }
        let handle = tokio::runtime::Handle::try_current().ok()?;
        *wr(&self.inner.rt) = Some(handle.clone());
        Some(handle)
    }

    /// Лениво поднимает тикер статистики и цикл приёма входящих подключений.
    fn ensure_background(&self) {
        if self.inner.background.load(Ordering::SeqCst) {
            return;
        }
        let Some(runtime) = self.current_handle() else {
            return;
        };
        if self.inner.background.swap(true, Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.clone();
        runtime.spawn(async move { ticker_loop(inner).await });
        let inner = self.inner.clone();
        runtime.spawn(async move { accept_loop(inner).await });
    }
}

/// Пытается занять порт; при неудаче берёт любой свободный.
fn bind_listener(port: u16) -> (Option<std::net::TcpListener>, u16) {
    let attempt = |port: u16| std::net::TcpListener::bind(("0.0.0.0", port)).ok();
    let mut listener = attempt(port);
    if listener.is_none() && port != 0 {
        listener = attempt(0);
    }
    match listener {
        Some(listener) => {
            let actual = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
            let _ = listener.set_nonblocking(true);
            (Some(listener), actual)
        }
        None => (None, 0),
    }
}

async fn ticker_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(STATS_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if inner.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let handles: Vec<Arc<TorrentHandle>> = rd(&inner.handles).clone();
        for handle in handles {
            handle.tick();
        }
        // Состояние могло измениться само (торрент докачался, проверка
        // завершилась) — очередь пересобирается на каждом тике.
        schedule(&inner);
    }
}

async fn accept_loop(inner: Arc<Inner>) {
    let listener = lk(&inner.listener).take();
    let Some(listener) = listener else {
        return;
    };
    let _ = listener.set_nonblocking(true);
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(_) => return,
    };
    loop {
        if inner.shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Короткий тайм-аут вместо вечного ожидания: иначе при остановке
        // движка сокет остаётся занятым и порт не освобождается.
        let incoming = tokio::select! {
            accepted = listener.accept() => Some(accepted),
            _ = tokio::time::sleep(Duration::from_millis(100)) => None,
        };
        match incoming {
            Some(Ok((stream, addr))) => {
                let inner = inner.clone();
                tokio::spawn(async move { handle_incoming(inner, stream, addr).await });
            }
            Some(Err(_)) => tokio::time::sleep(Duration::from_millis(300)).await,
            None => {}
        }
    }
    // Сокет освобождается вместе с `listener` здесь же.
}

async fn handle_incoming(inner: Arc<Inner>, mut stream: TcpStream, addr: SocketAddr) {
    let mut buf = [0u8; 68];
    if read_exact_timeout(&mut stream, &mut buf, Duration::from_secs(10))
        .await
        .is_err()
    {
        return;
    }
    let remote = match Handshake::parse(&buf) {
        Ok(remote) => remote,
        Err(_) => return,
    };
    let runtime = {
        let handles: Vec<Arc<TorrentHandle>> = rd(&inner.handles).clone();
        handles
            .iter()
            .find(|handle| handle.info_hash == remote.info_hash)
            .and_then(|handle| handle.runtime_clone())
    };
    let Some(runtime) = runtime else {
        return;
    };
    peer::accept(runtime, addr, stream, remote).await;
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_sanitized() {
        let config = EngineConfig {
            download_dir: PathBuf::new(),
            max_peers_per_torrent: 0,
            download_limit_kib: 0,
            upload_limit_kib: 0,
            listen_port: 0,
            seed_after_complete: true,
            max_active_downloads: 0,
            max_active_seeds: 0,
        }
        .sanitized();
        assert_eq!(config.max_peers_per_torrent, 1);
        assert!(!config.download_dir.as_os_str().is_empty());
        // 0 — без ограничения, слишком большие значения подрезаются.
        assert_eq!(config.max_active_downloads, 0);
        assert_eq!(config.max_active_seeds, 0);
        let huge = EngineConfig {
            max_active_downloads: 10_000,
            max_active_seeds: 10_000,
            ..config
        }
        .sanitized();
        assert_eq!(huge.max_active_downloads, MAX_ACTIVE_LIMIT);
        assert_eq!(huge.max_active_seeds, MAX_ACTIVE_LIMIT);
    }

    #[test]
    fn states_serialize_lowercase() {
        let value = serde_json::to_string(&TorrentState::Downloading).unwrap();
        assert_eq!(value, "\"downloading\"");
        let value = serde_json::to_string(&TorrentState::Metadata).unwrap();
        assert_eq!(value, "\"metadata\"");
        let value = serde_json::to_string(&TorrentState::Checking).unwrap();
        assert_eq!(value, "\"checking\"");
    }

    #[test]
    fn priority_serializes_lowercase() {
        let value = serde_json::to_string(&FilePriority::High).unwrap();
        assert_eq!(value, "\"high\"");
        let value = serde_json::to_string(&FilePriority::Skip).unwrap();
        assert_eq!(value, "\"skip\"");
        assert_eq!(FilePriority::default(), FilePriority::Normal);
    }

    #[test]
    fn plan_respects_limits_and_user_pause() {
        let entries = vec![
            PlanEntry {
                id: 1,
                kind: PlanKind::Download,
                user_paused: false,
            },
            PlanEntry {
                id: 2,
                kind: PlanKind::Download,
                user_paused: false,
            },
            PlanEntry {
                id: 3,
                kind: PlanKind::Seed,
                user_paused: false,
            },
            PlanEntry {
                id: 4,
                kind: PlanKind::Seed,
                user_paused: false,
            },
            PlanEntry {
                id: 5,
                kind: PlanKind::Download,
                user_paused: true,
            },
            PlanEntry {
                id: 6,
                kind: PlanKind::Idle,
                user_paused: false,
            },
        ];
        // Без лимитов активны все, кроме выключенных и приостановленных вручную.
        assert_eq!(plan(&entries, (0, 0)), vec![1, 2, 3, 4]);
        // Слоты загрузки и раздачи считаются раздельно, порядок — по списку.
        assert_eq!(plan(&entries, (1, 1)), vec![1, 3]);
        assert_eq!(plan(&entries, (2, 1)), vec![1, 2, 3]);
        assert!(plan(&entries, (0, 0)).contains(&2));
        // Приостановленный вручную торрент не занимает слот даже при избытке.
        assert!(!plan(&entries, (5, 5)).contains(&5));
    }

    #[test]
    fn snapshot_serializes_snake_case() {
        let event = EngineEvent::StateChanged {
            id: 7,
            state: TorrentState::Seeding,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"kind\":\"state_changed\""), "{json}");
        assert!(json.contains("\"state\":\"seeding\""), "{json}");
    }

    #[test]
    fn rates_smooth_and_reset_to_zero() {
        let atom = AtomicU64::new(0f64.to_bits());
        smooth_rate(&atom, 1000.0);
        assert!(load_f64(&atom) > 500.0);
        smooth_rate(&atom, 0.0);
        smooth_rate(&atom, 0.0);
        smooth_rate(&atom, 0.0);
        assert!(load_f64(&atom) < 400.0);
    }

    #[test]
    fn unix_time_is_positive() {
        assert!(now_unix() > 1_600_000_000);
    }
}
