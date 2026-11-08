import { h, setAttr } from "../lib/dom";
import { icon, type IconName } from "../lib/icons";

/**
 * Всплывающее меню: контекстное (в точке клика) и подменю.
 *
 * Один экземпляр на документ. Закрывается по Esc, клику вне и скроллу.
 * Пункты с `children` открывают подменю по наведению и по ArrowRight,
 * пункты с `checked` рисуются как `menuitemradio` с галочкой.
 */

export interface MenuItem {
  label: string;
  icon?: IconName;
  danger?: boolean;
  separatorBefore?: boolean;
  checked?: boolean;
  disabled?: boolean;
  shortcut?: string;
  children?: MenuItem[];
  onSelect?(): void;
}

export interface MenuAnchor {
  x: number;
  y: number;
  /** Куда вернуть фокус после закрытия (обычно строка списка). */
  restore?: HTMLElement;
}

export type MenuTarget = HTMLElement | MenuAnchor;

export interface MenuHandle {
  element: HTMLElement;
  close(restoreFocus?: boolean): void;
}

interface SubmenuEntry {
  panel: HTMLElement;
  trigger: HTMLElement;
  wrap: HTMLElement;
}

let active: { root: HTMLElement; close(restoreFocus?: boolean): void } | null = null;

/** Закрыть открытое меню. Возвращает `true`, если что-то закрыли. */
export function closeMenu(): boolean {
  if (!active) {
    return false;
  }
  active.close(false);
  return true;
}

function focusablesOf(panel: HTMLElement): HTMLElement[] {
  return Array.from(panel.querySelectorAll<HTMLElement>("button.menu__item")).filter(
    (node) => !node.hasAttribute("disabled"),
  );
}

