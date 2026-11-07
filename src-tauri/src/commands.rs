//! Tauri-команды: единственный способ, которым фронтенд общается с движком.
//!
//! Правила модуля:
//! * ни одной паники — всё возвращает `Result<T, String>` с русским текстом;
//! * тяжёлые операции (чтение файла, диалоги) не блокируют поток Tauri;
//! * ошибки движка (`EngineError`) отдаются через `Display`, без двойных префиксов.

use std::path::PathBuf;

use tauri::{AppHandle, State};
use tauri_plugin_dialog::{DialogExt as _, FilePath};
use tauri_plugin_opener as opener;

use crate::settings::SettingsDto;
use crate::state::{engine_error_text, AppEngine, AppStateDto};

/// Максимальный размер .torrent-файла, который принимает бэкенд.
const MAX_TORRENT_BYTES: usize = 32 * 1024 * 1024;

/// Отдаёт полный снимок состояния (используется при первом рендере).
#[tauri::command]
pub async fn get_state(state: State<'_, AppEngine>) -> Result<AppStateDto, String> {
    Ok(state.state())
}

/// Добавляет торрент из .torrent-файла на диске.
#[tauri::command]
pub async fn add_torrent_file(
    state: State<'_, AppEngine>,
    path: String,
) -> Result<u64, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("не указан путь к .torrent-файлу".to_string());
    }

    let file_path = PathBuf::from(trimmed);
    let display = file_path.display().to_string();
    if !file_path.is_file() {
        return Err(format!("файл «{display}» не найден"));
    }

    let bytes = {
        let file_path = file_path.clone();
        let read = tauri::async_runtime::spawn_blocking(move || std::fs::read(file_path));
        match read.await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) => return Err(format!("не удалось прочитать «{display}»: {err}")),
            Err(err) => return Err(format!("не удалось прочитать «{display}»: {err}")),
        }
    };

    if bytes.is_empty() {
        return Err(format!("файл «{display}» пустой"));
    }
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err(format!(
            "файл «{display}» слишком большой для .torrent ({} МиБ)",
            bytes.len() / (1024 * 1024)
        ));
    }

    state
        .manager
        .add_file(bytes, None)
        .await
        .map_err(|err| engine_error_text(&err))
}

/// Добавляет торрент из содержимого .torrent-файла.
///
/// `name_hint` используется только в тексте ошибки (например, имя файла при
/// drag&drop), чтобы пользователь понимал, какой именно файл не подошёл.
#[tauri::command]
pub async fn add_torrent_bytes(
    state: State<'_, AppEngine>,
    bytes: Vec<u8>,
    name_hint: Option<String>,
) -> Result<u64, String> {
    let hint = name_hint
        .as_deref()
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .map(str::to_string);

    if bytes.is_empty() {
        return Err(match &hint {
            Some(name) => format!("файл «{name}» пустой"),
            None => "получен пустой .torrent-файл".to_string(),
        });
    }
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err(format!(
            "файл слишком большой для .torrent ({} МиБ)",
            bytes.len() / (1024 * 1024)
        ));
    }

    state
        .manager
        .add_file(bytes, None)
        .await
        .map_err(|err| match &hint {
            Some(name) => format!("«{name}»: {}", engine_error_text(&err)),
            None => engine_error_text(&err),
        })
}

/// Добавляет торрент по magnet-ссылке.
#[tauri::command]
pub async fn add_magnet(state: State<'_, AppEngine>, link: String) -> Result<u64, String> {
    let trimmed = link.trim();
    if trimmed.is_empty() {
        return Err("укажите magnet-ссылку".to_string());
    }
    if !trimmed.to_ascii_lowercase().starts_with("magnet:") {
        return Err("ссылка должна начинаться с magnet:".to_string());
    }

    state
        .manager
        .add_magnet(trimmed, None)
        .await
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
#[tauri::command]
pub async fn remove_torrent(
    state: State<'_, AppEngine>,
    id: u64,
    delete_files: bool,
) -> Result<(), String> {
    state
        .manager
        .remove(id, delete_files)
        .await
        .map_err(|err| engine_error_text(&err))
}

/// Возвращает текущие настройки.
#[tauri::command]
pub async fn get_settings(state: State<'_, AppEngine>) -> Result<SettingsDto, String> {
    Ok(state.settings())
}

/// Сохраняет настройки и применяет их к движку; возвращает нормализованные.
#[tauri::command]
pub async fn set_settings(
    state: State<'_, AppEngine>,
    settings: SettingsDto,
) -> Result<SettingsDto, String> {
    state.apply_settings(settings)
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

/// Разрешены ли ссылки с такой схемой (защита от `file:`, `javascript:` и т. п.).
fn allowed_external(url: &str) -> bool {
    let lowered = url.to_ascii_lowercase();
    lowered.starts_with("http://") || lowered.starts_with("https://") || lowered.starts_with("mailto:")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
