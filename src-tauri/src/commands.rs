//! Tauri-команды: единственный способ, которым фронтенд общается с движком.
//!
//! Правила модуля:
//! * ни одной паники — всё возвращает `Result<T, String>` с русским текстом;
//! * тяжёлые операции (чтение файла, диалоги, получение метаданных magnet)
//!   не блокируют поток Tauri;
//! * ошибки движка (`EngineError`) отдаются через `Display`, без двойных префиксов.
//!
//! Начиная с 1.1 добавление идёт парой `preview_torrent` + `add_torrent`, где
//! `source` — либо `magnet:...`, либо путь к `.torrent`-файлу. Разбор источника
//! вынесен в [`classify_source`], чтобы предпросмотр и добавление проверяли
//! вход одинаково.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter as _, State};
use tauri_plugin_dialog::{DialogExt as _, FilePath};
use tauri_plugin_opener as opener;
use tauri_plugin_updater::UpdaterExt as _;
use torrent_core::engine::{AddOptions, TorrentManager, TorrentPreview};

use crate::association::{self, AssociationSlot, AssociationState, UNSUPPORTED_PLATFORM};
use crate::auto_shutdown::{AutoShutdownDto, AutoShutdownEvent, AUTO_SHUTDOWN_EVENT};
use crate::session::SessionRegistry;
use crate::settings::{self, SettingsDto};
use crate::state::{engine_error_text, AppEngine, AppStateDto};

/// Максимальный размер .torrent-файла, который принимает бэкенд.
pub const MAX_TORRENT_BYTES: usize = 32 * 1024 * 1024;

/// Тайм-аут получения метаданных magnet-ссылки для предпросмотра.
///
/// Совпадает с рекомендацией контракта (25 с): дольше пользователь уже не
/// готов ждать в модальном окне добавления.
pub const MAGNET_PREVIEW_TIMEOUT: Duration = Duration::from_secs(25);

/// Префикс magnet-ссылки; сравнивается побайтово и без учёта регистра.
const MAGNET_PREFIX: &[u8] = b"magnet:";

/// Источник добавления торрента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentSource {
    /// Magnet-ссылка (`magnet:?xt=...`).
    Magnet(String),
    /// Существующий `.torrent`-файл на диске.
    File(PathBuf),
}

/// Разбирает строку источника: magnet-ссылка или путь к `.torrent`-файлу.
///
/// Проверки те же, что были в 1.0 (пустая строка, отсутствующий файл), плюс
/// проверка, что в magnet есть параметр `xt`. Сравнение префикса побайтовое:
/// так строка с кириллицей не может вызвать панику на границе символа.
pub fn classify_source(source: &str) -> Result<TorrentSource, String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err("укажите magnet-ссылку или путь к .torrent-файлу".to_string());
    }

    if has_magnet_prefix(trimmed) {
        let lowered = trimmed.to_ascii_lowercase();
        if !lowered.contains("xt=") {
            return Err("в magnet-ссылке нет параметра xt (info hash)".to_string());
        }
        return Ok(TorrentSource::Magnet(trimmed.to_string()));
    }

    let path = PathBuf::from(trimmed);
    if !path.is_file() {
        return Err(format!("файл «{}» не найден", path.display()));
    }
    Ok(TorrentSource::File(path))
}

/// Начинается ли строка с `magnet:` (регистр не важен, кириллица безопасна).
fn has_magnet_prefix(value: &str) -> bool {
    value
        .as_bytes()
        .get(..MAGNET_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(MAGNET_PREFIX))
}

/// Читает `.torrent`-файл в фоне: блокирующий ввод-вывод не держит поток Tauri.
async fn read_torrent_file(path: &Path) -> Result<Vec<u8>, String> {
    let display = path.display().to_string();
    let path = path.to_path_buf();
    tauri::async_runtime::spawn_blocking(move || read_torrent_bytes(&path))
        .await
        .map_err(|err| format!("не удалось прочитать «{display}»: {err}"))?
}

/// Синхронное чтение с проверкой размера (выполняется в блокирующем пуле).
fn read_torrent_bytes(path: &Path) -> Result<Vec<u8>, String> {
    // Размер проверяется до чтения: файл на несколько гигабайт не должен целиком
    // попасть в память только ради того, чтобы потом быть отвергнутым.
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_TORRENT_BYTES as u64 {
            return Err(too_large(path, meta.len()));
        }
    }

    let bytes = std::fs::read(path)
        .map_err(|err| format!("не удалось прочитать «{}»: {err}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("файл «{}» пустой", path.display()));
    }
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err(too_large(path, bytes.len() as u64));
    }

    Ok(bytes)
}

/// Текст ошибки «файл больше [`MAX_TORRENT_BYTES`]».
fn too_large(path: &Path, len: u64) -> String {
    format!(
        "файл «{}» слишком большой для .torrent: {} МиБ (предел {} МиБ)",
        path.display(),
        len / (1024 * 1024),
        MAX_TORRENT_BYTES / (1024 * 1024)
    )
}

/// Приводит выбранный каталог к `PathBuf`: пустая строка — «по умолчанию».
///
/// Каталог создаётся заранее, чтобы движок не падал на записи в несуществующий
/// путь; относительный путь отклоняется — иначе данные уедут в рабочий каталог
/// процесса, который пользователь не выбирал.
fn prepare_save_path(save_path: Option<String>) -> Result<Option<PathBuf>, String> {
    let Some(raw) = save_path else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(format!(
            "каталог загрузки должен быть абсолютным путём: «{trimmed}»"
        ));
    }
    if path.exists() && !path.is_dir() {
        return Err(format!("«{trimmed}» существует, но это не каталог"));
    }
    if !path.is_dir() {
        std::fs::create_dir_all(&path)
            .map_err(|err| format!("не удалось создать каталог загрузки «{trimmed}»: {err}"))?;
    }

    Ok(Some(path))
}

