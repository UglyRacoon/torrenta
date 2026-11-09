//! Torrenta — лёгкий торрент-клиент: точка входа Tauri-приложения.
//!
//! Модуль собирает всё вместе:
//! * регистрирует плагины диалогов и открытия ссылок;
//! * поднимает движок `torrent-core` с настройками из `settings.json`;
//! * кладёт [`AppEngine`] в managed-состояние Tauri (его и видят команды);
//! * применяет тему к главному окну при старте;
//! * запускает тикер, который каждые 500 мс отправляет событие `engine://state`
//!   с полным снимком состояния, а также транслирует события движка в
//!   `engine://event`;
//! * запускает наблюдатель автовыключения (`engine://auto-shutdown`);
//! * создаёт иконку в трее ([`tray`]) и показывает окно, если не выбран запуск «в трее»;
//! * синхронизирует автозапуск в реестре ([`autostart`]) с настройкой;
//! * перехватывает закрытие окна, когда включено «закрывать в трей»;
//! * держит один экземпляр приложения и открывает `.torrent`/magnet из
//!   проводника ([`single_instance`]);
//! * восстанавливает сессию при запуске и сохраняет её на диск ([`session`]);
//! * следит за папкой наблюдения и добавляет из неё `.torrent`-файлы ([`watch`]);
//! * показывает уведомление Windows о завершённой загрузке;
//! * обновляет подсказку иконки в трее текущими скоростями ([`tray`]);
//! * регистрирует 25 команд из [`commands`] и [`single_instance`].

pub mod auto_shutdown;
pub mod autostart;
pub mod commands;
pub mod session;
pub mod settings;
pub mod single_instance;
pub mod state;
pub mod tray;
pub mod watch;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tauri::utils::config::Color;
use tauri::{AppHandle, Emitter as _, Manager as _, Theme as NativeTheme};
use tauri_plugin_notification::NotificationExt as _;
use torrent_core::{EngineEvent, TorrentManager};

use crate::auto_shutdown::{AutoShutdownEvent, AUTO_SHUTDOWN_EVENT, COUNTDOWN_INTERVAL};
use crate::session::SessionRegistry;
use crate::state::{
    AppEngine, AppStateDto, ENGINE_EVENT, SIGNIFICANT_MIN_INTERVAL, STATE_EVENT, TICK_INTERVAL,
};

/// Метка главного окна из `tauri.conf.json` (нужна модулю [`tray`]).
pub(crate) const MAIN_WINDOW: &str = "main";

/// Значение темы «светлая» (совпадает с `settings::THEMES`).
const LIGHT_THEME: &str = "light";

/// Фон окна для тёмной темы — тот же, что в `tauri.conf.json`.
const DARK_BACKGROUND: Color = Color(0x0F, 0x13, 0x19, 0xFF);

/// Фон окна для светлой темы: без него при смене темы видна тёмная вспышка.
const LIGHT_BACKGROUND: Color = Color(0xF2, 0xF5, 0xF9, 0xFF);

/// Как часто проверять, изменилась ли сессия, и сохранять её на диск.
const SESSION_SAVE_INTERVAL: Duration = Duration::from_secs(2);

/// Как часто обновлять подсказку трея, даже если текст не изменился
/// (иконку могли пересоздать, и подсказка потерялась бы).
const TOOLTIP_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Заголовок уведомлений Windows.
const NOTIFICATION_TITLE: &str = "Torrenta";

/// Запускает приложение. Вызывается из `main.rs`.
///
/// Паники на этом пути нет: любая ошибка старта печатается в stderr и приводит
/// к аккуратному завершению процесса с кодом 1.
pub fn run() {
    let result = tauri::Builder::default()
        // Плагин единственного экземпляра обязан регистрироваться ПЕРВЫМ:
        // он проверяет, что приложение уже запущено, и передаёт аргументы
        // второго запуска в колбэк ниже, после чего второй процесс завершается.
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            // Второй экземпляр после колбэка умирает сам: здесь только передача
            // источника первому окну. `cwd` — каталог запуска, он нужен, если
            // проводник передал относительный путь к `.torrent`-файлу.
            match single_instance::source_from_args(argv, Some(Path::new(&cwd))) {
                Some(source) => single_instance::queue_open(app, source),
                // Источника нет (запуск ярлыка второй раз) — просто показываем окно.
                None => tray::show_main(app),
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // Уведомления Windows о завершённых загрузках. Разрешение
        // `notification:default` выдано в `capabilities/default.json`.
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            setup(app).map_err(|err| -> Box<dyn std::error::Error> { err.into() })?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::preview_torrent,
            commands::add_torrent,
            commands::set_file_selection,
            commands::recheck_torrent,
            commands::set_file_priorities,
            commands::set_torrent_limits,
            commands::pick_torrent_files,
            commands::pick_download_dir,
            commands::pick_watch_dir,
            commands::pause_torrent,
            commands::resume_torrent,
            commands::pause_all,
            commands::resume_all,
            commands::remove_torrent,
            commands::get_settings,
            commands::set_settings,
            commands::get_auto_shutdown,
            commands::set_auto_shutdown,
            commands::cancel_auto_shutdown,
            commands::reveal_torrent,
            commands::open_download_dir,
            commands::open_external,
            commands::quit_app,
            single_instance::take_open_request,
        ])
        .run(tauri::generate_context!());

    if let Err(err) = result {
        eprintln!("[torrenta] не удалось запустить приложение: {err}");
        std::process::exit(1);
    }
}

