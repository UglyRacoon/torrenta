//! Иконка Torrenta в системном трее (области уведомлений Windows).
//!
//! Трей нужен для двух сценариев: запуск «в трее» (окно скрыто до первого
//! открытия) и закрытие окна в трей вместо выхода. Иконка создаётся всегда,
//! при ошибке приложение продолжает работать — см. [`init`].
//!
//! Пункты меню:
//! * «Открыть Torrenta» — показать главное окно;
//! * «Выход» — завершить приложение.
//!
//! Левый клик по иконке тоже показывает окно (меню на левый клик не вешаем,
//! чтобы нельзя было случайно закрыть приложение одним нажатием).

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager as _};

/// Идентификатор иконки в трее.
pub const TRAY_ID: &str = "torrenta-tray";
/// Идентификатор пункта меню «Открыть Torrenta».
pub const MENU_SHOW: &str = "show";
/// Идентификатор пункта меню «Выход».
pub const MENU_QUIT: &str = "quit";
/// Подсказка при наведении на иконку.
pub const TOOLTIP: &str = "Torrenta";
/// Текст пункта меню «Открыть Torrenta».
pub const MENU_SHOW_TEXT: &str = "Открыть Torrenta";
/// Текст пункта меню «Выход».
pub const MENU_QUIT_TEXT: &str = "Выход";
/// Максимальная длина подсказки: Windows обрезает длинные строки сама, но мы
/// ограничиваем заранее, чтобы формат был предсказуемым.
pub const TOOLTIP_MAX_CHARS: usize = 120;
/// Префикс подсказки: имя приложения.
pub const TOOLTIP_PREFIX: &str = "Torrenta";
/// Подсказка, когда ничего не качается и не раздаётся.
pub const TOOLTIP_IDLE: &str = "Torrenta — нет активных загрузок";

/// Создаёт иконку в трее с меню и обработчиками.
///
/// # Errors
///
/// Возвращает ошибку Tauri, если система не дала создать иконку. Вызывающая
/// сторона должна только залогировать её: без трея приложение остаётся
/// работоспособным.
pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, MENU_SHOW, MENU_SHOW_TEXT, true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, MENU_QUIT, MENU_QUIT_TEXT, true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        // Меню открывается правым кликом: левый клик — «показать окно».
        .show_menu_on_left_click(false)
        .tooltip(TOOLTIP)
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_SHOW => show_main(app),
            MENU_QUIT => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });

    match app.default_window_icon().cloned() {
        Some(icon) => builder = builder.icon(icon),
        None => eprintln!("[torrenta] у приложения нет иконки: трей будет без картинки"),
    }

    // Иконка не удаляется после возврата: Tauri держит её в таблице ресурсов
    // приложения, поэтому `build` можно вызывать из `setup` и просто отбросить значение.
    builder.build(app)?;
    Ok(())
}

/// Меняет подсказку иконки в трее.
///
/// Ошибки не пробрасываются: трей — вспомогательный элемент, из-за подсказки
/// приложение не должно падать. Если иконки нет, вызов молча ничего не делает.
pub fn set_tooltip(app: &AppHandle, text: &str) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if let Err(err) = tray.set_tooltip(Some(text)) {
        eprintln!("[torrenta] не удалось обновить подсказку трея: {err}");
    }
}

/// Собирает текст подсказки трея из текущих цифр движка.
///
/// Вид при активных загрузках:
/// `Torrenta — ↓ 4,2 МБ/с · ↑ 512 КБ/с · 3 из 5`, где последняя пара чисел —
/// сколько торрентов активно из общего количества. Когда ничего не качается и
/// не раздаётся, подсказка одна и та же: [`TOOLTIP_IDLE`].
///
/// Скорости приходят в байтах в секунду (как в [`torrent_core::GlobalTotals`]),
/// единицы — двоичные (`КБ` = 1024 байта), разделитель дробной части — запятая,
/// как принято в русском интерфейсе. Результат обрезается до
/// [`TOOLTIP_MAX_CHARS`] символов.
pub fn tooltip_text(torrents: u32, active: u32, down: f64, up: f64) -> String {
    if active == 0 || torrents == 0 {
        return TOOLTIP_IDLE.to_string();
    }
    let text = format!(
        "{TOOLTIP_PREFIX} — ↓ {} · ↑ {} · {active} из {torrents}",
        format_rate(down),
        format_rate(up)
    );
    truncate_chars(&text, TOOLTIP_MAX_CHARS)
}

