import { h } from "../lib/dom";
import { icon } from "../lib/icons";

/**
 * Собственный выпадающий список в стиле меню: кнопка + всплывающий список.
 *
 * Нативный `<select>` нельзя оформить под палитру приложения (в WebView2 он
 * рисуется системно), поэтому здесь та же раскладка, что у контекстного меню.
 * Фокус остаётся на кнопке, активный пункт помечается `aria-activedescendant` —
 * так ловушка фокуса модального окна не ломается.
 */

export interface SelectOption {
  value: string;
  label: string;
}

export interface SelectOptions {
  /** Подпись для `aria-label` кнопки. */
  label: string;
  options: SelectOption[];
  value: string;
  onChange(value: string): void;
  className?: string;
  /** Минимальная ширина всплывающего списка, px. */
  menuWidth?: number;
}

export interface SelectControl {
  element: HTMLElement;
  setValue(value: string): void;
  setOptions(options: SelectOption[], value?: string): void;
  close(): void;
  destroy(): void;
}

/** Открытый список — один на документ, как и меню. */
let activeSelect: (() => void) | null = null;
let sequence = 0;

export function closeActiveSelect(): boolean {
  const close = activeSelect;
  if (!close) {
    return false;
  }
  close();
  return true;
}

export function createSelect(options: SelectOptions): SelectControl {
  sequence += 1;
  const id = `select-${sequence}`;
  let list = options.options.slice();
  let current = options.value;
  let open = false;
  let activeIndex = -1;
  let menu: HTMLElement | null = null;
  let items: HTMLElement[] = [];

  const valueEl = h("span", { class: "select__value" });
  const button = h("button", {
    class: "select__button",
    attrs: {
      type: "button",
      "aria-haspopup": "listbox",
      "aria-expanded": "false",
      "aria-label": options.label,
      title: options.label,
    },
  });
  button.append(valueEl, h("span", { class: "select__chevron" }, icon("chevronDown", 14)));

  const element = h(
    "div",
    { class: `select${options.className ? ` ${options.className}` : ""}` },
    button,
  );

  function labelOf(value: string): string {
    const found = list.find((option) => option.value === value);
    return found ? found.label : (list[0]?.label ?? "");
  }

  function syncButton(): void {
    const label = labelOf(current);
    valueEl.textContent = label;
    button.setAttribute("aria-label", `${options.label}: ${label}`);
  }

  function renderMenu(): void {
    if (!menu) {
      return;
    }
    const nodes: HTMLElement[] = [];
    items = [];
    list.forEach((option, index) => {
      const selected = option.value === current;
      const check = h("span", { class: "select__check" });
      if (selected) {
        check.append(icon("check", 13));
      }
      const item = h(
        "div",
        {
          class: `select__option${selected ? " is-selected" : ""}`,
          attrs: {
            role: "option",
            id: `${id}-opt-${index}`,
            "aria-selected": selected ? "true" : "false",
          },
        },
        h("span", { class: "select__option-label", text: option.label }),
        check,
      );
      item.addEventListener("mouseenter", () => setActive(index));
      nodes.push(item);
      items.push(item);
    });
    menu.replaceChildren(...nodes);
    if (activeIndex >= items.length) {
      activeIndex = items.length - 1;
    }
    setActive(activeIndex);
  }

  function setActive(index: number): void {
    if (index === activeIndex && items[index]?.classList.contains("is-active")) {
      return;
    }
    activeIndex = index;
    items.forEach((item, position) => {
      item.classList.toggle("is-active", position === index);
    });
    if (index >= 0 && items[index]) {
      button.setAttribute("aria-activedescendant", `${id}-opt-${index}`);
      if (menu) {
        const item = items[index];
        const top = item.offsetTop;
        if (top < menu.scrollTop) {
          menu.scrollTop = top;
        } else if (top + item.offsetHeight > menu.scrollTop + menu.clientHeight) {
          menu.scrollTop = top + item.offsetHeight - menu.clientHeight;
        }
      }
    } else {
      button.removeAttribute("aria-activedescendant");
    }
  }

  function positionMenu(): void {
    if (!menu) {
      return;
    }
    const rect = button.getBoundingClientRect();
    menu.style.minWidth = `${Math.round(Math.max(rect.width, options.menuWidth ?? 0, 148))}px`;
    menu.style.left = "0px";
    menu.style.top = "0px";
    const menuRect = menu.getBoundingClientRect();
    let left = rect.left;
    if (left + menuRect.width > window.innerWidth - 8) {
      left = Math.max(8, rect.right - menuRect.width);
    }
    let top = rect.bottom + 4;
    if (top + menuRect.height > window.innerHeight - 8) {
      top = Math.max(8, rect.top - menuRect.height - 4);
    }
    menu.style.left = `${Math.round(left)}px`;
    menu.style.top = `${Math.round(top)}px`;
  }

  function onOutside(event: MouseEvent): void {
    const target = event.target;
    if (!(target instanceof Node)) {
      return;
    }
    if (menu?.contains(target) || element.contains(target)) {
      return;
    }
    close();
  }

  function onScroll(event: Event): void {
    const target = event.target;
    if (target instanceof Node && menu?.contains(target)) {
      return;
    }
    close();
  }

  function unbindDocument(): void {
    document.removeEventListener("mousedown", onOutside, true);
    window.removeEventListener("resize", close);
    window.removeEventListener("scroll", onScroll, true);
  }

  function openMenu(): void {
    if (open) {
      return;
    }
    closeActiveSelect();
    if (!menu) {
      menu = h("div", {
        class: "select__menu",
        attrs: { role: "listbox", id: `${id}-menu`, "aria-label": options.label },
        hidden: true,
      });
      menu.addEventListener("mousedown", (event) => event.preventDefault());
      menu.addEventListener("click", (event) => {
        const target = event.target;
        const node = target instanceof Element ? target.closest<HTMLElement>(".select__option") : null;
        const index = node ? items.indexOf(node) : -1;
        if (index >= 0) {
          choose(index);
        }
      });
      document.body.append(menu);
    }
    open = true;
    activeIndex = Math.max(0, list.findIndex((option) => option.value === current));
    renderMenu();
    menu.hidden = false;
    element.classList.add("is-open");
    button.setAttribute("aria-expanded", "true");
    positionMenu();
    activeSelect = close;
    document.addEventListener("mousedown", onOutside, true);
    window.addEventListener("resize", close);
    window.addEventListener("scroll", onScroll, true);
    button.focus();
  }

  function choose(index: number): void {
    const option = list[index];
    if (!option) {
      return;
    }
    current = option.value;
    syncButton();
    close();
    // (1.7) Сообщаем о выборе даже совпадающего с текущим пункта: панель
    // сортировки и списки с подстановкой первого доступного значения иначе
    // теряли клик — смены значения нет, значит и `onChange` не придёт.
    options.onChange(option.value);
  }

  function close(): void {
    if (!open) {
      return;
    }
    open = false;
    if (activeSelect === close) {
      activeSelect = null;
    }
    if (menu) {
      menu.hidden = true;
    }
    element.classList.remove("is-open");
    button.setAttribute("aria-expanded", "false");
    button.removeAttribute("aria-activedescendant");
    unbindDocument();
    if (element.isConnected) {
      button.focus();
    }
  }

  function onKeyDown(event: KeyboardEvent): void {
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp": {
        event.preventDefault();
        if (!open) {
          openMenu();
          return;
        }
        const delta = event.key === "ArrowDown" ? 1 : -1;
        const count = items.length;
        if (count === 0) {
          return;
        }
        const next = (Math.min(Math.max(activeIndex, 0), count - 1) + delta + count) % count;
        setActive(next);
        return;
      }
      case "Home":
      case "End": {
        if (!open) {
          return;
        }
        event.preventDefault();
        setActive(event.key === "Home" ? 0 : items.length - 1);
        return;
      }
      case "Enter":
      case " ": {
        event.preventDefault();
        if (!open) {
          openMenu();
          return;
        }
        choose(activeIndex);
        return;
      }
      case "Escape": {
        if (!open) {
          return;
        }
        event.preventDefault();
        event.stopPropagation();
        close();
        return;
      }
      case "Tab": {
        close();
        return;
      }
      default:
        return;
    }
  }

  button.addEventListener("click", () => {
    if (open) {
      close();
    } else {
      openMenu();
    }
  });
  button.addEventListener("keydown", onKeyDown);

  syncButton();

  return {
    element,
    setValue(value: string) {
      current = value;
      syncButton();
      if (open) {
        renderMenu();
      }
    },
    setOptions(next: SelectOption[], value?: string) {
      list = next.slice();
      if (value !== undefined) {
        current = value;
      } else if (!list.some((option) => option.value === current)) {
        current = list[0]?.value ?? "";
      }
      syncButton();
      if (open) {
        renderMenu();
      }
    },
    close,
    destroy() {
      close();
      menu?.remove();
      menu = null;
      items = [];
    },
  };
}
