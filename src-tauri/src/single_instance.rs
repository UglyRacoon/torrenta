//! Один экземпляр приложения и открытие `.torrent`/magnet из проводника.
//!
//! Windows запускает отдельную копию Torrenta, когда пользователь дважды
//! щёлкает `.torrent`-файл или открывает ссылку `magnet:` через ассоциацию
//! файлов. Плагин `tauri-plugin-single-instance` (он регистрируется первым в
//! `lib.rs::run`) передаёт аргументы запуска уже работающему приложению, после
//! чего второй процесс завершается сам.
//!
//! Модуль делит работу на три части:
//! * [`source_from_args`] — достаёт источник (`magnet:...` или путь к
//!   `.torrent`) из аргументов командной строки;
//! * [`PendingOpen`] — хранит источник до тех пор, пока фронтенд не будет
//!   готов его забрать ([`take_open_request`], запрос ровно один раз);
//! * [`queue_open`] — поднимает окно первого экземпляра и шлёт сигнальное
//!   событие [`OPEN_EVENT`], по которому фронтенд открывает окно добавления.

use std::path::Path;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter as _, Manager as _};

/// Событие «появился источник для добавления» (payload — `()`).
///
/// Полезной нагрузки нет: фронтенд по событию вызывает [`take_open_request`] и
/// открывает модальное окно добавления. Источник мог быть уже забран (двумя
/// обработчиками события сразу) — повторный `take` вернёт `None`, это норма.
pub const OPEN_EVENT: &str = "engine://open-source";

/// Префикс magnet-ссылки; сравнивается без учёта регистра.
const MAGNET_PREFIX: &str = "magnet:";

/// Расширение торрент-описания; сравнивается без учёта регистра.
const TORRENT_EXT: &str = ".torrent";

/// Достаёт первый подходящий источник из аргументов командной строки.
///
/// `args` — аргументы **без** имени исполняемого файла: вызывающий передаёт
/// `std::env::args().skip(1)` (свой старт) или `argv` из колбэка плагина
/// single-instance (там первый элемент — путь к exe, он просто не подойдёт).
///
/// Подходящим считается:
/// * строка с префиксом `magnet:` (без учёта регистра) — возвращается как есть;
/// * строка, оканчивающаяся на `.torrent` (без учёта регистра).
///
/// Флаги (`--foo`, `-x`), пустые строки и всё остальное игнорируются: система
/// и оболочка могут добавить к запуску служебные ключи.
///
/// Для **относительного** `.torrent`-пути при `base = Some(dir)` возвращается
/// `dir.join(path)`, но только если такой файл действительно существует; иначе
/// строка остаётся как была — существование проверит предпросмотр в UI.
/// Абсолютные пути и пути без `base` не меняются.
pub fn source_from_args(
    args: impl IntoIterator<Item = String>,
    base: Option<&Path>,
) -> Option<String> {
    args.into_iter().find_map(|arg| classify_arg(&arg, base))
}

/// Разбирает один аргумент: `None` — не источник или служебный ключ.
fn classify_arg(arg: &str, base: Option<&Path>) -> Option<String> {
    let trimmed = arg.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }
    if starts_with_ignore_case(trimmed, MAGNET_PREFIX) {
        return Some(trimmed.to_string());
    }
    if !ends_with_ignore_case(trimmed, TORRENT_EXT) {
        return None;
    }
    Some(resolve_torrent_path(trimmed, base))
}

/// Дополняет относительный `.torrent`-путь каталогом запуска, если файл найден.
fn resolve_torrent_path(raw: &str, base: Option<&Path>) -> String {
    let path = Path::new(raw);
    let Some(base) = base else {
        return raw.to_string();
    };
    if path.is_absolute() {
        return raw.to_string();
    }

    let joined = base.join(path);
    if joined.is_file() {
        joined.display().to_string()
    } else {
        raw.to_string()
    }
}

