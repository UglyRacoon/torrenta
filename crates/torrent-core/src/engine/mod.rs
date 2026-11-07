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

pub(crate) use torrent::Env;

use crate::magnet::Magnet;
use crate::metainfo::{InfoHash, Metainfo};
use crate::peerid;
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
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            download_dir: default_download_dir(),
            max_peers_per_torrent: 50,
            download_limit_kib: 0,
            upload_limit_kib: 0,
            listen_port: 0,
            seed_after_complete: true,
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
    Queued,
    Metadata,
    Downloading,
    Seeding,
    Paused,
    Completed,
    Error,
}

/// Файл внутри торрента.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileView {
    pub path: String,
    pub length: u64,
    pub downloaded: u64,
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

/// Разделяемые флаги торрента: пауза и удаление.
#[derive(Debug, Default)]
pub(crate) struct Flags {
    paused: AtomicBool,
    removed: AtomicBool,
}

impl Flags {
    pub(crate) fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
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
        self.total_bytes.store(meta.info.total_length, Ordering::Relaxed);
        self.piece_count
            .store(meta.info.piece_count() as u32, Ordering::Relaxed);
        self.piece_length.store(meta.info.piece_length, Ordering::Relaxed);
        *wr(&self.files) = meta
            .info
            .files
            .iter()
            .map(|file| FileView {
                path: file.path.clone(),
                length: file.length,
                downloaded: 0,
            })
            .collect();
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
}

impl TorrentHandle {
    pub(crate) fn runtime_clone(&self) -> Option<Arc<torrent::Runtime>> {
        lk(&self.runtime).clone()
    }

    pub(crate) fn effective_state(&self) -> TorrentState {
        if self.flags.paused() {
            TorrentState::Paused
        } else {
            *lk(&self.stats.state)
        }
    }

