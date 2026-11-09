//! Папка наблюдения за `.torrent`-файлами.
//!
//! Если в настройках задан `watch_dir`, приложение раз в [`WATCH_INTERVAL`]
//! просматривает этот каталог и автоматически добавляет найденные торренты.
//! Правила отбора (см. [`collect_candidates`]):
//! * расширение `.torrent` без учёта регистра;
//! * обычный файл (не каталог и не ссылка), размер больше нуля;
//! * файл не менялся дольше [`WATCH_MIN_AGE`] — чтобы не прочитать `.torrent`,
//!   который ещё копируется в папку;
//! * обход идёт по именам, поэтому порядок добавления предсказуемый.
//!
//! После добавления файл переносится в `<watch_dir>/done`. Туда же он
//! отправляется, если торрент добавить не удалось (например, он уже есть в
//! списке): иначе одна и та же ошибка повторялась бы каждый тик, пока файл
//! лежит на месте. Имена в `done` не перезаписываются — при совпадении
//! добавляется суффикс ` (2)`, ` (3)` и так далее.
//!
//! Настройки читаются из [`AppEngine`] на каждом тике, поэтому папку можно
//! включать, менять и выключать без перезапуска приложения. Задача спит между
//! тиками и не мешает выходу из приложения.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tauri::{AppHandle, Manager as _};
use torrent_core::{AddOptions, EngineError};

use crate::session::SessionRegistry;
use crate::state::AppEngine;

/// Как часто просматривается папка наблюдения.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(3);
/// Минимальный «возраст» файла: младше этого срока файл считается недописанным.
pub const WATCH_MIN_AGE: Duration = Duration::from_secs(2);
/// Подкаталог, куда переносятся обработанные файлы.
pub const WATCH_DONE_DIR: &str = "done";
/// Расширение, которое интересует наблюдателя.
pub const WATCH_EXTENSION: &str = "torrent";

/// Есть ли у файла расширение `.torrent` (регистр не важен).
pub fn has_torrent_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case(WATCH_EXTENSION))
        .unwrap_or(false)
}

/// Готов ли файл к добавлению: обычный файл, непустой, не свежий.
pub fn is_candidate(path: &Path, now: SystemTime) -> bool {
    if !has_torrent_extension(path) {
        return false;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    // Ссылки и каталоги не трогаем: `.torrent`-каталог с таким именем — точно
    // не то, что нужно добавить.
    if !meta.file_type().is_file() || meta.len() == 0 {
        return false;
    }
    match meta
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
    {
        Some(age) => age >= WATCH_MIN_AGE,
        // Время изменения в будущем (часы съехали) — считаем файл свежим,
        // чтобы не прочитать недописанный торрент.
        None => false,
    }
}

/// Список файлов, готовых к добавлению, в порядке имён.
pub fn collect_candidates(dir: &Path, now: SystemTime) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<(String, PathBuf)> = entries
        .filter_map(|item| item.ok())
        .map(|item| item.path())
        .filter(|path| is_candidate(path, now))
        .map(|path| {
            let key = path
                .file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            (key, path)
        })
        .collect();
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    candidates.into_iter().map(|(_, path)| path).collect()
}

/// Свободное имя в каталоге `done`: `name.torrent`, `name (2).torrent`, ...
pub fn unique_target(done_dir: &Path, file: &Path) -> PathBuf {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("torrent.{WATCH_EXTENSION}"));
    let candidate = done_dir.join(&name);
    if !candidate.exists() {
        return candidate;
    }

    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) => (stem.to_string(), format!(".{extension}")),
        None => (name.clone(), String::new()),
    };
    for index in 2..10_000u32 {
        let candidate = done_dir.join(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Практически недостижимо; главное — не перезаписать существующий файл.
    done_dir.join(format!("{stem} (new){extension}"))
}

/// Переносит файл в `<watch_dir>/done` и возвращает его новый путь.
pub fn move_to_done(watch_dir: &Path, file: &Path) -> Result<PathBuf, String> {
    let done_dir = watch_dir.join(WATCH_DONE_DIR);
    fs::create_dir_all(&done_dir)
        .map_err(|err| format!("не удалось создать каталог «{}»: {err}", done_dir.display()))?;
    let target = unique_target(&done_dir, file);
    match fs::rename(file, &target) {
        Ok(()) => Ok(target),
        Err(rename_err) => {
            // Другой диск или занятый файл: пробуем копирование с удалением.
            match fs::copy(file, &target) {
                Ok(_) => {
                    let _ = fs::remove_file(file);
                    Ok(target)
                }
                Err(copy_err) => Err(format!(
                    "не удалось перенести «{}» в «{}»: {rename_err}; {copy_err}",
                    file.display(),
                    target.display()
                )),
            }
        }
    }
}

/// Запускает фоновую задачу наблюдения за папкой.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut log = LoudOnce::default();
        loop {
            tokio::time::sleep(WATCH_INTERVAL).await;
            scan(&app, &mut log).await;
        }
    });
}