/// Инициализация состояния: пути, настройки, движок, тема, фоновые задачи.
///
/// Ошибка означает, что приложение не может работать: каталог настроек не
/// определяется, поэтому `run` печатает текст и завершает процесс.
fn setup(app: &mut tauri::App) -> Result<(), String> {
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
    // `TorrentManager::new` не требует tokio-рантайма: фоновые задачи движка
    // стартуют при первой async-команде, уже внутри рантайма Tauri.
    let manager = TorrentManager::new(config);

    let theme = loaded.theme.clone();
    let start_minimized = loaded.start_minimized;
    let autostart_enabled = loaded.autostart;
    let engine = AppEngine::new(manager, loaded, app_dir.clone(), fallback_dir);
    let handle = app.handle().clone();
    let _ = app.manage(engine);
    // Реестр сессии: помнит, откуда добавлен каждый торрент, и умеет читать
    // и писать `session.json`. Создаётся до команд, чтобы `add_torrent` успел
    // зарегистрировать источник.
    let _ = app.manage(SessionRegistry::new(&app_dir));
    // Очередь «открыть источник»: плагин single-instance и стартовые аргументы
    // складывают сюда `.torrent`-путь или magnet, пока фронтенд не готов.
    let _ = app.manage(single_instance::PendingOpen::default());

    // Окна из `tauri.conf.json` уже созданы — тему можно ставить сразу.
    apply_theme(&handle, &theme);

    // Трей создаём всегда: он нужен и для запуска «в трее», и для закрытия в трей.
    let tray_ready = match tray::init(&handle) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("[torrenta] не удалось создать иконку в трее: {err}");
            false
        }
    };

    // Автозапуск всегда синхронизируется с настройкой: это заодно обновляет путь
    // к exe после обновления приложения. Ошибка реестра не мешает старту.
    if let Err(err) = autostart::apply(autostart_enabled) {
        eprintln!("[torrenta] не удалось настроить автозапуск: {err}");
    }

    // Главное окно из конфигурации создаётся скрытым. Показываем его, если это не
    // запуск в трее; без иконки в трее показываем всегда, иначе окно не найти.
    if !start_minimized || !tray_ready {
        tray::show_main(&handle);
    }

    watch_close_request(&handle);

    // Старт с `.torrent`/magnet в аргументах (двойной щелчок в проводнике).
    // Окно в этот момент ещё грузит фронтенд, поэтому источник остаётся в
    // PendingOpen, а событие может потеряться — это нормально: фронтенд заберёт
    // запрос командой `take_open_request` при инициализации.
    if let Some(source) = single_instance::source_from_args(std::env::args().skip(1), None) {
        single_instance::queue_open(&handle, source);
    }

    spawn_state_ticker(handle.clone());
    spawn_engine_event_forwarder(handle.clone());
    spawn_auto_shutdown_watcher(handle.clone());
    // Сессию восстанавливаем уже после показа окна: добавление не блокирует
    // старт, а торренты просто появляются в списке по мере готовности.
    // Сохраняет сессию сам тикер состояния (см. `save_session`).
    spawn_session_restore(handle.clone());
    // Папка наблюдения живёт отдельной задачей: настройка читается на каждом
    // шаге, поэтому её можно включать и выключать без перезапуска.
    watch::spawn(handle);

    Ok(())
}

