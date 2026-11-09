//! Автовыключение компьютера после завершения всех загрузок.
//!
//! Модуль разделён на две части:
//! * чистые функции (условие запуска, аргументы `shutdown.exe`, отсчёт) — их
//!   проверяют юнит-тесты, и они никогда ничего не запускают;
//! * [`AutoShutdown`] — маленькое разделяемое состояние обратного отсчёта,
//!   которое дёргают наблюдатель (в `lib.rs`) и команды.
//!
//! Системный таймер Windows (`shutdown.exe /s /t 60`) взводится один раз, а UI
//! получает обратный отсчёт раз в секунду; при отмене отправляется `/a`.

use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use torrent_core::{TorrentSnapshot, TorrentState};

/// Событие обратного отсчёта (payload — [`AutoShutdownEvent`]).
pub const AUTO_SHUTDOWN_EVENT: &str = "engine://auto-shutdown";
/// Задержка системного таймера — она же стартовое значение обратного отсчёта.
pub const SHUTDOWN_DELAY_SECS: u32 = 60;
/// Комментарий, который Windows показывает в диалоге завершения работы.
pub const SHUTDOWN_MESSAGE: &str = "Torrenta: все загрузки завершены";
/// Шаг обратного отсчёта в интерфейсе: раз в секунду значение уменьшается на единицу.
pub const COUNTDOWN_INTERVAL: Duration = Duration::from_secs(1);
/// Исполняемый файл системного выключения (Windows).
const SHUTDOWN_EXE: &str = "shutdown.exe";

/// Состояние автовыключения для фронтенда (snake_case в JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutoShutdownDto {
    /// Включена ли настройка «выключить ПК после загрузок».
    pub enabled: bool,
    /// Сколько секунд осталось до выключения (`None` — отсчёт не идёт).
    pub pending_seconds: Option<u32>,
}

/// Payload события [`AUTO_SHUTDOWN_EVENT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AutoShutdownEvent {
    /// Сколько секунд осталось; `None` — отсчёт отменён.
    pub pending_seconds: Option<u32>,
}

impl AutoShutdownEvent {
    /// Событие отмены (`pending_seconds: null`).
    pub const fn cancelled() -> Self {
        Self {
            pending_seconds: None,
        }
    }
}

/// Считается ли состояние торрента причиной не выключать компьютер.
///
/// Блокируют выключение четыре состояния: `Queued`/`Metadata`/`Downloading`
/// (загрузка идёт или вот-вот начнётся) и `Paused` (пользователь поставил
/// торрент на паузу осознанно — выключать ПК за его спиной нельзя).
/// `Error`, `Completed` и `Seeding` не блокируют: качать больше нечего.
pub fn is_busy(state: TorrentState) -> bool {
    matches!(
        state,
        TorrentState::Queued
            | TorrentState::Metadata
            | TorrentState::Downloading
            | TorrentState::Paused
    )
}

/// Завершена ли загрузка торрента (`Completed` или `Seeding`).
///
/// Хотя бы один такой торрент нужен, иначе выключать нечего: список из одних
/// `Error`/`Paused` не даёт повода завершать работу компьютера.
pub fn is_finished(state: TorrentState) -> bool {
    matches!(state, TorrentState::Completed | TorrentState::Seeding)
}

/// Условие выключения: настройка включена, есть завершённый торрент и никто
/// не мешает (`Queued`/`Metadata`/`Downloading`/`Paused`).
pub fn should_shutdown(enabled: bool, torrents: &[TorrentSnapshot]) -> bool {
    enabled
        && torrents.iter().any(|torrent| is_finished(torrent.state))
        && !torrents.iter().any(|torrent| is_busy(torrent.state))
}

/// Аргументы запуска системного таймера: `/s /t <delay> /c <message>`.
///
/// Вынесено отдельной функцией, чтобы тест проверял ровно то, что уйдёт в
/// `shutdown.exe`, и никогда не запускал его сам.
pub fn shutdown_args(delay_secs: u32) -> Vec<String> {
    vec![
        "/s".to_string(),
        "/t".to_string(),
        delay_secs.to_string(),
        "/c".to_string(),
        SHUTDOWN_MESSAGE.to_string(),
    ]
}

/// Аргументы отмены системного таймера: `/a`.
pub fn abort_args() -> Vec<String> {
    vec!["/a".to_string()]
}

/// Запускает `shutdown.exe` с аргументами, не дожидаясь его завершения.
///
/// Процесс короткоживущий и полностью отвязан от приложения: `spawn` без
/// `wait` не блокирует поток и не мешает закрытию Torrenta.
fn run_shutdown(args: &[String]) -> Result<(), String> {
    Command::new(SHUTDOWN_EXE)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|err| format!("не удалось запустить {SHUTDOWN_EXE}: {err}"))
}