/// Нормализует список выбранных файлов: без дублей, по возрастанию индекса.
///
/// `None` — «все файлы»; `Some(vec![])` остаётся пустым, и движок отвечает
/// `EngineError::Selection` — ровно как требует контракт.
fn normalize_files(files: Option<Vec<usize>>) -> Option<Vec<usize>> {
    files.map(|files| {
        let mut files = files;
        files.sort_unstable();
        files.dedup();
        files
    })
}

/// Ограничивает лимиты скорости допустимым диапазоном (`0` — без лимита).
pub fn clamp_limits(down_kib: u32, up_kib: u32) -> (u32, u32) {
    (settings::clamp_rate(down_kib), settings::clamp_rate(up_kib))
}

/// Отдаёт полный снимок состояния (используется при первом рендере).
#[tauri::command]
pub async fn get_state(state: State<'_, AppEngine>) -> Result<AppStateDto, String> {
    Ok(state.state())
}

/// Разбирает источник без добавления: путь к `.torrent` или magnet-ссылка.
#[tauri::command]
pub async fn preview_torrent(
    state: State<'_, AppEngine>,
    source: String,
) -> Result<TorrentPreview, String> {
    match classify_source(&source)? {
        TorrentSource::File(path) => {
            let data = read_torrent_file(&path).await?;
            TorrentManager::preview_file(&data).map_err(|err| engine_error_text(&err))
        }
        TorrentSource::Magnet(link) => state
            .manager
            .preview_magnet(&link, MAGNET_PREVIEW_TIMEOUT)
            .await
            .map_err(|err| engine_error_text(&err)),
    }
}

/// Добавляет торрент из magnet-ссылки или `.torrent`-файла.
///
/// `save_path` — каталог загрузки для этого торрента (`None` — из настроек),
/// `files` — индексы выбранных файлов (`None` — все).
///
/// Источник запоминается в сессии (см. [`crate::session`]): при следующем
/// запуске приложение сможет восстановить торрент. Ошибка записи в сессию не
/// ломает добавление — торрент уже работает, поэтому она только логируется.
#[tauri::command]
pub async fn add_torrent(
    state: State<'_, AppEngine>,
    session: State<'_, SessionRegistry>,
    source: String,
    save_path: Option<String>,
    files: Option<Vec<usize>>,
) -> Result<u64, String> {
    let opts = AddOptions {
        save_path: prepare_save_path(save_path)?,
        files: normalize_files(files),
    };

    match classify_source(&source)? {
        TorrentSource::File(path) => {
            let data = read_torrent_file(&path).await?;
            let id = state
                .manager
                .add_file_with(data.clone(), opts)
                .await
                .map_err(|err| engine_error_text(&err))?;
            let info_hash = info_hash_of(&state.manager, id);
            if let Err(err) = session.register_file(id, &info_hash, &data) {
                eprintln!("[torrenta] не удалось запомнить торрент в сессии (id {id}): {err}");
            }
            Ok(id)
        }
        TorrentSource::Magnet(link) => {
            let id = state
                .manager
                .add_magnet_with(&link, opts)
                .await
                .map_err(|err| engine_error_text(&err))?;
            if let Err(err) = session.register_magnet(id, &link) {
                eprintln!(
                    "[torrenta] не удалось запомнить magnet-ссылку в сессии (id {id}): {err}"
                );
            }
            Ok(id)
        }
    }
}

/// Находит `info_hash` добавленного торрента по его идентификатору.
///
/// Движок возвращает только `id`, а для файла с сессией нужен `info_hash`;
/// пустая строка означает «движок ещё не показал торрент» — сессия такой
/// источник не примет и сообщит об этом вызывающей стороне.
fn info_hash_of(manager: &TorrentManager, id: u64) -> String {
    manager
        .snapshot()
        .into_iter()
        .find(|torrent| torrent.id == id)
        .map(|torrent| torrent.info_hash)
        .unwrap_or_default()
}

/// Меняет набор скачиваемых файлов у добавленного торрента.
#[tauri::command]
pub async fn set_file_selection(
    state: State<'_, AppEngine>,
    id: u64,
    files: Vec<usize>,
) -> Result<(), String> {
    state
        .manager
        .set_file_selection(id, files)
        .map_err(|err| engine_error_text(&err))
}

/// Приоритет файла торрента так, как его присылает фронтенд.
///
/// В JSON — строки `"skip" | "low" | "normal" | "high"` (контракт 1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilePriorityDto {
    /// Файл не скачивается вовсе.
    Skip,
    /// Низкий приоритет: качается, только когда нет кандидатов выше.
    Low,
    /// Обычный приоритет (поведение 1.2).
    Normal,
    /// Высокий приоритет: куски запрашиваются раньше остальных.
    High,
}

impl From<FilePriorityDto> for torrent_core::engine::FilePriority {
    fn from(dto: FilePriorityDto) -> Self {
        match dto {
            FilePriorityDto::Skip => torrent_core::engine::FilePriority::Skip,
            FilePriorityDto::Low => torrent_core::engine::FilePriority::Low,
            FilePriorityDto::Normal => torrent_core::engine::FilePriority::Normal,
            FilePriorityDto::High => torrent_core::engine::FilePriority::High,
        }
    }
}

/// Перепроверяет файлы торрента на диске (состояние «Проверка»).
///
/// Повторный вызов во время проверки движок игнорирует (no-op `Ok`), готовность
/// торрента к перепроверке проверяет сам движок (`EngineError::NotReady`).
#[tauri::command]
pub async fn recheck_torrent(state: State<'_, AppEngine>, id: u64) -> Result<(), String> {
    state
        .manager
        .recheck(id)
        .map_err(|err| engine_error_text(&err))
}

/// Меняет приоритеты файлов торрента.
///
/// `priorities` — по одному значению на файл в порядке файлов снапшота; длина
/// должна совпадать с числом файлов (иначе движок вернёт ошибку выбора).
#[tauri::command]
pub async fn set_file_priorities(
    state: State<'_, AppEngine>,
    id: u64,
    priorities: Vec<FilePriorityDto>,
) -> Result<(), String> {
    let priorities: Vec<torrent_core::engine::FilePriority> =
        priorities.into_iter().map(Into::into).collect();
    state
        .manager
        .set_file_priorities(id, priorities)
        .map_err(|err| engine_error_text(&err))
}