/// Форматирует скорость в байтах в секунду по-русски: `512 КБ/с`, `4,2 МБ/с`.
pub fn format_rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "0 Б/с".to_string();
    }
    const UNITS: [(&str, f64); 4] = [
        ("ГБ/с", 1024.0 * 1024.0 * 1024.0),
        ("МБ/с", 1024.0 * 1024.0),
        ("КБ/с", 1024.0),
        ("Б/с", 1.0),
    ];
    for (unit, scale) in UNITS {
        let value = bytes_per_second / scale;
        if value >= 1.0 || scale == 1.0 {
            // Крупные и целые значения показываем целыми, мелкие — с одним
            // знаком: «512 КБ/с», «4,2 МБ/с», «0,3 КБ/с».
            let rounded = value.round();
            let text = if scale == 1.0 || value >= 100.0 || (value - rounded).abs() < 0.05 {
                format!("{rounded:.0}")
            } else {
                format!("{value:.1}")
            };
            return format!("{} {unit}", text.replace('.', ","));
        }
    }
    "0 Б/с".to_string()
}

/// Обрезает строку до `max` символов (не байт) без паники на юникоде.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut result: String = text.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

///
/// Показывает главное окно: разворачивает из свёрнутого состояния и даёт фокус.
///
/// Ошибки не пробрасываются: трей — вспомогательный элемент, из-за него
/// приложение не должно падать. Неудачи логируются, чтобы их было видно
/// в консоли при отладке.
pub fn show_main(app: &AppHandle) {
    let Some(window) = app.get_webview_window(crate::MAIN_WINDOW) else {
        eprintln!(
            "[torrenta] окно «{}» не найдено: трей не может его показать",
            crate::MAIN_WINDOW
        );
        return;
    };
    if let Err(err) = window.show() {
        eprintln!("[torrenta] не удалось показать окно из трея: {err}");
    }
    if let Err(err) = window.unminimize() {
        eprintln!("[torrenta] не удалось развернуть окно из трея: {err}");
    }
    if let Err(err) = window.set_focus() {
        eprintln!("[torrenta] не удалось передать фокус окну из трея: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_constants_are_distinct_and_filled() {
        assert_ne!(MENU_SHOW, MENU_QUIT, "идентификаторы пунктов меню совпали");
        assert_ne!(TRAY_ID, MENU_SHOW);
        assert_eq!(TOOLTIP, "Torrenta");
        assert!(!MENU_SHOW_TEXT.trim().is_empty());
        assert!(!MENU_QUIT_TEXT.trim().is_empty());
        assert_eq!(MENU_SHOW_TEXT, "Открыть Torrenta");
        assert_eq!(MENU_QUIT_TEXT, "Выход");
        assert_eq!(TOOLTIP_PREFIX, TOOLTIP);
        assert!(TOOLTIP_IDLE.starts_with(TOOLTIP));
        assert!(TOOLTIP_IDLE.chars().count() <= TOOLTIP_MAX_CHARS);
    }

    #[test]
    fn tooltip_shows_rates_and_active_count() {
        let down = 4.2 * 1024.0 * 1024.0;
        let up = 512.0 * 1024.0;
        assert_eq!(
            tooltip_text(5, 3, down, up),
            "Torrenta — ↓ 4,2 МБ/с · ↑ 512 КБ/с · 3 из 5"
        );
    }

    #[test]
    fn tooltip_is_idle_without_active_torrents() {
        assert_eq!(tooltip_text(5, 0, 1.0, 2.0), TOOLTIP_IDLE);
        assert_eq!(tooltip_text(0, 0, 0.0, 0.0), TOOLTIP_IDLE);
        assert_eq!(
            tooltip_text(1, 0, 0.0, 0.0),
            "Torrenta — нет активных загрузок"
        );
    }

    #[test]
    fn tooltip_never_exceeds_the_limit() {
        let text = tooltip_text(
            u32::MAX,
            u32::MAX,
            999.0 * 1024.0 * 1024.0,
            999.0 * 1024.0 * 1024.0,
        );
        assert!(
            text.chars().count() <= TOOLTIP_MAX_CHARS,
            "слишком длинная подсказка: {text}"
        );
        assert_eq!(truncate_chars("абвгд", 3), "аб…");
        assert_eq!(truncate_chars("абв", 3), "абв");
        assert_eq!(truncate_chars("", 0), "");
    }

    #[test]
    fn rates_are_formatted_in_russian_units() {
        assert_eq!(format_rate(0.0), "0 Б/с");
        assert_eq!(format_rate(-5.0), "0 Б/с");
        assert_eq!(format_rate(f64::NAN), "0 Б/с");
        assert_eq!(format_rate(512.0), "512 Б/с");
        assert_eq!(format_rate(512.0 * 1024.0), "512 КБ/с");
        assert_eq!(format_rate(1024.0), "1 КБ/с");
        assert_eq!(format_rate(4.2 * 1024.0 * 1024.0), "4,2 МБ/с");
        assert_eq!(format_rate(1024.0 * 1024.0 * 1024.0), "1 ГБ/с");
        assert_eq!(format_rate(300.0 * 1024.0 * 1024.0), "300 МБ/с");
    }
}
