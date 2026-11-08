//! Автозапуск Torrenta вместе с Windows.
//!
//! Автозапуск реализован без сторонних крейтов: значение пишется в ветку
//! реестра `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` утилитой
//! `reg.exe` из состава Windows. Запуск `reg.exe` выполняется с флагом
//! `CREATE_NO_WINDOW`, чтобы окно консоли не мелькало на экране.
//!
//! Модуль намеренно устроен так, что все операции идемпотентны:
//! * включение — `reg.exe add ... /f`, повторный вызов просто перезаписывает
//!   значение (это же обновляет путь к exe после обновления приложения);
//! * выключение — `reg.exe delete ... /f`, отсутствие значения не считается
//!   ошибкой.
//!
//! На не-Windows платформах все функции — заглушки, [`apply`] всегда успешен.

use std::path::Path;

/// Ветка реестра с программами автозапуска текущего пользователя.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// Имя значения в ветке автозапуска.
pub const VALUE_NAME: &str = "Torrenta";
/// Утилита для работы с реестром.
#[cfg(windows)]
const REG_EXE: &str = "reg.exe";
/// `CREATE_NO_WINDOW`: не показывать консольное окно дочернего процесса.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Полный путь ветки реестра, в которой хранится автозапуск.
pub fn key_path() -> String {
    RUN_KEY.to_string()
}

/// Путь к exe в виде значения реестра: всегда в кавычках, без лишних пробелов.
///
/// Кавычки обязательны: путь может содержать пробелы (`C:\Program Files\...`),
/// иначе Windows запустит только первый сегмент пути.
pub fn quote_exe(exe: &Path) -> String {
    let raw = exe.to_string_lossy();
    let trimmed = raw.trim().trim_matches('"').trim();
    format!("\"{trimmed}\"")
}

/// Аргументы `reg.exe`, включающие автозапуск для указанного exe.
pub fn add_args(exe: &Path) -> Vec<String> {
    add_args_for(VALUE_NAME, exe)
}

/// Аргументы `reg.exe`, выключающие автозапуск.
pub fn delete_args() -> Vec<String> {
    delete_args_for(VALUE_NAME)
}

/// Аргументы `reg.exe` для включения автозапуска под произвольным именем значения.
fn add_args_for(value: &str, exe: &Path) -> Vec<String> {
    vec![
        "add".to_string(),
        RUN_KEY.to_string(),
        "/v".to_string(),
        value.to_string(),
        "/t".to_string(),
        "REG_SZ".to_string(),
        "/d".to_string(),
        quote_exe(exe),
        "/f".to_string(),
    ]
}

/// Аргументы `reg.exe` для выключения автозапуска под произвольным именем значения.
fn delete_args_for(value: &str) -> Vec<String> {
    vec![
        "delete".to_string(),
        RUN_KEY.to_string(),
        "/v".to_string(),
        value.to_string(),
        "/f".to_string(),
    ]
}

/// Аргументы `reg.exe` для чтения значения (используется в тестах и при проверках).
#[cfg(windows)]
fn query_args_for(value: &str) -> Vec<String> {
    vec![
        "query".to_string(),
        RUN_KEY.to_string(),
        "/v".to_string(),
        value.to_string(),
    ]
}

/// Включает или выключает автозапуск Torrenta.
///
/// Функция идемпотентна и безопасна для повторных вызовов: при старте
/// приложение всегда вызывает её со значением настройки, что заодно
/// освежает путь к exe после обновления.
///
/// # Errors
///
/// Возвращает текст на русском, если `reg.exe` не удалось запустить или он
/// завершился с ошибкой (например, реестр недоступен политикой).
pub fn apply(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        apply_named(VALUE_NAME, enabled)
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Ok(())
    }
}

/// Включает или выключает автозапуск под произвольным именем значения.
#[cfg(windows)]
fn apply_named(value: &str, enabled: bool) -> Result<(), String> {
    if enabled {
        let exe = std::env::current_exe()
            .map_err(|err| format!("не удалось определить путь к Torrenta: {err}"))?;
        let output = run_reg(&add_args_for(value, &exe))?;
        if !output.status.success() {
            return Err(format!(
                "не удалось включить автозапуск: {}",
                reg_detail(&output)
            ));
        }
        return Ok(());
    }

    let output = run_reg(&delete_args_for(value))?;
    if output.status.success() {
        return Ok(());
    }
    // Отсутствие значения — нормальный случай: автозапуск уже выключен.
    match query_value(value) {
        Ok(None) => Ok(()),
        _ => Err(format!(
            "не удалось выключить автозапуск: {}",
            reg_detail(&output)
        )),
    }
}

/// Запускает `reg.exe` без окна консоли и возвращает результат запуска.
#[cfg(windows)]
fn run_reg(args: &[String]) -> Result<std::process::Output, String> {
    use std::os::windows::process::CommandExt as _;

    std::process::Command::new(REG_EXE)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|err| format!("не удалось запустить {REG_EXE}: {err}"))
}

/// Читает значение из ветки автозапуска: `Ok(None)` — значения нет.
#[cfg(windows)]
fn query_value(value: &str) -> Result<Option<String>, String> {
    let output = run_reg(&query_args_for(value))?;
    if !output.status.success() {
        // reg.exe ругается и на отсутствующее значение — это не сбой.
        return Ok(None);
    }
    Ok(parse_query_output(
        &String::from_utf8_lossy(&output.stdout),
        value,
    ))
}