export function openMenu(target: MenuTarget, items: MenuItem[]): MenuHandle {
  closeMenu();

  const anchorElement = target instanceof HTMLElement ? target : null;
  const anchorPoint = target instanceof HTMLElement ? null : target;
  const restore = anchorElement ?? anchorPoint?.restore ?? null;

  const root = h("div", { class: "menu", attrs: { role: "menu", tabindex: "-1" } });
  const submenus: SubmenuEntry[] = [];

  function closeSubmenus(from = 0): void {
    while (submenus.length > from) {
      const entry = submenus.pop();
      if (!entry) {
        return;
      }
      entry.panel.remove();
      entry.wrap.classList.remove("is-open");
      setAttr(entry.trigger, "aria-expanded", "false");
    }
  }

  function openSubmenu(wrap: HTMLElement, trigger: HTMLElement, children: MenuItem[]): HTMLElement {
    const existing = submenus.find((entry) => entry.wrap === wrap);
    if (existing) {
      return existing.panel;
    }
    closeSubmenus(0);
    const panel = buildPanel(children, submenus.length + 1);
    document.body.append(panel);
    setAttr(trigger, "aria-expanded", "true");
    wrap.classList.add("is-open");
    submenus.push({ panel, trigger, wrap });

    const box = trigger.getBoundingClientRect();
    panel.style.left = "0px";
    panel.style.top = "0px";
    const size = panel.getBoundingClientRect();
    let left = box.right + 2;
    if (left + size.width > window.innerWidth - 8) {
      left = Math.max(8, box.left - size.width - 2);
    }
    let top = box.top - 5;
    if (top + size.height > window.innerHeight - 8) {
      top = Math.max(8, window.innerHeight - size.height - 8);
    }
    panel.style.left = `${Math.round(left)}px`;
    panel.style.top = `${Math.round(top)}px`;
    return panel;
  }

  /** Дети групп: нужны клавиатуре (ArrowRight), когда подменю ещё не открыто. */
  const groupChildren = new WeakMap<HTMLElement, MenuItem[]>();

  function groupItems(group: HTMLElement): MenuItem[] {
    return groupChildren.get(group) ?? [];
  }

  function buildItem(item: MenuItem): HTMLElement {
    const children = item.children?.filter(Boolean) ?? [];
    const hasChildren = children.length > 0;
    const radio = item.checked !== undefined && !hasChildren;
    const attrs: Record<string, string> = {
      type: "button",
      role: radio ? "menuitemradio" : "menuitem",
      tabindex: "-1",
    };
    if (radio) {
      attrs["aria-checked"] = item.checked ? "true" : "false";
    }
    const button = h(
      "button",
      {
        class: `menu__item${item.danger ? " menu__item--danger" : ""}${item.disabled ? " is-disabled" : ""}`,
        attrs,
      },
      ...(radio
        ? [h("span", { class: "menu__icon menu__icon--check" }, item.checked ? icon("check", 14) : h("i"))]
        : item.icon
          ? [h("span", { class: "menu__icon" }, icon(item.icon, 15))]
          : []),
      h("span", { class: "menu__label", text: item.label }),
      ...(item.shortcut ? [h("span", { class: "menu__shortcut", text: item.shortcut })] : []),
      ...(hasChildren ? [h("span", { class: "menu__chevron" }, icon("chevronRight", 14))] : []),
    );
    if (item.disabled) {
      button.disabled = true;
      setAttr(button, "aria-disabled", "true");
    }

    if (hasChildren) {
      setAttr(button, "aria-haspopup", "menu");
      setAttr(button, "aria-expanded", "false");
      const wrap = h("div", { class: "menu__group" }, button);
      button.addEventListener("mouseenter", () => {
        if (!button.disabled) {
          openSubmenu(wrap, button, children);
        }
      });
      button.addEventListener("click", (event) => {
        event.stopPropagation();
        if (button.disabled) {
          return;
        }
        const panel = openSubmenu(wrap, button, children);
        focusablesOf(panel)[0]?.focus();
      });
      groupChildren.set(wrap, children);
      return wrap;
    }

    button.addEventListener("mouseenter", () => {
      closeSubmenus(0);
    });
    button.addEventListener("click", () => {
      if (button.disabled) {
        return;
      }
      rootClose(false);
      item.onSelect?.();
    });
    return button;
  }

  function buildPanel(list: MenuItem[], depth: number): HTMLElement {
    const panel = h("div", {
      class: `menu${depth > 0 ? " menu--sub" : ""}`,
      attrs: { role: "menu" },
    });
    for (const item of list) {
      if (item.separatorBefore) {
        panel.append(h("div", { class: "menu__sep", attrs: { role: "separator" } }));
      }
      panel.append(buildItem(item));
    }
    panel.addEventListener("keydown", (event) => onPanelKey(event, panel, depth));
    return panel;
  }

  function onPanelKey(event: KeyboardEvent, panel: HTMLElement, depth: number): void {
    const nodes = focusablesOf(panel);
    const currentIndex = nodes.indexOf(document.activeElement as HTMLElement);
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp": {
        event.preventDefault();
        if (nodes.length === 0) {
          return;
        }
        const delta = event.key === "ArrowDown" ? 1 : -1;
        const from = currentIndex < 0 ? (delta > 0 ? -1 : 0) : currentIndex;
        const next = (from + delta + nodes.length) % nodes.length;
        nodes[next]?.focus();
        return;
      }
      case "Home":
      case "End": {
        event.preventDefault();
        if (nodes.length > 0) {
          nodes[event.key === "Home" ? 0 : nodes.length - 1]?.focus();
        }
        return;
      }
      case "ArrowRight": {
        const node = nodes[currentIndex];
        const group = node?.closest<HTMLElement>(".menu__group");
        if (node && group) {
          event.preventDefault();
          const childPanel = openSubmenu(group, node, groupItems(group));
          focusablesOf(childPanel)[0]?.focus();
        }
        return;
      }
      case "ArrowLeft": {
        if (depth > 0) {
          event.preventDefault();
          const entry = submenus[depth - 1];
          closeSubmenus(depth - 1);
          entry?.trigger.focus();
        }
        return;
      }
      case "Escape": {
        event.preventDefault();
        event.stopPropagation();
        if (depth > 0) {
          const entry = submenus[depth - 1];
          closeSubmenus(depth - 1);
          entry?.trigger.focus();
          return;
        }
        rootClose(true);
        return;
      }
      case "Tab": {
        rootClose(true);
        return;
      }
      default:
        return;
    }
  }

  function inside(target: Node | null): boolean {
    if (!target) {
      return false;
    }
    if (root.contains(target)) {
      return true;
    }
    return submenus.some((entry) => entry.panel.contains(target));
  }

  function onOutside(event: MouseEvent): void {
    const target = event.target;
    if (target instanceof Node && inside(target)) {
      return;
    }
    rootClose(false);
  }

  function onResize(): void {
    rootClose(false);
  }

  function onScroll(event: Event): void {
    const target = event.target;
    if (target instanceof Node && inside(target)) {
      return;
    }
    rootClose(false);
  }

  function onDocumentKeyDown(event: KeyboardEvent): void {
    if (event.key === "Escape" && !event.defaultPrevented) {
      event.preventDefault();
      rootClose(true);
    }
  }

  function rootClose(restoreFocus: boolean): void {
    if (!root.isConnected) {
      return;
    }
    closeSubmenus(0);
    root.remove();
    document.removeEventListener("mousedown", onOutside, true);
    document.removeEventListener("keydown", onDocumentKeyDown);
    window.removeEventListener("resize", onResize);
    window.removeEventListener("scroll", onScroll, true);
    if (active?.root === root) {
      active = null;
    }
    if (restoreFocus && restore?.isConnected) {
      restore.focus();
    }
  }

  root.append(...items.map((item) => buildItem(item)));
  root.addEventListener("keydown", (event) => onPanelKey(event, root, 0));

  document.body.append(root);
  root.style.left = "0px";
  root.style.top = "0px";
  const size = root.getBoundingClientRect();
  let left: number;
  let top: number;
  if (anchorElement) {
    const box = anchorElement.getBoundingClientRect();
    left = box.right - size.width;
    top = box.bottom + 4;
    if (top + size.height > window.innerHeight - 8) {
      top = Math.max(8, box.top - size.height - 4);
    }
  } else if (anchorPoint) {
    left = anchorPoint.x;
    top = anchorPoint.y;
  } else {
    left = 0;
    top = 0;
  }
  left = Math.min(Math.max(8, left), Math.max(8, window.innerWidth - size.width - 8));
  top = Math.min(Math.max(8, top), Math.max(8, window.innerHeight - size.height - 8));
  root.style.left = `${Math.round(left)}px`;
  root.style.top = `${Math.round(top)}px`;

  active = { root, close: rootClose };
  document.addEventListener("mousedown", onOutside, true);
  document.addEventListener("keydown", onDocumentKeyDown);
  window.addEventListener("resize", onResize);
  window.addEventListener("scroll", onScroll, true);

  focusablesOf(root)[0]?.focus();

  return {
    element: root,
    close(restoreFocus = true) {
      rootClose(restoreFocus);
    },
  };
}