/// Задаёт лимиты скорости конкретного торрента (КиБ/с, `0` — без лимита).
#[tauri::command]
pub async fn set_torrent_limits(
    state: State<'_, AppEngine>,
    id: u64,
    down_kib: u32,
    up_kib: u32,
) -> Result<(), String> {
    let (down_kib, up_kib) = clamp_limits(down_kib, up_kib);
    state
        .manager
        .set_limits(id, down_kib, up_kib)
        .map_err(|err| engine_error_text(&err))
}

/// Открывает диалог выбора .torrent-файлов. Пустой список — пользователь отменил.
#[tauri::command]
pub async fn pick_torrent_files(app: AppHandle) -> Result<Vec<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<FilePath>>>();
    app.dialog()
        .file()
        .add_filter("Torrent-файлы", &["torrent"])
        .set_title("Выберите .torrent-файлы")
        .pick_files(move |files| {
            let _ = tx.send(files);
        });

    let picked = rx
        .await
        .map_err(|_| "диалог выбора файла закрылся без ответа".to_string())?;
    let Some(files) = picked else {
        return Ok(Vec::new());
    };

    Ok(files
        .into_iter()
        .filter_map(|file| file.simplified().into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
        .collect())
}

/// Открывает диалог выбора каталога для загрузок.
#[tauri::command]
pub async fn pick_download_dir(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<FilePath>>();
    app.dialog()
        .file()
        .set_title("Каталог для загрузок")
        .pick_folder(move |folder| {
            let _ = tx.send(folder);
        });

    let picked = rx
        .await
        .map_err(|_| "диалог выбора каталога закрылся без ответа".to_string())?;
    Ok(picked
        .and_then(|folder| folder.simplified().into_path().ok())
        .map(|path| path.to_string_lossy().into_owned()))
}

/// Открывает диалог выбора папки наблюдения за `.torrent`-файлами.
///
/// Пустой ответ — пользователь отменил выбор (настройка не меняется).
#[tauri::command]
pub async fn pick_watch_dir(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<FilePath>>();
    app.dialog()
        .file()
        .set_title("Папка наблюдения")
        .pick_folder(move |folder| {
            let _ = tx.send(folder);
        });

    let picked = rx
        .await
        .map_err(|_| "диалог выбора папки наблюдения закрылся без ответа".to_string())?;
    Ok(picked
        .and_then(|folder| folder.simplified().into_path().ok())
        .map(|path| path.to_string_lossy().into_owned()))
}

/// Ставит торрент на паузу.
#[tauri::command]
pub async fn pause_torrent(state: State<'_, AppEngine>, id: u64) -> Result<(), String> {
    state
        .manager
        .pause(id)
        .map_err(|err| engine_error_text(&err))
}

/// Возобновляет торрент.
#[tauri::command]
pub async fn resume_torrent(state: State<'_, AppEngine>, id: u64) -> Result<(), String> {
    state
        .manager
        .resume(id)
        .map_err(|err| engine_error_text(&err))
}

/// Ставит на паузу все торренты.
#[tauri::command]
pub async fn pause_all(state: State<'_, AppEngine>) -> Result<(), String> {
    state.manager.pause_all();
    Ok(())
}

/// Возобновляет все торренты.
#[tauri::command]
pub async fn resume_all(state: State<'_, AppEngine>) -> Result<(), String> {
    state.manager.resume_all();
    Ok(())
}

/// Удаляет торрент; `delete_files` — удалить ли уже скачанные данные.
///
/// Запись сессии тоже удаляется: после перезапуска приложение не должно
/// возвращать торрент, который пользователь убрал из списка.
#[tauri::command]
pub async fn remove_torrent(
    state: State<'_, AppEngine>,
    session: State<'_, SessionRegistry>,
    id: u64,
    delete_files: bool,
) -> Result<(), String> {
    state
        .manager
        .remove(id, delete_files)
        .await
        .map_err(|err| engine_error_text(&err))?;
    let _ = session.forget(id);
    Ok(())
}

/// Возвращает текущие настройки.
#[tauri::command]
pub async fn get_settings(state: State<'_, AppEngine>) -> Result<SettingsDto, String> {
    Ok(state.settings())
}

/// Сохраняет настройки, применяет их к движку и к заголовку окна.
///
/// Ошибки применения темы не ломают сохранение: настройки уже на диске.
#[tauri::command]
pub async fn set_settings(
    app: AppHandle,
    state: State<'_, AppEngine>,
    slot: State<'_, AssociationSlot>,
    settings: SettingsDto,
) -> Result<SettingsDto, String> {
    let previous = state.settings();
    let applied = state.apply_settings(settings)?;

    crate::apply_theme(&app, &applied.theme);

    if previous.auto_shutdown && !applied.auto_shutdown {
        // Автовыключение выключили через общие настройки: снимаем таймер.
        state.cancel_auto_shutdown();
        emit_auto_shutdown_cancelled(&app);
    }

    if applied.associate_files && !previous.associate_files {
        // Привязку `.torrent` включили только что — чиним реестр сразу, не
        // дожидаясь перезапуска. Настройки уже сохранены, поэтому сбой реестра
        // не превращаем в ошибку команды: показываем его в состоянии привязки.
        if let Ok(exe) = association_exe(&state) {
            if let Ok(next) = association_ensure(exe).await {
                store_association(&slot, next);
            }
        }
    }

    Ok(applied)
}

/// Состояние автовыключения: включена ли настройка и сколько осталось.
#[tauri::command]
pub async fn get_auto_shutdown(state: State<'_, AppEngine>) -> Result<AutoShutdownDto, String> {
    Ok(state.auto_shutdown_dto())
}

/// Включает или выключает автовыключение (настройка сохраняется на диск).
#[tauri::command]
pub async fn set_auto_shutdown(
    app: AppHandle,
    state: State<'_, AppEngine>,
    enabled: bool,
) -> Result<AutoShutdownDto, String> {
    let dto = state.set_auto_shutdown(enabled)?;
    if !dto.enabled {
        emit_auto_shutdown_cancelled(&app);
    }
    Ok(dto)
}

