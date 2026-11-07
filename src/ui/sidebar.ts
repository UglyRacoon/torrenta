import type { AppState } from "../state/store";
import type { FilterKey } from "../api/types";
import { activeCount, filterCounts } from "../state/view";
import { fmtBytes, fmtCount, fmtRate } from "../lib/format";
import { h, setAttr, setText } from "../lib/dom";
import { brandMark, icon, type IconName } from "../lib/icons";

const NAV: { key: FilterKey; label: string; icon: IconName }[] = [
  { key: "all", label: "Все", icon: "layers" },
  { key: "downloading", label: "Загрузки", icon: "download" },
  { key: "seeding", label: "Раздачи", icon: "upload" },
  { key: "paused", label: "Пауза", icon: "pause" },
  { key: "completed", label: "Завершено", icon: "check" },
];

export interface SidebarOptions {
  onFilter(filter: FilterKey): void;
  onToggleTheme(): void;
  onOpenSettings(): void;
  onToggleAll(): void;
}

export interface Sidebar {
  update(state: AppState): void;
}

export function createSidebar(mount: HTMLElement, options: SidebarOptions): Sidebar {
  const brand = h(
    "div",
    { class: "brand" },
    h("span", { class: "brand__mark" }, brandMark(24)),
    h("span", { class: "brand__name", text: "Torrenta" }),
  );

  const nav = h("nav", { class: "nav", attrs: { "aria-label": "Фильтры торрентов" } });
  const navItems = new Map<FilterKey, { button: HTMLButtonElement; count: HTMLElement }>();

  for (const item of NAV) {
    const count = h("span", { class: "nav__count num", text: "0" });
    const label = h("span", { class: "nav__label", text: item.label });
    const button = h("button", {
      class: "nav__item",
      attrs: { type: "button", "data-filter": item.key },
    });
    button.append(icon(item.icon, 17), label, count);
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

  const themeLabel = h("span", { text: "Светлая тема" });
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
    h("span", { text: "Настройки" }),
  );

  const footer = h("div", { class: "sidebar__footer" }, themeButton, settingsButton);

  mount.append(brand, nav, speed, h("div", { class: "sidebar__spacer" }), footer);

  themeButton.addEventListener("click", () => options.onToggleTheme());
  settingsButton.addEventListener("click", () => options.onOpenSettings());
  allButton.addEventListener("click", () => options.onToggleAll());

  let allMode: "pause" | "resume" | "idle" | null = null;
  let theme: string | null = null;

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
