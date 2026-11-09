import type { AppState } from "../state/store";
import type { FilterKey } from "../api/types";
import { activeCount, filterCounts } from "../state/view";
import { clamp, fmtBytes, fmtCount, fmtRate } from "../lib/format";
import { h, setAttr, setText } from "../lib/dom";
import { brandMark, icon, type IconName } from "../lib/icons";

const NAV: { key: FilterKey; label: string; icon: IconName }[] = [
  { key: "all", label: "Все", icon: "layers" },
  { key: "downloading", label: "Загрузки", icon: "download" },
  { key: "seeding", label: "Раздачи", icon: "upload" },
  { key: "paused", label: "Пауза", icon: "pause" },
  { key: "completed", label: "Завершено", icon: "check" },
];

/** Минимальная ширина — режим «только иконки» (см. 4.3). */
const MIN_WIDTH = 56;
const MAX_WIDTH = 360;
/** Ниже этой ширины подписи не помещаются — скрываем их. */
const COMPACT_BELOW = 136;
const WIDTH_KEY = "torrenta:sidebar-w";

export interface SidebarOptions {
  onFilter(filter: FilterKey): void;
  onToggleTheme(): void;
  onOpenSettings(): void;
  onToggleAll(): void;
  /** Полное завершение приложения (1.4): кнопка «Выход» в футере. */
  onQuit(): void;
}

export interface Sidebar {
  update(state: AppState): void;
}

function readStoredWidth(): number | null {
  try {
    const raw = window.localStorage.getItem(WIDTH_KEY);
    if (!raw) {
      return null;
    }
    const value = Math.round(Number.parseInt(raw, 10));
    return Number.isFinite(value) ? clamp(value, MIN_WIDTH, MAX_WIDTH) : null;
  } catch {
    return null;
  }
}

function storeWidth(width: number): void {
  try {
    window.localStorage.setItem(WIDTH_KEY, String(Math.round(width)));
  } catch {
    /* приватный режим — ширина просто не запомнится */
  }
}