/// Отменяет идущий обратный отсчёт и выключение компьютера.
///
/// Настройка при этом выключается: иначе наблюдатель увидит выполненное условие
/// и взведёт таймер снова через секунду — кнопка «Отменить» была бы бесполезной.
#[tauri::command]
pub async fn cancel_auto_shutdown(
    app: AppHandle,
    state: State<'_, AppEngine>,
) -> Result<AutoShutdownDto, String> {
    // Сначала гасится настройка: иначе наблюдатель увидит выполненное условие и
    // взведёт таймер снова через секунду — кнопка «Отменить» была бы бесполезной.
    let dto = state.set_auto_shutdown(false)?;
    state.cancel_auto_shutdown();
    emit_auto_shutdown_cancelled(&app);
    Ok(dto)
}

/// Отправляет фронтенду «отсчёт отменён» (`pending_seconds: null`).
fn emit_auto_shutdown_cancelled(app: &AppHandle) {
    if let Err(err) = app.emit(AUTO_SHUTDOWN_EVENT, AutoShutdownEvent::cancelled()) {
        eprintln!("[torrenta] не удалось отправить событие автовыключения: {err}");
    }
}

/// Открывает проводник с выделенным файлом (или каталогом) торрента.
#[tauri::command]
pub async fn reveal_torrent(state: State<'_, AppEngine>, id: u64) -> Result<(), String> {
    let snapshot = state.manager.snapshot();
    let torrent = snapshot
        .into_iter()
        .find(|torrent| torrent.id == id)
        .ok_or_else(|| format!("торрент {id} не найден"))?;

    let dir = PathBuf::from(&torrent.save_path);
    let file = if torrent.files.len() == 1 {
        let path = dir.join(&torrent.files[0].path);
        path.is_file().then_some(path)
    } else {
        None
    };

    let target = match file {
        Some(path) => path,
        None if dir.is_dir() => dir,
        None => {
            return Err(format!(
                "файлы торрента ещё не загружены: «{}»",
                dir.display()
            ))
        }
    };

    opener::reveal_item_in_dir(&target)
        .map_err(|err| format!("не удалось открыть проводник: {err}"))
}

/// Открывает каталог загрузок в системном файловом менеджере.
#[tauri::command]
pub async fn open_download_dir(state: State<'_, AppEngine>) -> Result<(), String> {
    let dir = PathBuf::from(state.settings().download_dir);
    if !dir.is_dir() {
        return Err(format!(
            "каталог загрузок «{}» не существует",
            dir.display()
        ));
    }

    opener::open_path(&dir, None::<&str>)
        .map_err(|err| format!("не удалось открыть каталог: {err}"))
}

/// Открывает внешнюю ссылку в браузере (только http/https/mailto).
#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("пустая ссылка".to_string());
    }
    if !allowed_external(trimmed) {
        return Err(format!(
            "ссылка отклонена: разрешены только http, https и mailto («{trimmed}»)"
        ));
    }

    opener::open_url(trimmed, None::<&str>)
        .map_err(|err| format!("не удалось открыть ссылку: {err}"))
}

/// Завершает приложение целиком, игнорируя режим «закрывать в трей».
///
/// То же, что пункт «Выход» в меню трея, но доступно из интерфейса: выход
/// всегда полный — окно закрывается, тикер и движок останавливаются вместе с
/// процессом. Код завершения — 0, как при штатном выходе через меню.
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Разрешены ли ссылки с такой схемой (защита от `file:`, `javascript:` и т. п.).
fn allowed_external(url: &str) -> bool {
    let lowered = url.to_ascii_lowercase();
    lowered.starts_with("http://")
        || lowered.starts_with("https://")
        || lowered.starts_with("mailto:")
}

/// Текст для интерфейса, когда обновления не настроены.
///
/// Формулировка честная: ключа подписи нет, поэтому проверять обновления нечем.
const UPDATE_NOT_CONFIGURED: &str = "обновления не настроены: нет ключа подписи";

/// Диагностика движка (команда `engine_diagnostics`).
///
/// Поля один в один повторяют `torrent_core::Diagnostics` и остаются
/// `snake_case`, как у остальных команд проекта.
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticsDto {
    /// Порт входящих подключений (`0` — ещё не выбран).
    pub listen_port: u16,
    /// Включён ли DHT.
    pub dht: bool,
    /// Порт DHT.
    pub dht_port: u16,
    /// Сколько узлов DHT известно движку.
    pub dht_nodes: usize,
    /// Включён ли PEX.
    pub pex: bool,
    /// Включены ли web seeds.
    pub web_seeds: bool,
    /// Прокси (`None` — движок работает напрямую).
    pub proxy: Option<String>,
    /// Удался ли проброс порта.
    pub port_mapping: bool,
    /// Внешний адрес проброса `адрес:порт`, если он удался.
    pub mapped_external: Option<String>,
    /// Включено ли расписание скорости.
    pub schedule_enabled: bool,
    /// Начало окна расписания, минуты от полуночи.
    pub schedule_from_min: u32,
    /// Конец окна расписания, минуты от полуночи.
    pub schedule_to_min: u32,
    /// Действует ли окно расписания прямо сейчас.
    pub schedule_active: bool,
    /// Действующий лимит загрузки (КиБ/с, `0` — без лимита).
    pub download_limit_kib: u32,
    /// Действующий лимит отдачи (КиБ/с, `0` — без лимита).
    pub upload_limit_kib: u32,
    /// Последовательная загрузка по умолчанию.
    pub sequential: bool,
    /// Сколько всего торрентов.
    pub torrents: usize,
    /// Сколько торрентов активно качается или раздаётся.
    pub active: usize,
    /// Сколько пиров подключено всего.
    pub peers: usize,
    /// Скорость загрузки, байт/с.
    pub download_bps: u64,
    /// Скорость отдачи, байт/с.
    pub upload_bps: u64,
    /// Сколько секунд работает движок.
    pub uptime_secs: u64,
}

