//! Разделяемое состояние Tauri-приложения: движок, настройки, DTO для фронтенда.
//!
//! Здесь нет ни одной команды и ни одного `unwrap` на рабочих путях: модуль
//! отдаёт данные, которые команды сериализуют в ответ, а тикер — в событие
//! `engine://state`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Serialize;
use torrent_core::{EngineError, GlobalTotals, TorrentManager, TorrentSnapshot, TorrentState};

use crate::settings::{self, SettingsDto};

/// Событие с полным снимком состояния (payload — [`AppStateDto`]).
pub const STATE_EVENT: &str = "engine://state";
/// Дополнительное событие с отдельными сигналами движка (payload — `EngineEvent`).
pub const ENGINE_EVENT: &str = "engine://event";
/// Период тикера состояния.
pub const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// «Значимые» изменения отправляются не чаще одного раза в секунду.
pub const SIGNIFICANT_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Имя приложения (используется в интерфейсе и строке состояния).
pub const APP_NAME: &str = "Torrenta";

/// Статическая информация о приложении для строки состояния фронтенда.
#[derive(Debug, Clone, Serialize)]
pub struct AppInfoDto {
    /// Отображаемое имя приложения.
    pub name: String,
    /// Версия приложения.
    pub version: String,
    /// Версия движка `torrent-core`.
    pub engine: String,
    /// Фактический порт входящих подключений (`0` — ещё не выбран).
    pub listen_port: u16,
    /// Платформа в виде `os-arch`.
    pub platform: String,
}

impl AppInfoDto {
    /// Собирает информацию о приложении; `listen_port` берётся у движка.
    pub fn new(listen_port: u16) -> Self {
        Self {
            name: APP_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            engine: format!("torrent-core {}", env!("CARGO_PKG_VERSION")),
            listen_port,
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        }
    }
}

/// Полный снимок состояния, который получает фронтенд (событие и `get_state`).
#[derive(Debug, Clone, Serialize)]
pub struct AppStateDto {
    /// Все торренты движка.
    pub torrents: Vec<TorrentSnapshot>,
    /// Суммарные показатели по всем торрентам.
    pub totals: GlobalTotals,
    /// Текущие настройки.
    pub settings: SettingsDto,
    /// Информация о приложении.
    pub app: AppInfoDto,
}

impl AppStateDto {
    /// Хеши состояния: «значимая» часть и «живая» (скорости, объёмы, ETA).
    ///
    /// Значимая часть — состояние, ошибки, прогресс с точностью 0.1 %, число
    /// пиров и кусков, настройки: именно она решает, отправлять ли событие при
    /// отсутствии видимых изменений. Живая часть меняется почти каждый тик, и
    /// именно она отправляется всегда.
    pub fn fingerprints(&self) -> StateFingerprint {
        let mut significant = DefaultHasher::new();
        let mut dynamic = DefaultHasher::new();

        let totals = &self.totals;
        totals.torrents.hash(&mut significant);
        totals.active.hash(&mut significant);
        totals.downloaded_bytes.hash(&mut dynamic);
        totals.uploaded_bytes.hash(&mut dynamic);
        totals.down_rate.to_bits().hash(&mut dynamic);
        totals.up_rate.to_bits().hash(&mut dynamic);

        self.settings.hash(&mut significant);

        for torrent in &self.torrents {
            torrent.id.hash(&mut significant);
            torrent.name.hash(&mut significant);
            state_code(torrent.state).hash(&mut significant);
            match &torrent.error {
                Some(error) => {
                    1u8.hash(&mut significant);
                    error.hash(&mut significant);
                }
                None => 0u8.hash(&mut significant),
            }
            torrent.pieces_have.hash(&mut significant);
            torrent.piece_count.hash(&mut significant);
            torrent.peers_connected.hash(&mut significant);
            torrent.peers_known.hash(&mut significant);
            progress_permille(torrent.progress).hash(&mut significant);
            torrent.total_bytes.hash(&mut significant);

            torrent.downloaded_bytes.hash(&mut dynamic);
            torrent.uploaded_bytes.hash(&mut dynamic);
            torrent.down_rate.to_bits().hash(&mut dynamic);
            torrent.up_rate.to_bits().hash(&mut dynamic);
            torrent.eta_seconds.hash(&mut dynamic);
        }

        StateFingerprint {
            significant: significant.finish(),
            dynamic: dynamic.finish(),
        }
    }
}