    pub(crate) fn set_state(&self, state: TorrentState) {
        *lk(&self.stats.state) = state;
        let _ = self.env.events.send(EngineEvent::StateChanged {
            id: self.id,
            state,
        });
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
        let have_bytes = self.stats.have_bytes.load(Ordering::Relaxed);
        let mut files = rd(&self.stats.files).clone();
        let per_file = self.file_bytes();
        if per_file.len() == files.len() {
            for (file, bytes) in files.iter_mut().zip(per_file) {
                file.downloaded = bytes;
            }
        }
        let down_rate = load_f64(&self.stats.down_rate);
        let up_rate = load_f64(&self.stats.up_rate);
        let eta = if total > have_bytes && down_rate > 1.0 {
            Some(((total - have_bytes) as f64 / down_rate).ceil() as u64)
        } else {
            None
        };
        TorrentSnapshot {
            id: self.id,
            name: rd(&self.stats.name).clone(),
            info_hash: self.info_hash.to_hex(),
            state,
            error: rd(&self.stats.error).clone(),
            total_bytes: total,
            downloaded_bytes: self.stats.downloaded.load(Ordering::Relaxed),
            uploaded_bytes: self.stats.uploaded.load(Ordering::Relaxed),
            progress: if total == 0 {
                0.0
            } else {
                (have_bytes as f64 / total as f64).clamp(0.0, 1.0)
            },
            piece_count: self.stats.piece_count.load(Ordering::Relaxed),
            piece_length: self.stats.piece_length.load(Ordering::Relaxed),
            pieces_have: self.stats.pieces_have.load(Ordering::Relaxed),
            peers_connected: self.stats.peers_connected.load(Ordering::Relaxed),
            peers_known: self.stats.peers_known.load(Ordering::Relaxed),
            down_rate,
            up_rate,
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
            }),
        });
        manager.ensure_background();
        manager
    }

    /// Текущая конфигурация.
    pub fn config(&self) -> EngineConfig {
        rd(&self.inner.config).clone()
    }

    /// Применяет новую конфигурацию (каталог, лимиты, порт для новых сессий).
    pub fn set_config(&self, config: EngineConfig) {
        *wr(&self.inner.config) = config.sanitized();
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
        self.ensure_background();
        let meta = Metainfo::parse(&data).map_err(|err| EngineError::Metainfo(err.to_string()))?;
        let hash = meta.info_hash;
        if self.find_by_hash(hash).is_some() {
            return Err(EngineError::Duplicate(hash.to_hex()));
        }
        let dir = save_path.unwrap_or_else(|| self.config().download_dir);
        let name = meta.info.name.clone();
        let handle = self.make_handle(hash, None, name.clone(), dir, TorrentState::Queued);
        self.push_handle(handle.clone())?;
        if let Err(message) = torrent::install(&handle, meta) {
            self.drop_handle(handle.id);
            return Err(EngineError::Io(message));
        }
        let _ = self.inner.events.send(EngineEvent::Added {
            id: handle.id,
            name,
        });
        Ok(handle.id)
    }

    /// Добавляет торрент по magnet-ссылке; метаданные подтягиваются у пиров.
    pub async fn add_magnet(
        &self,
        link: &str,
        save_path: Option<PathBuf>,
    ) -> Result<u64, EngineError> {
        self.ensure_background();
        let magnet = crate::magnet::parse(link).map_err(|err| EngineError::Magnet(err.to_string()))?;
        let hash = magnet.info_hash;
        if self.find_by_hash(hash).is_some() {
            return Err(EngineError::Duplicate(hash.to_hex()));
        }
        let dir = save_path.unwrap_or_else(|| self.config().download_dir);
        let name = magnet
            .display_name
            .clone()
            .unwrap_or_else(|| hash.to_hex());
        let handle = self.make_handle(
            hash,
            Some(magnet),
            name.clone(),
            dir,
            TorrentState::Metadata,
        );
        self.push_handle(handle.clone())?;
        let _ = self.inner.events.send(EngineEvent::Added {
            id: handle.id,
            name,
        });
        torrent::start_metadata_fetch(&handle);
        Ok(handle.id)
    }

    /// Ставит торрент на паузу (соединения закрываются, данные сохраняются).
    pub fn pause(&self, id: u64) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        handle.flags.set_paused(true);
        let _ = self.inner.events.send(EngineEvent::StateChanged {
            id,
            state: TorrentState::Paused,
        });
        Ok(())
    }

    /// Снимает торрент с паузы.
    pub fn resume(&self, id: u64) -> Result<(), EngineError> {
        self.ensure_background();
        let handle = self.find(id).ok_or(EngineError::NotFound(id))?;
        handle.flags.set_paused(false);
        let state = *lk(&handle.stats.state);
        let state = if state == TorrentState::Paused {
            if handle.runtime_clone().is_some() {
                TorrentState::Downloading
            } else {
                TorrentState::Metadata
            }
        } else {
            state
        };
        handle.set_state(state);
        if let Some(runtime) = handle.runtime_clone() {
            // Сессии на паузе закрылись, а ждать объявления трекера долго:
            // возвращаем в очередь уже известные адреса.
            runtime.requeue_peers();
        }
        if let Some(magnet) = &handle.magnet {
            if runtime_missing(&handle) {
                // Метаданные ещё не получены — возобновляем их поиск.
                let _ = magnet;
                torrent::start_metadata_fetch(&handle);
            }
        }
        Ok(())
    }

    /// Пауза для всех торрентов.
    pub fn pause_all(&self) {
        self.ensure_background();
        for handle in self.handles() {
            if !handle.flags.paused() {
                handle.flags.set_paused(true);
                let _ = self.inner.events.send(EngineEvent::StateChanged {
                    id: handle.id,
                    state: TorrentState::Paused,
                });
            }
        }
    }

    /// Снимает с паузы все торренты.
    pub fn resume_all(&self) {
        self.ensure_background();
        for handle in self.handles() {
            if handle.flags.paused() {
                handle.flags.set_paused(false);
                let state = *lk(&handle.stats.state);
                let state = if state == TorrentState::Paused {
                    if handle.runtime_clone().is_some() {
                        TorrentState::Downloading
                    } else {
                        TorrentState::Metadata
                    }
                } else {
                    state
                };
                handle.set_state(state);
                if let Some(runtime) = handle.runtime_clone() {
                    runtime.requeue_peers();
                }
            }
        }
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
            },
            added_at: now_unix(),
            save_dir,
            meta: RwLock::new(None),
            runtime: Mutex::new(None),
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

fn runtime_missing(handle: &Arc<TorrentHandle>) -> bool {
    handle.runtime_clone().is_none()
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
        }
        .sanitized();
        assert_eq!(config.max_peers_per_torrent, 1);
        assert!(!config.download_dir.as_os_str().is_empty());
    }

    #[test]
    fn states_serialize_lowercase() {
        let value = serde_json::to_string(&TorrentState::Downloading).unwrap();
        assert_eq!(value, "\"downloading\"");
        let value = serde_json::to_string(&TorrentState::Metadata).unwrap();
        assert_eq!(value, "\"metadata\"");
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
