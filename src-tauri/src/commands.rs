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
use torrent_core::engine::{AddOptions, TorrentManager, TorrentPreview};

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
}
