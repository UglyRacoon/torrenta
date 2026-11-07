import { h } from "../lib/dom";
import { icon, type IconName } from "../lib/icons";

export interface MenuItem {
  label: string;
  icon?: IconName;
  danger?: boolean;
  separatorBefore?: boolean;
  onSelect(): void;
}

export interface MenuHandle {
  close(restoreFocus?: boolean): void;
}

let active: { menu: HTMLElement; dispose(restoreFocus: boolean): void } | null = null;

export function closeMenu(): void {
  active?.dispose(false);
  active = null;
}

/**
 * Контекстное меню у якоря: кнопка «…» в строке или контекстное меню правой кнопкой.
 * Клавиатура: ↑/↓/Home/End, Esc, Tab. Фокус возвращается на якорь.
 */
export function openMenu(anchor: HTMLElement, items: MenuItem[]): MenuHandle {
  closeMenu();

  const menu = h("div", { class: "menu", attrs: { role: "menu" } });
  const buttons: HTMLButtonElement[] = [];

  for (const item of items) {
    if (item.separatorBefore) {
      menu.append(h("div", { class: "menu__separator", attrs: { role: "separator" } }));
    }
    const button = h("button", {
      class: `menu__item${item.danger ? " menu__item--danger" : ""}`,
      attrs: { type: "button", role: "menuitem", tabindex: "-1" },
    });
    if (item.icon) {
      button.append(icon(item.icon, 16));
    }
    button.append(h("span", { text: item.label }));
    button.addEventListener("click", () => {
      dispose(true);
      item.onSelect();
    });
    menu.append(button);
    buttons.push(button);
  }

  document.body.append(menu);

  const anchorRect = anchor.getBoundingClientRect();
  const menuRect = menu.getBoundingClientRect();
  const margin = 8;
  let left = anchorRect.right - menuRect.width;
  left = Math.max(margin, Math.min(left, window.innerWidth - menuRect.width - margin));
  let top = anchorRect.bottom + 6;
  if (top + menuRect.height > window.innerHeight - margin) {
    top = Math.max(margin, anchorRect.top - menuRect.height - 6);
  }
  menu.style.left = `${Math.round(left)}px`;
  menu.style.top = `${Math.round(top)}px`;

  function focusAt(index: number): void {
    const count = buttons.length;
    if (count === 0) {
      return;
    }
    const next = ((index % count) + count) % count;
    buttons[next]?.focus();
  }

  function onMenuKeyDown(event: KeyboardEvent): void {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      dispose(true);
      return;
    }
    if (event.key === "Tab") {
      event.preventDefault();
      dispose(true);
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp" || event.key === "Home" || event.key === "End") {
      event.preventDefault();
      const current = buttons.indexOf(document.activeElement as HTMLButtonElement);
      if (event.key === "Home") {
        focusAt(0);
      } else if (event.key === "End") {
        focusAt(buttons.length - 1);
      } else {
        focusAt(current + (event.key === "ArrowDown" ? 1 : -1));
      }
    }
  }

  function onDocumentPointerDown(event: MouseEvent): void {
    if (event.target instanceof Node && !menu.contains(event.target)) {
      dispose(false);
    }
  }

  function onWindowChange(): void {
    dispose(false);
  }

  function dispose(restoreFocus: boolean): void {
    if (active?.menu !== menu) {
      return;
    }
    active = null;
    document.removeEventListener("mousedown", onDocumentPointerDown, true);
    window.removeEventListener("resize", onWindowChange);
    window.removeEventListener("scroll", onWindowChange, true);
    menu.removeEventListener("keydown", onMenuKeyDown);
    menu.remove();
    if (restoreFocus && anchor.isConnected) {
      anchor.focus();
    }
  }

  document.addEventListener("mousedown", onDocumentPointerDown, true);
  window.addEventListener("resize", onWindowChange);
  window.addEventListener("scroll", onWindowChange, true);
  menu.addEventListener("keydown", onMenuKeyDown);

  // Фокус — на первый пункт; у меню есть tabindex у кнопок.
  buttons[0]?.focus();
  active = { menu, dispose };

  return {
    close: (restoreFocus = false) => dispose(restoreFocus),
  };
}