/// Применяет тему к главному окну: системное оформление заголовка и фон.
///
/// Вызывается при старте и после сохранения настроек. Ошибки только
/// логируются: тема не должна ломать ни запуск, ни сохранение настроек.
pub fn apply_theme(app: &AppHandle, theme: &str) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        eprintln!("[torrenta] окно «{MAIN_WINDOW}» не найдено: тема не применена");
        return;
    };

    let (native, background) = if theme == LIGHT_THEME {
        (NativeTheme::Light, LIGHT_BACKGROUND)
    } else {
        (NativeTheme::Dark, DARK_BACKGROUND)
    };

    if let Err(err) = window.set_theme(Some(native)) {
        eprintln!("[torrenta] не удалось применить тему окна: {err}");
    }
    if let Err(err) = window.set_background_color(Some(background)) {
        eprintln!("[torrenta] не удалось задать цвет фона окна: {err}");
    }
}

/// Перехватывает закрытие главного окна, когда включена настройка «закрывать в трей».
///
/// Флаг читается в момент события, поэтому переключатель в настройках работает
/// без перезапуска приложения. При выключенном флаге окно закрывается обычным
/// образом и приложение завершается вместе с ним.
fn watch_close_request(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        eprintln!("[torrenta] окно «{MAIN_WINDOW}» не найдено: закрытие в трей недоступно");
        return;
    };

    let handle = app.clone();
    window.on_window_event(move |event| {
        let tauri::WindowEvent::CloseRequested { api, .. } = event else {
            return;
        };

        let close_to_tray = handle
            .try_state::<AppEngine>()
            .map(|engine| engine.settings().close_to_tray)
            .unwrap_or(false);
        if !close_to_tray {
            return;
        }

        // Крестик только прячет окно: приложение живёт в трее, а завершает его
        // пункт «Выход» меню трея (см. [`tray`]).
        api.prevent_close();
        if let Some(window) = handle.get_webview_window(MAIN_WINDOW) {
            if let Err(err) = window.hide() {
                eprintln!("[torrenta] не удалось скрыть окно в трей: {err}");
            }
        }
    });
}

/// Тикер состояния: раз в [`TICK_INTERVAL`] собирает снимок и отправляет его
/// фронтенду, если что-то действительно изменилось.
fn spawn_state_ticker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut last_significant: Option<u64> = None;
        let mut last_dynamic: Option<u64> = None;
        let mut last_significant_emit: Option<Instant> = None;
        // Подсказка трея обновляется «по изменению», но не чаще раза в две
        // секунды, и раз в две секунды — даже без изменений (иконку могли
        // пересоздать).
        let mut last_tooltip = String::new();
        let mut last_tooltip_at: Option<Instant> = None;
        // Сессия сохраняется тем же тикером, что и состояние: отдельный таймер
        // не нужен, а запись на диск идёт только при изменении состава списка.
        let mut last_session_hash: Option<u64> = None;
        let mut last_session_save: Option<Instant> = None;

        loop {
            tokio::time::sleep(TICK_INTERVAL).await;

            // Блокировка состояния живёт только до конца выражения: между ней и
            // следующим `.await` ничего не удерживается, поэтому future — `Send`.
            let payload = match app.try_state::<AppEngine>() {
                Some(engine) => engine.state(),
                None => continue,
            };

            update_tray_tooltip(&app, &payload, &mut last_tooltip, &mut last_tooltip_at);
            save_session(&app, &mut last_session_hash, &mut last_session_save);

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

/// Обновляет подсказку трея по текущим суммарным показателям.
///
/// Отдельная функция, чтобы тикер оставался читаемым: форматирование текста
/// живёт в [`tray::tooltip_text`], здесь только решение «пора обновлять».
fn update_tray_tooltip(
    app: &AppHandle,
    payload: &AppStateDto,
    last_text: &mut String,
    last_at: &mut Option<Instant>,
) {
    let totals = &payload.totals;
    let text = tray::tooltip_text(
        totals.torrents,
        totals.active,
        totals.down_rate,
        totals.up_rate,
    );
    let due = match last_at {
        Some(at) => at.elapsed() >= TOOLTIP_REFRESH_INTERVAL,
        None => true,
    };
    if !due && *last_text == text {
        return;
    }
    tray::set_tooltip(app, &text);
    *last_text = text;
    *last_at = Some(Instant::now());
}

/// Восстанавливает торренты из `session.json` в фоне.
///
/// Флаг `restore_session` читается один раз при старте: пользователь мог
/// выключить восстановление ради «чистого» запуска. Ошибки отдельных записей
/// не прерывают восстановление остальных — см. [`session::restore_from_session`].
fn spawn_session_restore(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Some(engine) = app.try_state::<AppEngine>() else {
            eprintln!("[torrenta] сессия не восстановлена: состояние приложения не найдено");
            return;
        };
        if !engine.settings().restore_session {
            return;
        }
        let Some(registry) = app.try_state::<SessionRegistry>() else {
            eprintln!("[torrenta] сессия не восстановлена: реестр сессии не найден");
            return;
        };

        let report = session::restore_from_session(engine.inner(), registry.inner()).await;
        if report.restored > 0 || report.skipped > 0 {
            eprintln!(
                "[torrenta] сессия: восстановлено {}, пропущено {}",
                report.restored, report.skipped
            );
        }
    });
}