/// Два хеша состояния: «значимый» и «живой».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateFingerprint {
    /// Состояние, ошибки, прогресс (0.1 %), пиры, куски, настройки.
    pub significant: u64,
    /// Скорости, объёмы, ETA.
    pub dynamic: u64,
}

/// Общее состояние приложения, которым владеет Tauri (`app.manage`).
///
/// Держит движок, актуальные настройки и пути конфигурации. Все методы
/// безопасны к «отравленным» блокировкам — паник на рабочих путях нет.
pub struct AppEngine {
    /// Движок торрентов (создаётся один раз при старте).
    pub manager: Arc<TorrentManager>,
    /// Текущие настройки (источник истины для `get_settings`).
    settings: RwLock<SettingsDto>,
    /// Каталог конфигурации приложения.
    app_dir: PathBuf,
    /// Путь к `settings.json`.
    settings_path: PathBuf,
    /// Каталог загрузки по умолчанию (системный «Загрузки»).
    fallback_dir: PathBuf,
}

impl AppEngine {
    /// Создаёт состояние приложения.
    pub fn new(
        manager: Arc<TorrentManager>,
        settings: SettingsDto,
        app_dir: PathBuf,
        fallback_dir: PathBuf,
    ) -> Self {
        let settings_path = settings::settings_path(&app_dir);
        Self {
            manager,
            settings: RwLock::new(settings),
            app_dir,
            settings_path,
            fallback_dir,
        }
    }

    /// Каталог конфигурации приложения.
    pub fn app_dir(&self) -> &Path {
        &self.app_dir
    }

    /// Путь к файлу настроек.
    pub fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    /// Каталог загрузки по умолчанию.
    pub fn fallback_dir(&self) -> &Path {
        &self.fallback_dir
    }