/// Один проход по папке наблюдения.
async fn scan(app: &AppHandle, log: &mut LoudOnce) {
    let Some(engine) = app.try_state::<AppEngine>() else {
        return;
    };
    let watch_dir = engine.settings().watch_dir.trim().to_string();
    if watch_dir.is_empty() {
        // Наблюдение выключено — не считаем это ошибкой и не логируем.
        return;
    }

    let dir = PathBuf::from(&watch_dir);
    if !dir.is_dir() {
        log.report(format!("папка наблюдения «{watch_dir}» недоступна"));
        return;
    }

    let candidates = collect_candidates(&dir, SystemTime::now());
    if candidates.is_empty() {
        return;
    }
    let registry = app.try_state::<SessionRegistry>();
    // Порядок добавления совпадает с порядком переноса в `done`.
    for file in candidates {
        let label = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.display().to_string());
        let result =
            add_from_file(&engine, registry.as_ref().map(|state| state.inner()), &file).await;
        match result {
            Ok(()) => {
                log.forget();
                if let Err(err) = move_to_done(&dir, &file) {
                    log.report(err);
                }
            }
            Err(message) => {
                // Файл всё равно убираем из папки наблюдения: иначе ошибка
                // повторялась бы каждый тик. Причина остаётся в журнале.
                log.report(format!("«{label}» не добавлен: {message}"));
                if let Err(err) = move_to_done(&dir, &file) {
                    log.report(err);
                }
            }
        }
    }
}

/// Добавляет один `.torrent` из папки наблюдения.
///
/// `registry` нужен, чтобы торрент пережил перезапуск: копия метаданных ляжет
/// рядом с сессией. Его отсутствие не мешает добавлению.
async fn add_from_file(
    engine: &AppEngine,
    registry: Option<&SessionRegistry>,
    file: &Path,
) -> Result<(), String> {
    let data = fs::read(file).map_err(|err| format!("не удалось прочитать файл: {err}"))?;
    if data.is_empty() {
        return Err("файл пустой".to_string());
    }

    let manager = engine.manager.clone();
    let id = manager
        .add_file_with(
            data.clone(),
            AddOptions {
                // Каталог по умолчанию берётся из настроек движка.
                save_path: None,
                files: None,
            },
        )
        .await
        .map_err(|err| match err {
            EngineError::Duplicate(hash) => format!("торрент уже добавлен ({hash})"),
            other => other.to_string(),
        })?;

    if let Some(registry) = registry {
        let info_hash = manager
            .snapshot()
            .into_iter()
            .find(|torrent| torrent.id == id)
            .map(|torrent| torrent.info_hash)
            .unwrap_or_default();
        if !info_hash.is_empty() {
            if let Err(err) = registry.register_file(id, &info_hash, &data) {
                eprintln!("[torrenta] наблюдение: не удалось запомнить торрент в сессии: {err}");
            }
        }
    }
    Ok(())
}

/// Запоминает последнее сообщение и печатает только изменившиеся.
///
/// Наблюдение работает постоянно, поэтому одинаковые ошибки (нет каталога,
/// файл занят) нельзя печатать каждый тик — иначе журнал распухнет.
#[derive(Debug, Default)]
struct LoudOnce {
    last: Option<String>,
}

impl LoudOnce {
    /// Печатает сообщение, если оно отличается от предыдущего.
    fn report(&mut self, message: String) {
        if self.last.as_deref() == Some(message.as_str()) {
            return;
        }
        eprintln!("[torrenta] наблюдение: {message}");
        self.last = Some(message);
    }

    /// Сбрасывает запомненное сообщение после успешного добавления.
    fn forget(&mut self) {
        self.last = None;
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;

    fn test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "torrenta-watch-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("временный каталог должен создаваться");
        dir
    }

    /// Создаёт файл с заданным «возрастом» изменения.
    fn touch(path: &Path, contents: &[u8], age: Duration) {
        fs::write(path, contents).expect("файл должен создаваться");
        let modified = SystemTime::now().checked_sub(age).expect("время в прошлом");
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("файл должен открываться");
        file.set_times(FileTimes::new().set_modified(modified))
            .expect("время изменения должно задаваться");
    }