impl From<torrent_core::Diagnostics> for DiagnosticsDto {
    fn from(diagnostics: torrent_core::Diagnostics) -> Self {
        Self {
            listen_port: diagnostics.listen_port,
            dht: diagnostics.dht,
            dht_port: diagnostics.dht_port,
            dht_nodes: diagnostics.dht_nodes,
            pex: diagnostics.pex,
            web_seeds: diagnostics.web_seeds,
            proxy: diagnostics.proxy,
            port_mapping: diagnostics.port_mapping,
            mapped_external: diagnostics.mapped_external,
            schedule_enabled: diagnostics.schedule_enabled,
            schedule_from_min: diagnostics.schedule_from_min,
            schedule_to_min: diagnostics.schedule_to_min,
            schedule_active: diagnostics.schedule_active,
            download_limit_kib: diagnostics.download_limit_kib,
            upload_limit_kib: diagnostics.upload_limit_kib,
            sequential: diagnostics.sequential,
            torrents: diagnostics.torrents,
            active: diagnostics.active,
            peers: diagnostics.peers,
            download_bps: diagnostics.download_bps,
            upload_bps: diagnostics.upload_bps,
            uptime_secs: diagnostics.uptime_secs,
        }
    }
}

/// Информация о каталоге данных (команда `portable_info`).
#[derive(Debug, Clone, Serialize)]
pub struct PortableInfoDto {
    /// `true`, если данные лежат рядом с `torrenta.exe`.
    pub portable: bool,
    /// Фактический каталог данных: `settings.json` и `session\`.
    pub data_dir: String,
}

/// Состояние обновлений (команда `check_update`).
#[derive(Debug, Clone, Serialize)]
pub struct UpdateStateDto {
    /// Настроена ли проверка (задан ключ подписи).
    pub configured: bool,
    /// Текущая версия приложения.
    pub current_version: String,
    /// Версия доступного обновления.
    pub available: Option<String>,
    /// Пояснение для интерфейса.
    pub message: Option<String>,
}

/// Состояние привязки `.torrent`-файлов (команды `association_status` и
/// `repair_association`).
///
/// Имена полей в JSON — camelCase: так договорились с фронтендом в 1.7; у
/// остальных DTO модуля ключи snake_case.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociationDto {
    /// ProgId текущего обработчика `.torrent` (`null` — привязки нет).
    pub handler: Option<String>,
    /// Команда открытия текущего обработчика.
    pub command: Option<String>,
    /// Открывает ли `.torrent` текущий exe Torrenta.
    pub is_owner: bool,
    /// Привязка была (пере)записана в этом вызове.
    pub repaired: bool,
    /// Пояснение для интерфейса.
    pub message: Option<String>,
    /// Платформа поддерживает ассоциации (Windows); иначе `false` и причина
    /// в `message`.
    pub available: bool,
}

impl AssociationDto {
    /// DTO из состояния [`association`] с учётом платформы.
    fn from_state(state: &AssociationState) -> Self {
        let available = cfg!(windows);
        Self {
            handler: state.handler.clone(),
            command: state.command.clone(),
            is_owner: available && state.is_owner,
            repaired: available && state.repaired,
            // Вне Windows реестра ассоциаций нет: показываем причину, а не
            // состояние, которое там всё равно не прочитать.
            message: if available {
                state.message.clone()
            } else {
                Some(UNSUPPORTED_PLATFORM.to_string())
            },
            available,
        }
    }
}

/// Включает или выключает последовательную загрузку торрента.
///
/// Порядок выбора кусков меняет движок: `Sequential` вместо `Rarest` — так
/// фильм можно смотреть, не дожидаясь конца загрузки.
#[tauri::command]
pub async fn set_torrent_sequential(
    state: State<'_, AppEngine>,
    id: u64,
    on: bool,
) -> Result<(), String> {
    state
        .manager
        .set_sequential(id, on)
        .map_err(|err| engine_error_text(&err))
}

/// Список трекеров торрента: из метаинфо плюс добавленные вручную.
#[tauri::command]
pub async fn torrent_trackers(state: State<'_, AppEngine>, id: u64) -> Result<Vec<String>, String> {
    state
        .manager
        .trackers(id)
        .map_err(|err| engine_error_text(&err))
}

/// Заменяет список трекеров торрента.
///
/// Схемы, дубликаты и лимит в 100 проверяет движок: слой Tauri не повторяет
/// эту логику, иначе правила разъедутся. Пустой список разрешён (DHT и PEX
/// работают и без трекеров).
#[tauri::command]
pub async fn set_torrent_trackers(
    state: State<'_, AppEngine>,
    id: u64,
    urls: Vec<String>,
) -> Result<(), String> {
    state
        .manager
        .set_trackers(id, urls)
        .map_err(|err| engine_error_text(&err))
}

/// Объявляет трекеры торрента заново (кнопка «Обновить трекеры»).
#[tauri::command]
pub async fn reannounce_torrent(state: State<'_, AppEngine>, id: u64) -> Result<(), String> {
    state
        .manager
        .reannounce(id)
        .map_err(|err| engine_error_text(&err))
}

/// Диагностика движка для окна «Диагностика».
#[tauri::command]
pub async fn engine_diagnostics(state: State<'_, AppEngine>) -> Result<DiagnosticsDto, String> {
    Ok(state.manager.diagnostics().into())
}

/// Сообщает, работает ли приложение в портативном режиме, и где лежат данные.
#[tauri::command]
pub async fn portable_info(state: State<'_, AppEngine>) -> Result<PortableInfoDto, String> {
    Ok(PortableInfoDto {
        portable: state.portable(),
        data_dir: state.app_dir().to_string_lossy().into_owned(),
    })
}