    /// Копия текущих настроек.
    pub fn settings(&self) -> SettingsDto {
        match self.settings.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Статическая информация о приложении.
    pub fn app_info(&self) -> AppInfoDto {
        AppInfoDto::new(self.manager.listen_port())
    }

    /// Полный снимок состояния для `get_state` и события `engine://state`.
    pub fn state(&self) -> AppStateDto {
        AppStateDto {
            torrents: self.manager.snapshot(),
            totals: self.manager.totals(),
            settings: self.settings(),
            app: self.app_info(),
        }
    }

    /// Проверяет, сохраняет на диск, применяет к движку и запоминает настройки.
    ///
    /// Порядок важен: сначала валидация и запись, потом применение к движку —
    /// при ошибке сохранения движок остаётся с прежней конфигурацией.
    pub fn apply_settings(&self, next: SettingsDto) -> Result<SettingsDto, String> {
        let prepared = settings::prepare(next, &self.fallback_dir)?;
        settings::save(&self.settings_path, &prepared)?;
        self.manager.set_config(settings::to_engine_config(&prepared));
        match self.settings.write() {
            Ok(mut guard) => *guard = prepared.clone(),
            Err(poisoned) => *poisoned.into_inner() = prepared.clone(),
        }
        Ok(prepared)
    }
}

/// Числовой код состояния торрента для хеширования (у `TorrentState` нет `Hash`).
fn state_code(state: TorrentState) -> u8 {
    match state {
        TorrentState::Queued => 0,
        TorrentState::Metadata => 1,
        TorrentState::Downloading => 2,
        TorrentState::Seeding => 3,
        TorrentState::Paused => 4,
        TorrentState::Completed => 5,
        TorrentState::Error => 6,
        // Страховка на случай расширения перечисления в движке.
        #[allow(unreachable_patterns)]
        _ => 7,
    }
}

/// Прогресс в промилле (0.1 %) — «значимая» точность для события состояния.
fn progress_permille(progress: f64) -> i64 {
    let clamped = if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (clamped * 1000.0).round() as i64
}

/// Человеческий текст ошибки движка (сообщения уже на русском в `Display`).
pub fn engine_error_text(error: &EngineError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn torrent(id: u64, state: TorrentState) -> TorrentSnapshot {
        TorrentSnapshot {
            id,
            name: format!("torrent-{id}"),
            info_hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
            state,
            error: None,
            total_bytes: 1024,
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            progress: 0.0,
            piece_count: 16,
            piece_length: 64,
            pieces_have: 0,
            peers_connected: 0,
            peers_known: 0,
            down_rate: 0.0,
            up_rate: 0.0,
            eta_seconds: None,
            save_path: "C:\\Torrents".to_string(),
            added_at: 1_700_000_000,
            files: Vec::new(),
            trackers: Vec::new(),
            peers: Vec::new(),
        }
    }

    fn state_with(torrents: Vec<TorrentSnapshot>) -> AppStateDto {
        AppStateDto {
            torrents,
            totals: GlobalTotals {
                torrents: 1,
                active: 1,
                down_rate: 0.0,
                up_rate: 0.0,
                downloaded_bytes: 0,
                uploaded_bytes: 0,
            },
            settings: SettingsDto::default(),
            app: AppInfoDto::new(6881),
        }
    }

    #[test]
    fn fingerprints_are_stable_for_identical_state() {
        let state = state_with(vec![torrent(1, TorrentState::Downloading)]);
        assert_eq!(state.fingerprints(), state.fingerprints());
        let other = state_with(vec![torrent(1, TorrentState::Downloading)]);
        assert_eq!(state.fingerprints(), other.fingerprints());
    }

    #[test]
    fn state_change_is_significant() {
        let a = state_with(vec![torrent(1, TorrentState::Downloading)]);
        let b = state_with(vec![torrent(1, TorrentState::Paused)]);
        assert_ne!(a.fingerprints().significant, b.fingerprints().significant);
    }

    #[test]
    fn progress_changes_in_significant_part() {
        let mut a = state_with(vec![torrent(1, TorrentState::Downloading)]);
        let mut b = a.clone();
        a.torrents[0].progress = 0.5;
        b.torrents[0].progress = 0.5;
        assert_eq!(a.fingerprints(), b.fingerprints());

        // Разница меньше 0.1 % считается шумом.
        b.torrents[0].progress = 0.500_04;
        assert_eq!(a.fingerprints().significant, b.fingerprints().significant);

        b.torrents[0].progress = 0.61;
        assert_ne!(a.fingerprints().significant, b.fingerprints().significant);
    }

    #[test]
    fn peer_and_error_changes_are_significant() {
        let a = state_with(vec![torrent(1, TorrentState::Downloading)]);
        let mut b = state_with(vec![torrent(1, TorrentState::Downloading)]);

        b.torrents[0].peers_connected = 3;
        assert_ne!(a.fingerprints().significant, b.fingerprints().significant);

        let mut c = state_with(vec![torrent(1, TorrentState::Downloading)]);
        c.torrents[0].error = Some("ошибка трекера".to_string());
        assert_ne!(a.fingerprints().significant, c.fingerprints().significant);
    }

    #[test]
    fn rates_land_in_dynamic_part() {
        let a = state_with(vec![torrent(1, TorrentState::Downloading)]);
        let mut b = state_with(vec![torrent(1, TorrentState::Downloading)]);
        b.torrents[0].down_rate = 1024.5;
        b.totals.down_rate = 1024.5;

        assert_eq!(a.fingerprints().significant, b.fingerprints().significant);
        assert_ne!(a.fingerprints().dynamic, b.fingerprints().dynamic);
    }

    #[test]
    fn settings_change_is_significant() {
        let a = state_with(vec![torrent(1, TorrentState::Downloading)]);
        let mut b = state_with(vec![torrent(1, TorrentState::Downloading)]);
        b.settings.max_peers = 120;
        assert_ne!(a.fingerprints().significant, b.fingerprints().significant);
    }

    #[test]
    fn engine_errors_are_human_readable_russian() {
        assert_eq!(
            engine_error_text(&EngineError::NotFound(7)),
            "торрент 7 не найден"
        );
        assert!(engine_error_text(&EngineError::Duplicate("ubuntu.iso".into()))
            .contains("уже добавлен"));
        assert!(engine_error_text(&EngineError::Metainfo("нет announce".into()))
            .contains("некорректный .torrent"));
        assert!(engine_error_text(&EngineError::Magnet("bad xt".into()))
            .contains("некорректная magnet-ссылка"));
        assert!(engine_error_text(&EngineError::Io("нет доступа".into()))
            .contains("ошибка ввода-вывода"));
    }

    #[test]
    fn app_info_has_expected_shape() {
        let info = AppInfoDto::new(51413);
        assert_eq!(info.name, APP_NAME);
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.engine.starts_with("torrent-core "));
        assert_eq!(info.listen_port, 51413);
        assert!(info.platform.contains('-'));
    }

    #[test]
    fn progress_permille_clamps_and_survives_nan() {
        assert_eq!(progress_permille(0.0), 0);
        assert_eq!(progress_permille(1.0), 1000);
        assert_eq!(progress_permille(1.5), 1000);
        assert_eq!(progress_permille(-1.0), 0);
        assert_eq!(progress_permille(f64::NAN), 0);
        assert_eq!(progress_permille(0.5004), 500);
    }
}