/// Сохраняет сессию на диск, если её состав изменился.
///
/// Вызывается из тикера, поэтому не чаще [`SESSION_SAVE_INTERVAL`] и только при
/// изменении сериализованной полезной нагрузки (сравнение по
/// [`session::payload_hash`]): прогресс загрузки в файл не попадает, иначе
/// приложение писало бы на диск каждые полсекунды. При выключенном
/// `restore_session` не сохраняется ничего — иначе файл остался бы от прошлой
/// настройки и «тихий» запуск не сработал бы.
fn save_session(app: &AppHandle, last_hash: &mut Option<u64>, last_at: &mut Option<Instant>) {
    let due = match last_at {
        Some(at) => at.elapsed() >= SESSION_SAVE_INTERVAL,
        None => true,
    };
    if !due {
        return;
    }
    // Отметку ставим до попытки записи: иначе при ошибке (нет прав, занятый
    // файл) журнал заполнялся бы на каждом тике.
    *last_at = Some(Instant::now());

    let Some(engine) = app.try_state::<AppEngine>() else {
        return;
    };
    if !engine.settings().restore_session {
        // Сбрасываем хеш: после включения настройки сессия сохранится сразу,
        // даже если она не менялась.
        *last_hash = None;
        return;
    }
    let Some(registry) = app.try_state::<SessionRegistry>() else {
        return;
    };

    let entries = registry.entries(&engine.manager.snapshot());
    let hash = session::payload_hash(&entries);
    if *last_hash == Some(hash) {
        return;
    }
    match registry.save(&entries) {
        Ok(()) => *last_hash = Some(hash),
        Err(err) => eprintln!("[torrenta] не удалось сохранить сессию: {err}"),
    }
}

/// Показывает уведомление Windows о завершённой загрузке.
///
/// Уведомление не нужно, когда пользователь и так смотрит на окно: проверяем,
/// что главное окно видимо и в фокусе. Ошибки показа только логируются —
/// уведомление не должно влиять на работу приложения.
fn notify_completed(app: &AppHandle, name: &str) {
    let Some(engine) = app.try_state::<AppEngine>() else {
        return;
    };
    if !engine.settings().notify_on_complete {
        return;
    }
    if main_window_in_foreground(app) {
        return;
    }

    let result = app
        .notification()
        .builder()
        .title(NOTIFICATION_TITLE)
        .body(format!("«{name}» загружен"))
        .show();
    if let Err(err) = result {
        eprintln!("[torrenta] не удалось показать уведомление о завершении «{name}»: {err}");
    }
}

/// Видимо ли главное окно и в фокусе ли оно.
///
/// Ошибки Tauri трактуются как «не в фокусе»: уведомление показать не страшно,
/// а пропустить его хуже.
fn main_window_in_foreground(app: &AppHandle) -> bool {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return false;
    };
    let visible = window.is_visible().unwrap_or(false);
    let focused = window.is_focused().unwrap_or(false);
    visible && focused
}