/// `value` начинается с `prefix` без учёта регистра (без паники на UTF-8).
fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// `value` оканчивается на `suffix` без учёта регистра (без паники на UTF-8).
fn ends_with_ignore_case(value: &str, suffix: &str) -> bool {
    value
        .get(value.len().saturating_sub(suffix.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// Источник, ожидающий, пока фронтенд будет готов его забрать.
///
/// Живёт в managed-состоянии Tauri. Запрос забирается ровно один раз
/// ([`PendingOpen::take`]), поэтому лишнее событие `engine://open-source` или
/// повторный `invoke` не открывают окно добавления дважды.
#[derive(Debug, Default)]
pub struct PendingOpen(Mutex<Option<String>>);

impl PendingOpen {
    /// Сохраняет источник; `None` очищает ожидание.
    pub fn set(&self, source: Option<String>) {
        // Отравленная блокировка не должна ронять приложение: данные простые.
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = source;
    }

    /// Забирает источник и очищает ожидание; повторный вызов вернёт `None`.
    pub fn take(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Пусто ли ожидание (нужно тестам и диагностике).
    pub fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
    }
}

/// Показывает окно и передаёт источник фронтенду.
///
/// Вызывается из колбэка single-instance (второй процесс), из `setup` при
/// старте с `.torrent`-файлом или magnet-ссылкой в аргументах. Окно
/// показывается всегда — даже при запуске «в трее»: пользователь явно открыл
/// файл и ждёт результата.
pub fn queue_open(app: &AppHandle, source: String) {
    match app.try_state::<PendingOpen>() {
        Some(pending) => pending.set(Some(source)),
        None => eprintln!("[torrenta] PendingOpen не зарегистрирован: источник не сохранён"),
    }

    crate::tray::show_main(app);

    if let Err(err) = app.emit(OPEN_EVENT, ()) {
        eprintln!("[torrenta] не удалось отправить событие открытия источника: {err}");
    }
}

/// Забирает ожидающий источник (`.torrent`-путь или magnet) ровно один раз.
#[tauri::command]
pub fn take_open_request(state: tauri::State<'_, PendingOpen>) -> Option<String> {
    state.take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Временный каталог с уникальным именем (без внешних зависимостей).
    fn test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "torrenta-single-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("временный каталог должен создаваться");
        dir
    }

    #[test]
    fn open_event_name_matches_contract() {
        assert_eq!(OPEN_EVENT, "engine://open-source");
    }

    #[test]
    fn magnet_argument_is_returned_as_is() {
        let args = vec!["magnet:?xt=urn:btih:abc&dn=ubuntu".to_string()];
        assert_eq!(
            source_from_args(args, None).as_deref(),
            Some("magnet:?xt=urn:btih:abc&dn=ubuntu")
        );
        // Регистр префикса не важен, как и в `commands::classify_source`.
        let upper = vec!["MAGNET:?XT=urn:btih:abc".to_string()];
        assert_eq!(
            source_from_args(upper, None).as_deref(),
            Some("MAGNET:?XT=urn:btih:abc")
        );
    }

    #[test]
    fn absolute_torrent_path_is_detected() {
        let args = vec!["C:\\Torrents\\ubuntu.iso.torrent".to_string()];
        assert_eq!(
            source_from_args(args, None).as_deref(),
            Some("C:\\Torrents\\ubuntu.iso.torrent")
        );
    }

    #[test]
    fn flags_and_other_arguments_are_skipped() {
        // Флаг перед путём не мешает: берётся первый подходящий аргумент.
        let args = vec![
            "--autostart".to_string(),
            "-q".to_string(),
            "C:\\Torrents\\a.torrent".to_string(),
        ];
        assert_eq!(
            source_from_args(args, None).as_deref(),
            Some("C:\\Torrents\\a.torrent")
        );

        // Только флаги и старый exe-путь — источника нет.
        let flags = vec![
            "C:\\Program Files\\Torrenta\\torrenta.exe".to_string(),
            "--minimized".to_string(),
            "".to_string(),
        ];
        assert_eq!(source_from_args(flags, None), None);

        // Не торрент и не magnet: обычный файл и просто строка.
        let garbage = vec!["readme.txt".to_string(), "просто текст".to_string()];
        assert_eq!(source_from_args(garbage, None), None);
    }

    #[test]
    fn path_with_spaces_and_cyrillic_is_kept() {
        let args = vec!["C:\\Мои загрузки\\Файл имя.torrent".to_string()];
        assert_eq!(
            source_from_args(args, None).as_deref(),
            Some("C:\\Мои загрузки\\Файл имя.torrent")
        );
    }

    #[test]
    fn relative_path_is_joined_with_known_base() {
        let dir = test_dir("base");
        let path = dir.join("sample.torrent");
        std::fs::write(&path, b"d4:infod4:name1:xe").expect("файл должен создаваться");

        let args = vec!["sample.torrent".to_string()];
        let resolved = source_from_args(args, Some(&dir)).expect("относительный путь");
        assert_eq!(resolved, path.display().to_string());

        // Файла нет — строка остаётся как есть, ошибку покажет предпросмотр.
        let missing = vec!["нет-такого.torrent".to_string()];
        assert_eq!(
            source_from_args(missing, Some(&dir)).as_deref(),
            Some("нет-такого.torrent")
        );

        // Без base относительный путь тоже не трогаем.
        let bare = vec!["sample.torrent".to_string()];
        assert_eq!(
            source_from_args(bare, None).as_deref(),
            Some("sample.torrent")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_open_returns_source_once() {
        let pending = PendingOpen::default();
        assert!(pending.is_empty());
        assert_eq!(pending.take(), None);

        pending.set(Some("magnet:?xt=urn:btih:abc".to_string()));
        assert!(!pending.is_empty());
        assert_eq!(pending.take().as_deref(), Some("magnet:?xt=urn:btih:abc"));
        assert!(pending.is_empty(), "после take запрос пуст");
        assert_eq!(pending.take(), None, "take-once: повтор ничего не вернёт");

        pending.set(Some("C:\\Torrents\\a.torrent".to_string()));
        pending.set(None);
        assert!(pending.is_empty(), "set(None) очищает ожидание");
    }
}
