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
    }
}