/// Сообщает, какая программа сейчас открывает `.torrent`-файлы.
///
/// Реестр читается заново, а не берётся из состояния `setup`: пользователь мог
/// поменять обработчик через «Параметры Windows» уже после запуска. Чтение идёт
/// в блокирующем пуле — реестр не мгновенный, а поток команд занят нельзя.
#[tauri::command]
pub async fn association_status(
    state: State<'_, AppEngine>,
    slot: State<'_, AssociationSlot>,
) -> Result<AssociationDto, String> {
    let exe = association_exe(&state)?;
    let next = association_snapshot(exe).await?;
    store_association(&slot, next.clone());
    Ok(AssociationDto::from_state(&next))
}

/// Заново регистрирует Torrenta обработчиком `.torrent`-файлов.
///
/// Живой чужой обработчик не перехватывается: ключи Torrenta перезаписываются,
/// но `UserChoice*` с чужим `ProgId` остаётся нетронутым, поэтому фактическим
/// обработчиком всё равно остаётся он (`isOwner: false` в ответе).
#[tauri::command]
pub async fn repair_association(
    state: State<'_, AppEngine>,
    slot: State<'_, AssociationSlot>,
) -> Result<AssociationDto, String> {
    let exe = association_exe(&state)?;
    let next = association_repair(exe).await?;
    store_association(&slot, next.clone());
    Ok(AssociationDto::from_state(&next))
}

/// Путь к текущему exe — цель привязки `.torrent`-файлов.
///
/// Не `app_dir`: в портативном режиме привязка должна вести на работающую копию.
fn association_exe(state: &AppEngine) -> Result<PathBuf, String> {
    state
        .exe_path()
        .ok_or_else(|| "не удалось определить путь к приложению".to_string())
}

/// Читает текущую привязку в блокирующем пуле.
async fn association_snapshot(exe: PathBuf) -> Result<AssociationState, String> {
    tauri::async_runtime::spawn_blocking(move || association::status(&exe))
        .await
        .map_err(|err| format!("не удалось проверить привязку .torrent: {err}"))
}

/// Перерегистрирует привязку в блокирующем пуле.
async fn association_repair(exe: PathBuf) -> Result<AssociationState, String> {
    tauri::async_runtime::spawn_blocking(move || association::repair(&exe))
        .await
        .map_err(|err| format!("не удалось изменить привязку .torrent: {err}"))
}

/// Проверяет привязку и чинит её, если она сломана (живого чужого не трогает).
async fn association_ensure(exe: PathBuf) -> Result<AssociationState, String> {
    tauri::async_runtime::spawn_blocking(move || association::ensure(&exe))
        .await
        .map_err(|err| format!("не удалось проверить привязку .torrent: {err}"))
}

/// Кладёт новое состояние в managed-слот, переживая «отравленную» блокировку.
fn store_association(slot: &AssociationSlot, next: AssociationState) {
    match slot.lock() {
        Ok(mut current) => *current = next,
        Err(poisoned) => *poisoned.into_inner() = next,
    }
}

/// Проверяет наличие обновления.
///
/// Пока в `tauri.conf.json` нет ключа подписи, обновления считаются
/// ненастроенными: плагин выключен (`active: false`), и честный ответ —
/// `configured: false` с пояснением, а не «обновлений нет».
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<UpdateStateDto, String> {
    let current_version = app.package_info().version.to_string();
    if !pubkey_configured(updater_pubkey(&app).as_deref()) {
        return Ok(update_not_configured(&current_version));
    }

    let updater = match app.updater() {
        Ok(updater) => updater,
        // Ключ задан, но адреса обновлений — нет (`endpoints: []`). Это не
        // ошибка сети: честнее вернуть состояние с пояснением, чем ронять команду.
        Err(err) => {
            return Ok(UpdateStateDto {
                configured: true,
                message: Some(format!("обновления настроены не полностью: {err}")),
                current_version,
                available: None,
            });
        }
    };
    let update = updater
        .check()
        .await
        .map_err(|err| format!("не удалось проверить обновления: {err}"))?;

    let available = update.as_ref().map(|update| update.version.clone());
    Ok(UpdateStateDto {
        configured: true,
        current_version,
        message: Some(match &available {
            Some(version) => format!("доступно обновление {version}"),
            None => "обновлений нет".to_string(),
        }),
        available,
    })
}

/// Ключ подписи обновлений из конфигурации (`plugins.updater.pubkey`).
fn updater_pubkey(app: &AppHandle) -> Option<String> {
    let updater = app.config().plugins.0.get("updater")?;
    Some(updater.get("pubkey")?.as_str()?.to_string())
}

/// Задан ли ключ подписи: пустая строка и пробелы ключом не считаются.
fn pubkey_configured(pubkey: Option<&str>) -> bool {
    pubkey.is_some_and(|key| !key.trim().is_empty())
}