/// Транслирует события движка (`EngineEvent`) в отдельное событие `engine://event`.
///
/// Полное состояние всё равно приходит в `engine://state`, поэтому здесь
/// достаточно тонкой обёртки: только чтобы UI мог мгновенно показывать тосты.
fn spawn_engine_event_forwarder(app: AppHandle) {
    // Подписка берётся в отдельном блоке: `State` держит заимствование `app`,
    // а его нужно переместить в асинхронную задачу.
    let mut receiver = {
        let Some(engine) = app.try_state::<AppEngine>() else {
            eprintln!(
                "[torrenta] состояние приложения не инициализировано: события движка недоступны"
            );
            return;
        };
        engine.manager.subscribe()
    };

    tauri::async_runtime::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if let Err(err) = app.emit(ENGINE_EVENT, &event) {
                        eprintln!("[torrenta] не удалось отправить событие движка: {err}");
                    }
                    // Уведомление Windows показываем только на завершение:
                    // остальные события интересны лишь интерфейсу.
                    if let EngineEvent::Completed { name, .. } = &event {
                        notify_completed(&app, name);
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

/// Наблюдатель автовыключения: условие проверяется на каждое событие движка и
/// раз в секунду — чтобы отсчёт шёл и когда движок молчит.
///
/// Логика ровно по контракту:
/// * условие выполнено и таймер не взведён — `shutdown.exe /s /t 60`, отсчёт
///   начинается с 60 и сразу уходит в `engine://auto-shutdown`;
/// * таймер взведён — каждую секунду отправляется остаток, вплоть до 0;
/// * условие нарушилось (добавили торрент, сняли настройку) — `shutdown.exe /a`,
///   отсчёт сбрасывается, в событие уходит `pending_seconds: null`.
fn spawn_auto_shutdown_watcher(app: AppHandle) {
    // Как и у форвардера событий: `State` освобождается до перемещения `app`.
    let (shared, mut receiver) = {
        let Some(engine) = app.try_state::<AppEngine>() else {
            eprintln!(
                "[torrenta] состояние приложения не инициализировано: автовыключение недоступно"
            );
            return;
        };
        (engine.auto_shutdown_handle(), engine.manager.subscribe())
    };

    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(COUNTDOWN_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_error: Option<String> = None;
        let mut arm_failed = false;

        loop {
            tokio::select! {
                event = receiver.recv() => match event {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = ticker.tick() => {}
            }

            // Состояние читается заново каждый шаг: подписка могла отстать.
            // Состояние читается в отдельном блоке: `arm_auto_shutdown` требует
            // `State`, а дальше по коду он уже не нужен.
            let (ready, scheduled, armed) = {
                let Some(engine) = app.try_state::<AppEngine>() else {
                    break;
                };
                let enabled = engine.settings().auto_shutdown;
                let torrents = engine.manager.snapshot();
                let ready = auto_shutdown::should_shutdown(enabled, &torrents);
                let scheduled = shared.is_scheduled();
                // Взвод таймера и старт отсчёта живут в `AppEngine`: так наблюдатель
                // и команды не могут разойтись в поведении.
                let armed = if ready && !scheduled && !arm_failed {
                    Some(engine.arm_auto_shutdown())
                } else {
                    None
                };
                (ready, scheduled, armed)
            };

            if !ready {
                arm_failed = false;
                if shared.reset() {
                    if let Err(err) = auto_shutdown::cancel_system_shutdown() {
                        log_auto_shutdown_error(
                            &mut last_error,
                            format!("не удалось отменить выключение: {err}"),
                        );
                    }
                    emit_auto_shutdown(&app, &mut last_error, None);
                }
                continue;
            }

            if let Some(armed) = armed {
                match armed {
                    Ok(remaining) => emit_auto_shutdown(&app, &mut last_error, Some(remaining)),
                    Err(err) => {
                        // Повторные попытки бессмысленны, пока условие не
                        // изменится: иначе `shutdown.exe` дёргался бы каждую секунду.
                        arm_failed = true;
                        log_auto_shutdown_error(
                            &mut last_error,
                            format!("не удалось запланировать выключение: {err}"),
                        );
                    }
                }
                continue;
            }

            if !scheduled {
                // Таймер не взведён, но и взводить нечего (прошлая попытка
                // упала) — ждём, пока условие не изменится.
                continue;
            }

            // Отсчёт уже взведён: раз в секунду сообщаем остаток (до нуля).
            if let Some(remaining) = shared.tick() {
                emit_auto_shutdown(&app, &mut last_error, Some(remaining));
            }
        }
    });
}

/// Отправляет событие автовыключения; сбой отправки не должен ронять задачу.
fn emit_auto_shutdown(
    app: &AppHandle,
    last_error: &mut Option<String>,
    pending_seconds: Option<u32>,
) {
    let event = AutoShutdownEvent { pending_seconds };
    if let Err(err) = app.emit(AUTO_SHUTDOWN_EVENT, event) {
        log_auto_shutdown_error(
            last_error,
            format!("не удалось отправить событие автовыключения: {err}"),
        );
    }
}

/// Пишет ошибку в stderr, но не повторяет одинаковые сообщения каждый тик.
fn log_auto_shutdown_error(last_error: &mut Option<String>, message: String) {
    if last_error.as_deref() == Some(message.as_str()) {
        return;
    }
    eprintln!("[torrenta] автовыключение: {message}");
    *last_error = Some(message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn window_and_theme_constants_match_config() {
        assert_eq!(MAIN_WINDOW, "main");
        assert_eq!(LIGHT_THEME, settings::THEMES[1]);
        assert_ne!(DARK_BACKGROUND, LIGHT_BACKGROUND);
        assert_eq!(DARK_BACKGROUND.3, 0xFF, "фон должен быть непрозрачным");
        assert_eq!(LIGHT_BACKGROUND.3, 0xFF, "фон должен быть непрозрачным");
        assert_eq!(TICK_INTERVAL, Duration::from_millis(500));
    }
}