    #[test]
    fn torrent_extension_is_case_insensitive() {
        assert!(has_torrent_extension(Path::new("a.torrent")));
        assert!(has_torrent_extension(Path::new("a.TORRENT")));
        assert!(has_torrent_extension(Path::new("a.Torrent")));
        assert!(!has_torrent_extension(Path::new("a.torrent.bak")));
        assert!(!has_torrent_extension(Path::new("a.txt")));
        assert!(!has_torrent_extension(Path::new("torrent")));
        assert!(!has_torrent_extension(Path::new("")));
    }

    #[test]
    fn candidate_requires_regular_nonempty_old_file() {
        let dir = test_dir("candidate");
        let now = SystemTime::now();

        let ready = dir.join("ready.torrent");
        touch(&ready, b"d4:infod", WATCH_MIN_AGE + Duration::from_secs(5));
        assert!(is_candidate(&ready, now));

        let fresh = dir.join("fresh.torrent");
        touch(&fresh, b"d4:infod", Duration::from_millis(100));
        assert!(!is_candidate(&fresh, now), "свежий файл ещё пишется");

        let empty = dir.join("empty.torrent");
        touch(&empty, b"", WATCH_MIN_AGE + Duration::from_secs(5));
        assert!(!is_candidate(&empty, now), "пустой файл не добавляем");

        let other = dir.join("other.txt");
        touch(&other, b"text", WATCH_MIN_AGE + Duration::from_secs(5));
        assert!(!is_candidate(&other, now), "чужое расширение не наше");

        let sub_dir = dir.join("nested.torrent");
        fs::create_dir_all(&sub_dir).expect("каталог");
        assert!(!is_candidate(&sub_dir, now), "каталоги пропускаем");
    }

    #[test]
    fn collect_candidates_filters_and_sorts_by_name() {
        let dir = test_dir("collect");
        let old = WATCH_MIN_AGE + Duration::from_secs(5);

        touch(&dir.join("b.torrent"), b"b", old);
        touch(&dir.join("A.TORRENT"), b"a", old);
        touch(&dir.join("ready.torrent"), b"r", old);
        touch(&dir.join("fresh.torrent"), b"f", Duration::from_millis(50));
        touch(&dir.join("empty.torrent"), b"", old);
        touch(&dir.join("ignore.txt"), b"t", old);

        let names: Vec<String> = collect_candidates(&dir, SystemTime::now())
            .into_iter()
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["A.TORRENT", "b.torrent", "ready.torrent"]);
    }

    #[test]
    fn collect_candidates_on_missing_dir_is_empty() {
        let dir = test_dir("missing").join("нет-такого");
        assert!(collect_candidates(&dir, SystemTime::now()).is_empty());
    }

    #[test]
    fn unique_target_never_overwrites() {
        let dir = test_dir("unique");
        let done = dir.join(WATCH_DONE_DIR);
        fs::create_dir_all(&done).expect("каталог done");

        let file = dir.join("a.torrent");
        assert_eq!(unique_target(&done, &file), done.join("a.torrent"));

        fs::write(done.join("a.torrent"), b"old").expect("файл");
        assert_eq!(unique_target(&done, &file), done.join("a (2).torrent"));

        fs::write(done.join("a (2).torrent"), b"old").expect("файл");
        assert_eq!(unique_target(&done, &file), done.join("a (3).torrent"));
    }

    #[test]
    fn unique_target_keeps_names_without_extension() {
        let dir = test_dir("unique-noext");
        let done = dir.join(WATCH_DONE_DIR);
        fs::create_dir_all(&done).expect("каталог done");
        fs::write(done.join("noext"), b"old").expect("файл");

        assert_eq!(
            unique_target(&done, &dir.join("noext")),
            done.join("noext (2)")
        );
    }

    #[test]
    fn move_to_done_creates_directory_and_moves_file() {
        let dir = test_dir("move");
        let file = dir.join("torrent.torrent");
        fs::write(&file, b"d4:infod").expect("файл");

        let moved = move_to_done(&dir, &file).expect("перенос должен работать");
        assert!(!file.exists(), "исходный файл должен исчезнуть");
        assert!(moved.is_file(), "файл должен лежать в done");
        assert_eq!(moved, dir.join(WATCH_DONE_DIR).join("torrent.torrent"));
        assert_eq!(fs::read(&moved).expect("чтение"), b"d4:infod");
    }

    #[test]
    fn loud_once_prints_only_changes() {
        let mut log = LoudOnce::default();
        log.report("одно".to_string());
        log.report("одно".to_string());
        assert_eq!(log.last.as_deref(), Some("одно"));
        log.report("другое".to_string());
        assert_eq!(log.last.as_deref(), Some("другое"));
        log.forget();
        assert!(log.last.is_none());
    }
}