/// Взводит системный таймер выключения (60 с).
///
/// Никогда не вызывается из тестов: только из наблюдателя и команд.
pub fn schedule_system_shutdown() -> Result<(), String> {
    run_shutdown(&shutdown_args(SHUTDOWN_DELAY_SECS))
}

/// Снимает системный таймер выключения (`shutdown.exe /a`).
pub fn cancel_system_shutdown() -> Result<(), String> {
    run_shutdown(&abort_args())
}

/// Внутреннее состояние обратного отсчёта (под мьютексом).
#[derive(Debug, Default)]
struct Runtime {
    /// Остаток в секундах; `None` — отсчёт не идёт.
    pending_seconds: Option<u32>,
    /// Системный таймер взведён (иначе отменять нечего).
    scheduled: bool,
}

/// Разделяемое состояние автовыключения (живёт в `AppEngine`).
#[derive(Debug, Default)]
pub struct AutoShutdown {
    runtime: Mutex<Runtime>,
}

impl AutoShutdown {
    /// Новое состояние: настройка выключена, таймер не взведён.
    pub fn new() -> Self {
        Self::default()
    }

    /// Блокировка, устойчивая к «отравлению»: паники на рабочих путях нет.
    fn lock(&self) -> std::sync::MutexGuard<'_, Runtime> {
        match self.runtime.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Остаток обратного отсчёта в секундах.
    pub fn pending_seconds(&self) -> Option<u32> {
        self.lock().pending_seconds
    }

    /// Взведён ли системный таймер (нужно, чтобы не звать `/a` зря).
    pub fn is_scheduled(&self) -> bool {
        self.lock().scheduled
    }

    /// Начинает отсчёт после успешного взвода системного таймера.
    ///
    /// Возвращает стартовое значение — его же отправляют фронтенду.
    pub fn begin(&self) -> u32 {
        let mut runtime = self.lock();
        runtime.scheduled = true;
        runtime.pending_seconds = Some(SHUTDOWN_DELAY_SECS);
        SHUTDOWN_DELAY_SECS
    }

    /// Уменьшает отсчёт на секунду: `Some(0)` — последний тик.
    ///
    /// `None` означает, что отсчёт уже идёт вне этого состояния (или завершён)
    /// и отправлять событие не нужно.
    pub fn tick(&self) -> Option<u32> {
        let mut runtime = self.lock();
        let current = runtime.pending_seconds?;
        if current == 0 {
            return None;
        }
        let next = current - 1;
        runtime.pending_seconds = Some(next);
        Some(next)
    }