export function createSidebar(mount: HTMLElement, options: SidebarOptions): Sidebar {
  const brand = h(
    "div",
    { class: "brand" },
    h("span", { class: "brand__mark", attrs: { title: "Torrenta" } }, brandMark(20)),
    h("span", { class: "brand__name", text: "Torrenta" }),
  );

  const nav = h("nav", { class: "nav", attrs: { "aria-label": "Фильтры торрентов" } });
  const navItems = new Map<FilterKey, { button: HTMLButtonElement; count: HTMLElement }>();

  for (const item of NAV) {
    const count = h("span", { class: "nav__count num", text: "0" });
    const label = h("span", { class: "nav__label", text: item.label });
    const button = h("button", {
      class: "nav__item",
      attrs: { type: "button", "data-filter": item.key, title: item.label },
    });
    button.append(h("span", { class: "nav__icon" }, icon(item.icon, 17)), label, count);
    button.addEventListener("click", () => options.onFilter(item.key));
    nav.append(button);
    navItems.set(item.key, { button, count });
  }

  const allButton = h("button", {
    class: "icon-btn icon-btn--sm",
    attrs: { type: "button" },
  });
  const speedHead = h(
    "div",
    { class: "speed__head" },
    h("span", { class: "section-label", text: "Скорость" }),
    allButton,
  );

  const downValue = h("b", { class: "speed__value num", text: "0 Б/с" });
  const upValue = h("b", { class: "speed__value num", text: "0 Б/с" });
  const downRow = h(
    "div",
    { class: "speed__row" },
    h("span", { class: "speed__icon" }, icon("arrowDown", 14)),
    h("span", { class: "speed__name", text: "Приём" }),
    downValue,
  );
  const upRow = h(
    "div",
    { class: "speed__row" },
    h("span", { class: "speed__icon speed__icon--up" }, icon("arrowUp", 14)),
    h("span", { class: "speed__name", text: "Отдача" }),
    upValue,
  );
  const sessionValue = h("span", { class: "num speed__session-value", text: "0 Б · 0 Б" });
  const session = h(
    "p",
    { class: "speed__session" },
    h("span", { class: "speed__session-key", text: "За сессию" }),
    sessionValue,
  );

  const speed = h(
    "section",
    { class: "speed", attrs: { "aria-label": "Суммарная скорость" } },
    speedHead,
    downRow,
    upRow,
    session,
  );

  const themeLabel = h("span", { class: "side-action__label", text: "Светлая тема" });
  const themeButton = h("button", {
    class: "side-action",
    attrs: { type: "button", title: "Переключить тему" },
  });
  const themeIconWrap = h("span", { class: "side-action__icon" }, icon("sun", 16));
  themeButton.append(themeIconWrap, themeLabel);

  const settingsButton = h("button", {
    class: "side-action",
    attrs: { type: "button", title: "Настройки (Ctrl+,)" },
  });
  settingsButton.append(
    h("span", { class: "side-action__icon" }, icon("settings", 16)),
    h("span", { class: "side-action__label", text: "Настройки" }),
  );

  // 1.4: выход из приложения. Цвета — только существующие токены (см. layout.css),
  // чтобы не расходиться с проверкой контраста.
  const quitButton = h("button", {
    class: "side-action side-action--quit",
    attrs: { type: "button", title: "Выход (завершить приложение)" },
  });
  quitButton.append(
    h("span", { class: "side-action__icon" }, icon("power", 16)),
    h("span", { class: "side-action__label", text: "Выход" }),
  );

  const footer = h("div", { class: "sidebar__footer" }, themeButton, settingsButton, quitButton);

  const resize = h("div", {
    class: "sidebar__resize",
    attrs: {
      role: "separator",
      "aria-orientation": "vertical",
      "aria-label": "Изменить ширину панели",
      title: "Потяните, чтобы изменить ширину · стрелки ←→",
      tabindex: "0",
      "aria-valuemin": String(MIN_WIDTH),
      "aria-valuemax": String(MAX_WIDTH),
    },
  });

  mount.append(brand, nav, speed, h("div", { class: "sidebar__spacer" }), footer, resize);

  themeButton.addEventListener("click", () => options.onToggleTheme());
  settingsButton.addEventListener("click", () => options.onOpenSettings());
  quitButton.addEventListener("click", () => options.onQuit());
  allButton.addEventListener("click", () => options.onToggleAll());

  let allMode: "pause" | "resume" | "idle" | null = null;
  let theme: string | null = null;
  let compact: boolean | null = null;
  let width = 0;

  function applyWidth(next: number, persist: boolean): void {
    width = Math.round(clamp(next, MIN_WIDTH, MAX_WIDTH));
    document.documentElement.style.setProperty("--sidebar-w", `${width}px`);
    setAttr(resize, "aria-valuenow", String(width));
    if (persist) {
      storeWidth(width);
    }
    syncCompact();
  }

  function syncCompact(): void {
    const isCompact = width < COMPACT_BELOW;
    if (compact === isCompact) {
      return;
    }
    compact = isCompact;
    mount.dataset.compact = isCompact ? "true" : "false";
    mount.classList.toggle("sidebar--compact", isCompact);
    for (const [key, item] of navItems) {
      const label = NAV.find((entry) => entry.key === key)?.label ?? key;
      setAttr(item.button, "aria-label", isCompact ? label : null);
      setAttr(item.button, "title", label);
    }
    setAttr(themeButton, "aria-label", isCompact ? "Переключить тему" : null);
    setAttr(settingsButton, "aria-label", isCompact ? "Настройки" : null);
    setAttr(quitButton, "aria-label", isCompact ? "Выход" : null);
    setAttr(allButton, "aria-label", allButton.dataset.label ?? "Пауза/возобновление");
  }

  // Начальная ширина: сохранённая или по содержимому (CSS max-content).
  const stored = readStoredWidth();
  if (stored !== null) {
    applyWidth(stored, false);
  } else {
    width = Math.round(mount.getBoundingClientRect().width) || MAX_WIDTH;
    setAttr(resize, "aria-valuenow", String(width));
    syncCompact();
  }

  const observer = new ResizeObserver((entries) => {
    const entry = entries[0];
    if (!entry) {
      return;
    }
    width = Math.round(entry.contentRect.width);
    setAttr(resize, "aria-valuenow", String(width));
    syncCompact();
  });
  observer.observe(mount);

  let dragStartX = 0;
  let dragStartWidth = 0;
  let dragging = false;

  function onPointerMove(event: PointerEvent): void {
    if (!dragging) {
      return;
    }
    applyWidth(dragStartWidth + (event.clientX - dragStartX), false);
  }

  function endDrag(event: PointerEvent): void {
    if (!dragging) {
      return;
    }
    dragging = false;
    document.documentElement.classList.remove("is-resizing");
    if (resize.hasPointerCapture(event.pointerId)) {
      resize.releasePointerCapture(event.pointerId);
    }
    storeWidth(width);
    resize.focus();
  }

  resize.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) {
      return;
    }
    event.preventDefault();
    dragging = true;
    dragStartX = event.clientX;
    dragStartWidth = mount.getBoundingClientRect().width;
    document.documentElement.classList.add("is-resizing");
    resize.setPointerCapture(event.pointerId);
  });
  resize.addEventListener("pointermove", onPointerMove);
  resize.addEventListener("pointerup", endDrag);
  resize.addEventListener("pointercancel", endDrag);

  resize.addEventListener("keydown", (event) => {
    const step = event.shiftKey ? 32 : 16;
    switch (event.key) {
      case "ArrowLeft":
        event.preventDefault();
        applyWidth(width - step, true);
        return;
      case "ArrowRight":
        event.preventDefault();
        applyWidth(width + step, true);
        return;
      case "Home":
        event.preventDefault();
        applyWidth(MIN_WIDTH, true);
        return;
      case "End":
        event.preventDefault();
        applyWidth(MAX_WIDTH, true);
        return;
      default:
        return;
    }
  });

  function setAllMode(next: "pause" | "resume" | "idle"): void {
    if (allMode === next) {
      return;
    }
    allMode = next;
    allButton.replaceChildren(icon(next === "resume" ? "play" : "pause", 15));
    allButton.disabled = next === "idle";
    const label =
      next === "resume"
        ? "Возобновить все"
        : next === "pause"
          ? "Приостановить все"
          : "Нет активных загрузок";
    allButton.dataset.label = label;
    setAttr(allButton, "aria-label", label);
    setAttr(allButton, "title", label);
  }

  function setTheme(next: string): void {
    if (theme === next) {
      return;
    }
    theme = next;
    const dark = next === "dark";
    themeIconWrap.replaceChildren(icon(dark ? "sun" : "moon", 16));
    setText(themeLabel, dark ? "Светлая тема" : "Тёмная тема");
  }

  return {
    update(state) {
      const payload = state.payload;
      const torrents = payload?.torrents ?? [];
      const counts = filterCounts(torrents);

      for (const [key, item] of navItems) {
        setText(item.count, fmtCount(counts[key]));
        item.count.hidden = counts[key] === 0;
        setAttr(item.button, "aria-current", state.filter === key ? "page" : null);
      }

      const totals = payload?.totals;
      setText(downValue, totals ? fmtRate(totals.down_rate) : "0 Б/с");
      setText(upValue, totals ? fmtRate(totals.up_rate) : "0 Б/с");
      setText(
        sessionValue,
        totals
          ? `↓ ${fmtBytes(totals.downloaded_bytes)} · ↑ ${fmtBytes(totals.uploaded_bytes)}`
          : "↓ 0 Б · ↑ 0 Б",
      );

      const anyActive = activeCount(torrents) > 0;
      const anyPaused = torrents.some((torrent) => torrent.state === "paused");
      setAllMode(anyActive ? "pause" : anyPaused ? "resume" : "idle");
      setTheme(state.theme);
    },
  };
}