/// Достаёт данные значения из вывода `reg.exe query`.
///
/// Формат строки: `    Torrenta    REG_SZ    "C:\Torrenta\torrenta.exe"`.
#[cfg(windows)]
fn parse_query_output(text: &str, value: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(value) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(data) = rest.strip_prefix("REG_SZ") else {
            continue;
        };
        return Some(data.trim().to_string());
    }
    None
}

/// Человеческое описание вывода `reg.exe` для текста ошибки.
#[cfg(windows)]
fn reg_detail(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}; {stderr}"),
        (false, true) => stdout,
        (true, false) => stderr,
        (true, true) => match output.status.code() {
            Some(code) => format!("reg.exe завершился с кодом {code}"),
            None => "reg.exe был прерван".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Имя значения для «боевых» проверок реестра: настоящую запись не трогаем.
    #[cfg(windows)]
    const PROBE_VALUE: &str = "Torrenta__test";

    #[test]
    fn key_points_to_current_user_run_branch() {
        assert_eq!(
            key_path(),
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run"
        );
    }

    #[test]
    fn quote_exe_wraps_path_with_spaces() {
        assert_eq!(
            quote_exe(Path::new(r"C:\Program Files\Torrenta\torrenta.exe")),
            r#""C:\Program Files\Torrenta\torrenta.exe""#
        );
        // Путь без пробелов тоже закавычивается: так значение читается однозначно.
        assert_eq!(
            quote_exe(Path::new(r"C:\Torrenta\torrenta.exe")),
            r#""C:\Torrenta\torrenta.exe""#
        );
        // Уже закавыченный путь не получает вторую пару кавычек.
        assert_eq!(
            quote_exe(Path::new(r#""C:\Torrenta\torrenta.exe""#)),
            r#""C:\Torrenta\torrenta.exe""#
        );
        // Случайные пробелы вокруг пути отбрасываются.
        assert_eq!(
            quote_exe(Path::new("  C:\\Torrenta\\torrenta.exe  ")),
            r#""C:\Torrenta\torrenta.exe""#
        );
    }

    #[test]
    fn add_args_write_value_with_forced_overwrite() {
        let args = add_args(Path::new(r"C:\Program Files\Torrenta\torrenta.exe"));
        let expected = [
            "add",
            RUN_KEY,
            "/v",
            VALUE_NAME,
            "/t",
            "REG_SZ",
            "/d",
            r#""C:\Program Files\Torrenta\torrenta.exe""#,
            "/f",
        ]
        .map(String::from);
        assert_eq!(args, expected, "неверные аргументы reg.exe add");
    }

    #[test]
    fn delete_args_remove_value_without_prompt() {
        let args = delete_args();
        let expected = ["delete", RUN_KEY, "/v", VALUE_NAME, "/f"].map(String::from);
        assert_eq!(args, expected, "неверные аргументы reg.exe delete");
    }

    #[cfg(not(windows))]
    #[test]
    fn apply_is_noop_outside_windows() {
        assert!(apply(true).is_ok());
        assert!(apply(false).is_ok());
    }

    #[cfg(windows)]
    fn probe_exists() -> Option<String> {
        query_value(PROBE_VALUE).expect("чтение тестового значения реестра")
    }

    /// Убирает тестовое значение из реестра, даже если тест упал посередине.
    #[cfg(windows)]
    struct ProbeGuard;

    #[cfg(windows)]
    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            if let Err(err) = apply_named(PROBE_VALUE, false) {
                eprintln!("[torrenta] не удалось убрать тестовое значение из реестра: {err}");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn registry_roundtrip_uses_test_value() {
        let _guard = ProbeGuard;
        // Начинаем с чистого состояния и проверяем, что выключение идемпотентно.
        apply_named(PROBE_VALUE, false).expect("подготовка: выключение не должно падать");
        assert_eq!(
            probe_exists(),
            None,
            "тестовое значение должно отсутствовать"
        );

        // Настоящую запись приложения тест не меняет — запомним её.
        let real_before = query_value(VALUE_NAME).expect("чтение настоящего значения");

        apply_named(PROBE_VALUE, true).expect("включение автозапуска");
        let exe = std::env::current_exe().expect("путь к тестовому exe");
        assert_eq!(
            probe_exists(),
            Some(quote_exe(&exe)),
            "в реестр должен попасть путь к текущему exe"
        );

        // Повторное включение идемпотентно: `reg.exe add /f` перезаписывает значение.
        apply_named(PROBE_VALUE, true).expect("повторное включение не должно падать");
        assert_eq!(
            probe_exists(),
            Some(quote_exe(&exe)),
            "значение не должно задвоиться"
        );

        apply_named(PROBE_VALUE, false).expect("выключение автозапуска");
        assert_eq!(probe_exists(), None, "значение должно исчезнуть из реестра");

        // Повторное выключение: отсутствие значения не является ошибкой.
        apply_named(PROBE_VALUE, false).expect("повторное выключение не должно падать");

        let real_after = query_value(VALUE_NAME).expect("повторное чтение настоящего значения");
        assert_eq!(
            real_after, real_before,
            "тест не должен менять настоящую запись автозапуска"
        );
    }

    #[cfg(windows)]
    #[test]
    fn parse_query_output_extracts_data() {
        let output = "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    \
Torrenta__test    REG_SZ    \"C:\\Program Files\\Torrenta\\torrenta.exe\"\r\n\r\n";
        assert_eq!(
            parse_query_output(output, "Torrenta__test"),
            Some(r#""C:\Program Files\Torrenta\torrenta.exe""#.to_string())
        );
        assert_eq!(parse_query_output(output, "Torrenta"), None);
        assert_eq!(parse_query_output("", "Torrenta__test"), None);
    }
}