    /// Сбрасывает отсчёт; `true` — до сброса таймер был взведён.
    pub fn reset(&self) -> bool {
        let mut runtime = self.lock();
        let was_active = runtime.scheduled || runtime.pending_seconds.is_some();
        runtime.scheduled = false;
        runtime.pending_seconds = None;
        was_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::{FileView, TorrentSnapshot};

    /// Снимок торрента в заданном состоянии (остальные поля не важны для теста).
    fn snapshot(state: TorrentState) -> TorrentSnapshot {
        TorrentSnapshot {
            id: 1,
            name: "torrent".to_string(),
            info_hash: "0123456789abcdef".to_string(),
            state,
            error: None,
            total_bytes: 1024,
            downloaded_bytes: 1024,
            uploaded_bytes: 0,
            progress: 1.0,
            piece_count: 1,
            piece_length: 1024,
            pieces_have: 1,
            peers_connected: 0,
            peers_known: 0,
            down_rate: 0.0,
            up_rate: 0.0,
            eta_seconds: None,
            // P2: счётчик ratio и признак лимита раздачи к тесту отношения не
            // имеют — берём значения по умолчанию.
            seed_ratio: 0.0,
            seed_limit_reached: false,
            wanted_bytes: 1024,
            down_limit_kib: 0,
            up_limit_kib: 0,
            save_path: "C:\\Torrents".to_string(),
            added_at: 0,
            files: vec![FileView {
                path: "file.bin".to_string(),
                length: 1024,
                downloaded: 1024,
                selected: true,
                priority: torrent_core::FilePriority::Normal,
            }],
            trackers: Vec::new(),
            peers: Vec::new(),
        }
    }

    #[test]
    fn busy_states_block_shutdown() {
        assert!(is_busy(TorrentState::Queued));
        assert!(is_busy(TorrentState::Metadata));
        assert!(is_busy(TorrentState::Downloading));
        // Пауза — осознанное действие пользователя: ПК выключать нельзя.
        assert!(is_busy(TorrentState::Paused));
        // Ошибка не блокирует: попытки исчерпаны, качать больше нечего.
        assert!(!is_busy(TorrentState::Error));
        assert!(!is_busy(TorrentState::Seeding));
        assert!(!is_busy(TorrentState::Completed));
    }

    #[test]
    fn finished_states_are_completed_and_seeding() {
        assert!(is_finished(TorrentState::Completed));
        assert!(is_finished(TorrentState::Seeding));
        assert!(!is_finished(TorrentState::Paused));
        assert!(!is_finished(TorrentState::Error));
        assert!(!is_finished(TorrentState::Downloading));
        assert!(!is_finished(TorrentState::Metadata));
        assert!(!is_finished(TorrentState::Queued));
    }

    #[test]
    fn shutdown_needs_enabled_setting() {
        let done = [snapshot(TorrentState::Completed)];
        assert!(should_shutdown(true, &done));
        assert!(!should_shutdown(false, &done));

        let seeding = [snapshot(TorrentState::Seeding)];
        assert!(should_shutdown(true, &seeding));
        assert!(!should_shutdown(false, &seeding));
    }

    #[test]
    fn shutdown_needs_at_least_one_torrent() {
        assert!(!should_shutdown(true, &[]));
    }

    #[test]
    fn shutdown_needs_a_finished_torrent() {
        // Одни Paused/Error: качать нечего, но и завершённых загрузок нет.
        assert!(!should_shutdown(
            true,
            &[
                snapshot(TorrentState::Paused),
                snapshot(TorrentState::Error)
            ]
        ));
        assert!(!should_shutdown(true, &[snapshot(TorrentState::Error)]));
        assert!(!should_shutdown(true, &[snapshot(TorrentState::Paused)]));
        // Error рядом с завершённым торрентом не мешает.
        assert!(should_shutdown(
            true,
            &[
                snapshot(TorrentState::Completed),
                snapshot(TorrentState::Error)
            ]
        ));
    }

    #[test]
    fn paused_torrent_blocks_shutdown_even_beside_completed() {
        let torrents = [
            snapshot(TorrentState::Completed),
            snapshot(TorrentState::Paused),
        ];
        assert!(!should_shutdown(true, &torrents));

        let torrents = [
            snapshot(TorrentState::Seeding),
            snapshot(TorrentState::Paused),
        ];
        assert!(!should_shutdown(true, &torrents));
    }

    #[test]
    fn shutdown_waits_for_active_torrent() {
        let torrents = [
            snapshot(TorrentState::Completed),
            snapshot(TorrentState::Downloading),
        ];
        assert!(!should_shutdown(true, &torrents));

        let torrents = [
            snapshot(TorrentState::Metadata),
            snapshot(TorrentState::Seeding),
        ];
        assert!(!should_shutdown(true, &torrents));

        let torrents = [
            snapshot(TorrentState::Queued),
            snapshot(TorrentState::Completed),
        ];
        assert!(!should_shutdown(true, &torrents));
    }

    #[test]
    fn shutdown_args_match_contract() {
        assert_eq!(
            shutdown_args(60),
            vec![
                "/s".to_string(),
                "/t".to_string(),
                "60".to_string(),
                "/c".to_string(),
                "Torrenta: все загрузки завершены".to_string(),
            ]
        );
        assert_eq!(abort_args(), vec!["/a".to_string()]);
    }

    #[test]
    fn countdown_starts_at_delay_and_reaches_zero() {
        let state = AutoShutdown::new();
        assert_eq!(state.pending_seconds(), None);
        assert!(!state.is_scheduled());

        assert_eq!(state.begin(), SHUTDOWN_DELAY_SECS);
        assert_eq!(state.pending_seconds(), Some(SHUTDOWN_DELAY_SECS));
        assert!(state.is_scheduled());

        for expected in (0..SHUTDOWN_DELAY_SECS).rev() {
            assert_eq!(state.tick(), Some(expected));
            assert_eq!(state.pending_seconds(), Some(expected));
        }
        assert_eq!(state.tick(), None);
        assert_eq!(state.pending_seconds(), Some(0));
    }

    #[test]
    fn reset_clears_countdown() {
        let state = AutoShutdown::new();
        assert!(!state.reset());

        state.begin();
        assert!(state.tick().is_some());
        assert!(state.reset());
        assert_eq!(state.pending_seconds(), None);
        assert!(!state.is_scheduled());
        assert_eq!(state.tick(), None);
    }

    #[test]
    fn tick_without_begin_is_noop() {
        let state = AutoShutdown::new();
        assert_eq!(state.tick(), None);
        assert_eq!(state.pending_seconds(), None);
    }

    #[test]
    fn shared_state_is_send_and_sync() {
        fn assert_sync<T: Send + Sync>() {}
        assert_sync::<AutoShutdown>();
        assert_sync::<AutoShutdownDto>();

        let dto = AutoShutdownDto {
            enabled: true,
            pending_seconds: Some(SHUTDOWN_DELAY_SECS),
        };
        let json = serde_json::to_string(&dto).expect("DTO должен сериализоваться");
        assert_eq!(json, r#"{"enabled":true,"pending_seconds":60}"#);

        let event = AutoShutdownEvent::cancelled();
        let json = serde_json::to_string(&event).expect("событие должно сериализоваться");
        assert_eq!(json, r#"{"pending_seconds":null}"#);
    }
}