/// Ответ `check_update`, когда обновления не настроены.
///
/// Вынесено из команды: `AppHandle` в тестах не создать, а текст сообщения
/// фронтенд показывает как есть.
fn update_not_configured(current_version: &str) -> UpdateStateDto {
    UpdateStateDto {
        configured: false,
        current_version: current_version.to_string(),
        available: None,
        message: Some(UPDATE_NOT_CONFIGURED.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Временный каталог с уникальным именем (без внешних зависимостей).
    fn test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("torrenta-cmd-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("временный каталог должен создаваться");
        dir
    }

    #[test]
    fn association_dto_reports_platform_support() {
        let ours = AssociationState {
            handler: Some("Torrenta.torrent".to_string()),
            command: Some(r#""C:\Torrenta\torrenta.exe" "%1""#.to_string()),
            is_owner: true,
            repaired: true,
            message: None,
        };
        let dto = AssociationDto::from_state(&ours);
        assert_eq!(dto.handler.as_deref(), Some("Torrenta.torrent"));
        assert_eq!(dto.available, cfg!(windows));
        if cfg!(windows) {
            assert!(dto.is_owner);
            assert!(dto.repaired);
            assert!(dto.message.is_none());
        } else {
            // Вне Windows состояние не прочитать: показываем причину, а не
            // «наше» состояние, которого там быть не может.
            assert!(!dto.is_owner);
            assert!(!dto.repaired);
            assert_eq!(dto.message.as_deref(), Some(UNSUPPORTED_PLATFORM));
        }

        // Недоступная привязка: обработчика нет, причина есть — интерфейс
        // показывает её вместо галки «файлы связаны».
        let broken = AssociationState::unavailable("нет доступа к реестру".to_string());
        let dto = AssociationDto::from_state(&broken);
        assert!(dto.handler.is_none());
        assert!(!dto.is_owner);
        assert!(dto.message.is_some());
    }

    #[test]
    fn association_dto_serializes_to_camel_case_contract() {
        // Контракт с фронтендом 1.7: ключи camelCase, значения — как есть.
        let state = AssociationState {
            handler: Some("Torrenta.torrent".to_string()),
            command: None,
            is_owner: true,
            repaired: true,
            message: Some("привязка восстановлена".to_string()),
        };
        let json = serde_json::to_value(AssociationDto::from_state(&state))
            .expect("состояние привязки должно сериализоваться");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("DTO — объект")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "available",
                "command",
                "handler",
                "isOwner",
                "message",
                "repaired"
            ]
        );
        assert_eq!(json["handler"], "Torrenta.torrent");
        assert!(json["command"].is_null());
        assert_eq!(json["isOwner"], true);
        assert_eq!(json["repaired"], true);
        assert_eq!(json["message"], "привязка восстановлена");
        assert_eq!(json["available"], cfg!(windows));
    }

    #[test]
    fn magnet_source_is_detected() {
        let source = classify_source("  magnet:?xt=urn:btih:abc&dn=ubuntu  ").expect("magnet");
        assert_eq!(
            source,
            TorrentSource::Magnet("magnet:?xt=urn:btih:abc&dn=ubuntu".to_string())
        );

        // Регистр префикса не важен — так же, как в 1.0.
        assert!(matches!(
            classify_source("MAGNET:?XT=urn:btih:abc"),
            Ok(TorrentSource::Magnet(_))
        ));
    }

    #[test]
    fn magnet_without_info_hash_is_rejected() {
        let err = classify_source("magnet:?dn=ubuntu").expect_err("без xt — ошибка");
        assert!(err.contains("xt"), "неожиданный текст ошибки: {err}");
    }

    #[test]
    fn empty_source_is_rejected() {
        let err = classify_source("   ").expect_err("пустая строка — ошибка");
        assert!(err.contains("magnet"), "неожиданный текст ошибки: {err}");
    }

    #[test]
    fn torrent_file_source_is_detected() {
        let dir = test_dir("source");
        let path = dir.join("sample.torrent");
        std::fs::write(&path, b"d4:infod4:name1:xe").expect("файл должен создаваться");

        let source = classify_source(&path.to_string_lossy()).expect("путь");
        assert_eq!(source, TorrentSource::File(path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_torrent_file_is_reported() {
        let dir = test_dir("missing");
        let path = dir.join("нет-такого.torrent");
        let err = classify_source(&path.to_string_lossy()).expect_err("файла нет — ошибка");
        assert!(err.contains("не найден"), "неожиданный текст ошибки: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cyrillic_source_does_not_panic() {
        // Побайтовое сравнение префикса не должно падать на границе символа.
        let err = classify_source("торрент").expect_err("не путь и не magnet");
        assert!(err.contains("не найден"));
        assert!(!has_magnet_prefix("магнит:?xt=1"));
        assert!(!has_magnet_prefix("mag"));
    }

    #[test]
    fn save_path_holes_become_none() {
        assert_eq!(prepare_save_path(None).expect("None"), None);
        assert_eq!(
            prepare_save_path(Some("   ".to_string())).expect("пустая строка"),
            None
        );
    }

    #[test]
    fn save_path_accepts_absolute_directory_and_creates_it() {
        let dir = test_dir("save-path");
        let target = dir.join("nested").join("downloads");
        let prepared = prepare_save_path(Some(target.to_string_lossy().into_owned()))
            .expect("абсолютный путь");
        assert_eq!(prepared, Some(target.clone()));
        assert!(target.is_dir(), "каталог должен создаваться заранее");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_path_rejects_relative_path() {
        let err = prepare_save_path(Some("downloads".to_string())).expect_err("относительный путь");
        assert!(err.contains("абсолютным"), "неожиданный текст: {err}");
    }

    #[test]
    fn save_path_rejects_file_instead_of_directory() {
        let dir = test_dir("save-file");
        let file = dir.join("file.bin");
        std::fs::write(&file, b"x").expect("файл должен создаваться");
        let err = prepare_save_path(Some(file.to_string_lossy().into_owned()))
            .expect_err("файл вместо каталога");
        assert!(err.contains("не каталог"), "неожиданный текст: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_selection_is_normalized() {
        assert_eq!(normalize_files(None), None);
        assert_eq!(
            normalize_files(Some(vec![2, 0, 2, 1])),
            Some(vec![0, 1, 2]),
            "дубли убираются, порядок стабилизируется"
        );
        assert_eq!(
            normalize_files(Some(Vec::new())),
            Some(Vec::new()),
            "пустой список остаётся пустым — движок сообщит об ошибке выбора"
        );
    }

    #[test]
    fn limits_are_clamped() {
        assert_eq!(clamp_limits(0, 0), (0, 0));
        assert_eq!(clamp_limits(64, 4096), (64, 4096));
        assert_eq!(
            clamp_limits(u32::MAX, settings::MAX_RATE_LIMIT_KIB + 1),
            (settings::MAX_RATE_LIMIT_KIB, settings::MAX_RATE_LIMIT_KIB)
        );
    }

    #[test]
    fn external_links_are_restricted() {
        assert!(allowed_external("https://example.com"));
        assert!(allowed_external("HTTP://example.com"));
        assert!(allowed_external("mailto:user@example.com"));
        assert!(!allowed_external("file:///C:/Windows/System32"));
        assert!(!allowed_external("javascript:alert(1)"));
        assert!(!allowed_external("ms-settings:privacy"));
        assert!(!allowed_external(""));
    }

    #[test]
    fn torrent_size_limit_is_reasonable() {
        assert_eq!(MAX_TORRENT_BYTES, 32 * 1024 * 1024);
        assert_eq!(MAGNET_PREVIEW_TIMEOUT, Duration::from_secs(25));
    }

    #[test]
    fn torrent_file_reading_checks_size_and_emptiness() {
        let dir = test_dir("read-torrent");

        let empty = dir.join("empty.torrent");
        std::fs::write(&empty, b"").expect("файл должен создаваться");
        let err = read_torrent_bytes(&empty).expect_err("пустой файл — ошибка");
        assert!(err.contains("пустой"), "неожиданный текст: {err}");

        // Размер выставляется без записи данных: проверяется именно отказ до чтения.
        let big = dir.join("big.torrent");
        let file = std::fs::File::create(&big).expect("файл должен создаваться");
        file.set_len(MAX_TORRENT_BYTES as u64 + 1)
            .expect("размер должен выставляться");
        drop(file);
        let err = read_torrent_bytes(&big).expect_err("слишком большой файл — ошибка");
        assert!(err.contains("слишком большой"), "неожиданный текст: {err}");

        let ok = dir.join("ok.torrent");
        std::fs::write(&ok, b"d4:infod4:name1:xe").expect("файл должен создаваться");
        assert_eq!(
            read_torrent_bytes(&ok).expect("нормальный файл читается"),
            b"d4:infod4:name1:xe".to_vec()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_priority_dto_uses_lowercase_json_names() {
        for (dto, json) in [
            (FilePriorityDto::Skip, "\"skip\""),
            (FilePriorityDto::Low, "\"low\""),
            (FilePriorityDto::Normal, "\"normal\""),
            (FilePriorityDto::High, "\"high\""),
        ] {
            assert_eq!(
                serde_json::to_string(&dto).expect("приоритет должен сериализоваться"),
                json
            );
            assert_eq!(
                serde_json::from_str::<FilePriorityDto>(json).expect("приоритет должен читаться"),
                dto
            );
        }
    }

    #[test]
    fn unknown_file_priority_is_rejected() {
        assert!(
            serde_json::from_str::<FilePriorityDto>("\"urgent\"").is_err(),
            "неизвестный приоритет должен отклоняться парсером"
        );
        assert!(serde_json::from_str::<FilePriorityDto>("\"Skip\"").is_err());
        assert!(serde_json::from_str::<Vec<FilePriorityDto>>("[\"normal\",\"bogus\"]").is_err());
    }

    #[test]
    fn file_priority_dto_converts_to_core() {
        let converted: Vec<torrent_core::engine::FilePriority> = vec![
            FilePriorityDto::Skip,
            FilePriorityDto::Low,
            FilePriorityDto::Normal,
            FilePriorityDto::High,
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        assert_eq!(
            converted,
            vec![
                torrent_core::engine::FilePriority::Skip,
                torrent_core::engine::FilePriority::Low,
                torrent_core::engine::FilePriority::Normal,
                torrent_core::engine::FilePriority::High,
            ]
        );
    }

    #[test]
    fn check_update_reports_missing_pubkey() {
        // Ключа нет вовсе, он пуст или состоит из пробелов — обновления не настроены.
        assert!(!pubkey_configured(None));
        assert!(!pubkey_configured(Some("")));
        assert!(!pubkey_configured(Some("   \t ")));
        assert!(pubkey_configured(Some("MCowBQYDK2VwAyEA")));
        assert!(pubkey_configured(Some("  MCowBQYDK2VwAyEA  ")));

        let dto = update_not_configured("1.7.0");
        assert!(!dto.configured);
        assert_eq!(dto.current_version, "1.7.0");
        assert_eq!(dto.available, None);
        let message = dto.message.expect("пояснение для интерфейса");
        assert!(
            message.contains("нет ключа подписи"),
            "неожиданный текст сообщения: {message}"
        );
    }

    #[test]
    fn diagnostics_dto_uses_snake_case_json_names() {
        let diagnostics = torrent_core::Diagnostics {
            listen_port: 51_000,
            dht: true,
            dht_port: 51_001,
            dht_nodes: 12,
            pex: true,
            web_seeds: true,
            proxy: Some("socks5://127.0.0.1:1080".to_string()),
            port_mapping: true,
            mapped_external: Some("1.2.3.4:5678".to_string()),
            schedule_enabled: true,
            schedule_from_min: 60,
            schedule_to_min: 90,
            schedule_active: true,
            download_limit_kib: 2048,
            upload_limit_kib: 128,
            sequential: true,
            torrents: 3,
            active: 1,
            peers: 7,
            download_bps: 1024,
            upload_bps: 128,
            uptime_secs: 600,
        };
        let value = serde_json::to_value(DiagnosticsDto::from(diagnostics))
            .expect("диагностика должна сериализоваться");

        for key in [
            "listen_port",
            "dht",
            "dht_port",
            "dht_nodes",
            "pex",
            "web_seeds",
            "proxy",
            "port_mapping",
            "mapped_external",
            "schedule_enabled",
            "schedule_from_min",
            "schedule_to_min",
            "schedule_active",
            "download_limit_kib",
            "upload_limit_kib",
            "sequential",
            "torrents",
            "active",
            "peers",
            "download_bps",
            "upload_bps",
            "uptime_secs",
        ] {
            assert!(value.get(key).is_some(), "в JSON нет поля «{key}»: {value}");
        }
        assert_eq!(
            value.as_object().map(serde_json::Map::len),
            Some(22),
            "лишние или недостающие поля: {value}"
        );
        assert_eq!(value["mapped_external"], "1.2.3.4:5678");
        assert_eq!(value["listen_port"], 51_000);
    }

    #[test]
    fn portable_info_dto_uses_snake_case_json_names() {
        let dto = PortableInfoDto {
            portable: true,
            data_dir: "C:\\Torrenta\\TorrentaData".to_string(),
        };
        let value = serde_json::to_value(&dto).expect("информация о портативности");
        assert_eq!(value["portable"], true);
        assert_eq!(value["data_dir"], "C:\\Torrenta\\TorrentaData");
        assert_eq!(value.as_object().map(serde_json::Map::len), Some(2));
    }
}
