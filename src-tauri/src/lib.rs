//! Torrenta — лёгкий торрент-клиент: точка входа Tauri-приложения.
//!
//! Модуль собирает всё вместе:
//! * регистрирует плагины диалогов и открытия ссылок;
//! * поднимает движок `torrent-core` с настройками из `settings.json`;
//! * кладёт [`AppEngine`] в managed-состояние Tauri (его и видят команды);
//! * запускает тикер, который каждые 500 мс отправляет событие `engine://state`
//!   с полным снимком состояния, а также транслирует события движка в
//!   `engine://event`;
//! * регистрирует 16 команд из [`commands`].

pub mod commands;
pub mod settings;
pub mod state;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter as _, Manager as _};
use torrent_core::TorrentManager;

use crate::state::{
    AppEngine, ENGINE_EVENT, SIGNIFICANT_MIN_INTERVAL, STATE_EVENT, TICK_INTERVAL,
};

/// Запускает приложение. Вызывается из `main.rs`.
///
/// Паники на этом пути нет: любая ошибка старта печатается в stderr и приводит
/// к аккуратному завершению процесса с кодом 1.
pub fn run() {
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            setup(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::add_torrent_file,
            commands::add_torrent_bytes,
            commands::add_magnet,
            commands::pick_torrent_files,
            commands::pick_download_dir,
            commands::pause_torrent,
            commands::resume_torrent,
            commands::pause_all,
            commands::resume_all,
            commands::remove_torrent,
            commands::get_settings,
            commands::set_settings,
            commands::reveal_torrent,
            commands::open_download_dir,
            commands::open_external,
        ])
        .run(tauri::generate_context!());

    if let Err(err) = result {
        eprintln!("[torrenta] не удалось запустить приложение: {err}");
        std::process::exit(1);
    }
}

/// Инициализация состояния: пути, настройки, движок, тикер.
fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let app_dir = app
        .path()
        .app_config_dir()
        .map_err(|err| format!("не удалось определить каталог настроек: {err}"))?;

    let fallback_dir = app
        .path()
        .download_dir()
        .or_else(|_| app.path().home_dir())
        .unwrap_or_else(|_| std::env::temp_dir());

    let settings_path = settings::settings_path(&app_dir);
    let loaded = settings::load(&settings_path, &fallback_dir);

    // Каталог загрузки должен существовать до старта движка; если создать его
    // не удалось — не падаем, движок сообщит об ошибке по конкретному торренту.
    let download_dir = PathBuf::from(&loaded.download_dir);
    if let Err(err) = std::fs::create_dir_all(&download_dir) {
        eprintln!(
            "[torrenta] не удалось подготовить каталог загрузки «{}»: {err}",
            download_dir.display()
        );
    }

    let config = settings::to_engine_config(&loaded);
    let manager: Arc<TorrentManager> =
        tauri::async_runtime::block_on(async move { TorrentManager::new(config) });

    let engine = AppEngine::new(manager, loaded, app_dir, fallback_dir);
    let handle = app.handle().clone();
    let _ = app.manage(engine);

    spawn_state_ticker(handle.clone());
    spawn_engine_event_forwarder(handle);

    Ok(())
}

/// Тикер состояния: раз в [`TICK_INTERVAL`] собирает снимок и отправляет его
/// фронтенду, если что-то действительно изменилось.
fn spawn_state_ticker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut last_significant: Option<u64> = None;
        let mut last_dynamic: Option<u64> = None;
        let mut last_significant_emit: Option<Instant> = None;

        loop {
            tokio::time::sleep(TICK_INTERVAL).await;

            // Блокировка состояния живёт только до конца выражения: между ней и
            // следующим `.await` ничего не удерживается, поэтому future — `Send`.
            let payload = match app.try_state::<AppEngine>() {
                Some(engine) => engine.state(),
                None => continue,
            };

            let fingerprint = payload.fingerprints();
            let dynamic_changed = last_dynamic != Some(fingerprint.dynamic);
            let significant_changed = last_significant != Some(fingerprint.significant);
            let significant_due = match last_significant_emit {
                Some(emitted_at) => emitted_at.elapsed() >= SIGNIFICANT_MIN_INTERVAL,
                None => true,
            };

            // Живые показатели уходят всегда; «значимые», но не сопровождаемые
            // живыми (например, однократная ошибка), — не чаще раза в секунду.
            if !dynamic_changed && !(significant_changed && significant_due) {
                continue;
            }

            let now = Instant::now();
            if let Err(err) = app.emit(STATE_EVENT, &payload) {
                eprintln!("[torrenta] не удалось отправить состояние: {err}");
                continue;
            }

            last_significant = Some(fingerprint.significant);
            last_dynamic = Some(fingerprint.dynamic);
            last_significant_emit = Some(now);
        }
    });
}

/// Транслирует события движка (`EngineEvent`) в отдельное событие `engine://event`.
///
/// Полное состояние всё равно приходит в `engine://state`, поэтому здесь
/// достаточно тонкой обёртки: только чтобы UI мог мгновенно показывать тосты.
fn spawn_engine_event_forwarder(app: AppHandle) {
    let Some(engine) = app.try_state::<AppEngine>() else {
        eprintln!("[torrenta] состояние приложения не инициализировано: события движка недоступны");
        return;
    };
    let mut receiver = engine.manager.subscribe();
    drop(engine);

    tauri::async_runtime::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if let Err(err) = app.emit(ENGINE_EVENT, &event) {
                        eprintln!("[torrenta] не удалось отправить событие движка: {err}");
                    }
                }
                // Отставание подписчика не критично: актуальные данные придут
                // в следующем тике `engine://state`.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Длительность одного тика (для отладки и тестов).
pub const fn tick_interval() -> Duration {
    TICK_INTERVAL
}
