import type { SortKey, TorrentSnapshot, TorrentState } from "../api/types";
import type { AppState } from "../state/store";
import { FILTER_LABELS, activeCount, visibleTorrents } from "../state/view";
import {
  fmtBytes,
  fmtCount,
  fmtDate,
  fmtEta,
  fmtPercent,
  fmtRate,
  fmtTorrents,
  fmtActive,
} from "../lib/format";
import { h, setAttr, setScaleX, setText } from "../lib/dom";
import { icon } from "../lib/icons";
import { createSelect } from "./select";

const BADGES: Record<TorrentState, { label: string; cls: string }> = {
  queued: { label: "В очереди", cls: "queued" },
  metadata: { label: "Метаданные", cls: "metadata" },
  downloading: { label: "Загрузка", cls: "downloading" },
  seeding: { label: "Раздача", cls: "seeding" },
  checking: { label: "Проверка", cls: "checking" },
  paused: { label: "Пауза", cls: "paused" },
  completed: { label: "Завершено", cls: "completed" },
  error: { label: "Ошибка", cls: "error" },
};

const SORT_OPTIONS: { value: SortKey; label: string }[] = [
  { value: "added", label: "Сначала новые" },
  { value: "name", label: "По имени" },
  { value: "progress", label: "По прогрессу" },
  { value: "speed", label: "По скорости" },
  { value: "size", label: "По размеру" },
];

type ActionKind = "pause" | "resume" | "retry";

/** Точка открытия контекстного меню (кнопка «…» или курсор правого клика). */
export interface MenuPoint {
  x: number;
  y: number;
  restore?: HTMLElement;
}

export type ContextAnchor = HTMLElement | MenuPoint;

interface RowView {
  el: HTMLElement;
  name: HTMLElement;
  metaMain: HTMLElement;
  metaStatus: HTMLElement;
  badge: HTMLElement;
  bar: HTMLElement;
  fill: HTMLElement;
  percent: HTMLElement;
  down: HTMLElement;
  up: HTMLElement;
  left: HTMLElement;
  pause: HTMLButtonElement;
  scale: { value: number };
  badgeState: TorrentState | null;
  action: ActionKind | null;
  actionState: TorrentState | null;
}

export interface ListOptions {
  /** Клик по строке — только выделение, панель подробностей не открывается. */
  onSelect(id: number): void;
  /** Двойной щелчок — открыть панель подробностей. */
  onOpen(id: number): void;
  onPauseToggle(id: number): void;
  onContextMenu(id: number, anchor: ContextAnchor): void;
  onAdd(): void;
  onQuery(query: string): void;
  onSort(sort: SortKey): void;
  onClearFilters(): void;
  onRetry(): void;
}

export interface TorrentList {
  update(state: AppState): void;
  focusSearch(): void;
  focusSelected(): void;
  element: HTMLElement;
}

function actionFor(state: TorrentState): ActionKind | null {
  switch (state) {
    case "paused":
    case "completed":
      return "resume";
    case "error":
      return "retry";
    case "downloading":
    case "seeding":
    case "metadata":
    case "queued":
      return "pause";
    case "checking":
      // Во время перепроверки движок не принимает паузу.
      return null;
    default:
      return null;
  }
}

function actionLabel(kind: ActionKind, state: TorrentState): string {
  if (kind === "retry") {
    return "Повторить";
  }
  if (kind === "resume") {
    return state === "completed" ? "Раздать" : "Продолжить";
  }
  return "Пауза";
}

function metaMainText(torrent: TorrentSnapshot): string {
  const size = torrent.total_bytes > 0 ? fmtBytes(torrent.total_bytes) : "Размер неизвестен";
  const peers = `${fmtCount(torrent.peers_connected)}/${fmtCount(torrent.peers_known)}`;
  return `${size} · пиры ${peers}`;
}

function metaStatusText(torrent: TorrentSnapshot): string {
  switch (torrent.state) {
    case "downloading":
      // Все файлы исключены приоритетом «не скачивать»: качать нечего.
      if (torrent.wanted_bytes === 0 && torrent.files.length > 0) {
        return "файлы не выбраны";
      }
      return torrent.eta_seconds === null ? "ETA неизвестно" : `осталось ${fmtEta(torrent.eta_seconds)}`;
    case "seeding":
      return `отдано ${fmtBytes(torrent.uploaded_bytes)}`;
    case "completed":
      return `завершён ${fmtDate(torrent.added_at)}`;
    case "queued":
      return "ожидает запуска";
    case "metadata":
      return "ожидание метаданных";
    case "checking":
      return `проверка данных · ${fmtPercent(torrent.progress)}`;
    case "paused":
      return "приостановлен";
    case "error":
      return torrent.error ?? "неизвестная ошибка";
    default:
      return "";
  }
}

function statValue(torrent: TorrentSnapshot, kind: "down" | "up" | "left"): string {
  const downloading = torrent.state === "downloading";
  if (kind === "down") {
    return downloading ? fmtRate(torrent.down_rate) : "—";
  }
  if (kind === "up") {
    return torrent.state === "seeding" || torrent.state === "downloading" || torrent.state === "completed"
      ? fmtRate(torrent.up_rate)
      : "—";
  }
  if (torrent.state === "downloading" || torrent.state === "paused" || torrent.state === "queued") {
    // Осталось скачать только выбранные файлы (контракт 1.1).
    const remaining = torrent.wanted_bytes - torrent.downloaded_bytes;
    return remaining > 0 && torrent.wanted_bytes > 0 ? fmtBytes(remaining) : "—";
  }
  return "—";
}

export function createList(mount: HTMLElement, options: ListOptions): TorrentList {
  const title = h("h1", { class: "toolbar__title", text: FILTER_LABELS.all, attrs: { id: "list-title" } });
  const subtitle = h("p", { class: "toolbar__sub", text: "Загрузка…", attrs: { id: "list-sub" } });

  const searchInput = h("input", {
    class: "search__input",
    attrs: {
      type: "search",
      placeholder: "Поиск по названию или хешу",
      "aria-label": "Поиск торрентов",
      autocomplete: "off",
      spellcheck: "false",
    },
  });
  const searchClear = h("button", {
    class: "search__clear",
    attrs: { type: "button", "aria-label": "Очистить поиск", hidden: "" },
  });
  searchClear.append(icon("close", 13));
  const search = h(
    "div",
    { class: "search" },
    h("span", { class: "search__icon" }, icon("search", 15)),
    searchInput,
    searchClear,
  );

  // Единый выпадающий список (контракт 4.7) вместо нативного <select>.
  const sortSelect = createSelect({
    label: "Сортировка списка",
    options: SORT_OPTIONS.map((option) => ({ value: option.value, label: option.label })),
    value: "added",
    onChange: (value) => options.onSort(value as SortKey),
    className: "sort__control",
    menuWidth: 196,
  });
  const sort = h(
    "div",
    { class: "sort" },
    h("span", { class: "sort__label", text: "Сортировка" }),
    sortSelect.element,
  );

  const addButton = h("button", {
    class: "btn btn--primary",
    attrs: { type: "button", title: "Добавить торрент (Ctrl+O)" },
  });
  addButton.append(icon("plus", 16), h("span", { text: "Добавить" }));

  const toolbar = h(
    "header",
    { class: "toolbar" },
    h("div", { class: "toolbar__titles" }, title, subtitle),
    h("div", { class: "toolbar__tools" }, search, sort, addButton),
  );

  const rowsEl = h("div", {
    class: "list",
    attrs: { role: "listbox", "aria-label": "Торренты", tabindex: "-1" },
  });

  const emptyIcon = h("span", { class: "empty__icon" });
  const emptyTitle = h("h2", { class: "empty__title", text: "" });
  const emptyText = h("p", { class: "empty__text", text: "" });
  const emptyHint = h("p", { class: "empty__hint", text: "" });
  const emptyAction = h("button", {
    class: "btn btn--primary empty__action",
    attrs: { type: "button", hidden: "" },
  });
  const empty = h(
    "div",
    { class: "empty", attrs: { hidden: "" } },
    emptyIcon,
    emptyTitle,
    emptyText,
    emptyHint,
    emptyAction,
  );

  const body = h("div", { class: "list-body" }, rowsEl, empty);
  mount.replaceChildren(toolbar, body);

  const rowsById = new Map<number, RowView>();

  function rowIds(): number[] {
    return Array.from(rowsEl.children)
      .map((node) => Number((node as HTMLElement).dataset.id))
      .filter((id) => Number.isFinite(id));
  }

  function focusRow(id: number): void {
    const view = rowsById.get(id);
    view?.el.focus();
  }

  function moveFocus(delta: number | "first" | "last"): void {
    const ids = rowIds();
    if (ids.length === 0) {
      return;
    }
    const current = Number((document.activeElement as HTMLElement | null)?.dataset?.id);
    let index = ids.indexOf(current);
    if (delta === "first") {
      index = 0;
    } else if (delta === "last") {
      index = ids.length - 1;
    } else {
      index = index < 0 ? 0 : Math.min(ids.length - 1, Math.max(0, index + delta));
    }
    const next = ids[index];
    if (next !== undefined) {
      options.onSelect(next);
      focusRow(next);
    }
  }

  rowsEl.addEventListener("keydown", (event) => {
    const target = event.target;
    if (!(target instanceof HTMLElement)) {
      return;
    }
    if (target.closest("button")) {
      return;
    }
    const row = target.closest<HTMLElement>(".torrent");
    if (!row || row.dataset.id === undefined) {
      return;
    }
    const id = Number(row.dataset.id);
    switch (event.key) {
      case "ArrowDown":
        event.preventDefault();
        moveFocus(1);
        break;
      case "ArrowUp":
        event.preventDefault();
        moveFocus(-1);
        break;
      case "Home":
        event.preventDefault();
        moveFocus("first");
        break;
      case "End":
        event.preventDefault();
        moveFocus("last");
        break;
      case " ":
        event.preventDefault();
        options.onPauseToggle(id);
        break;
      case "Enter":
        event.preventDefault();
        options.onSelect(id);
        break;
      case "Delete":
      case "Backspace":
        event.preventDefault();
        options.onContextMenu(id, row);
        break;
      default:
        break;
    }
  });

  function createRow(torrent: TorrentSnapshot): RowView {
    const name = h("p", { class: "torrent__name", text: torrent.name });
    const badge = h("span", { class: "badge" });
    const metaMain = h("span", { class: "torrent__meta-main" });
    const metaStatus = h("span", { class: "torrent__meta-status" });
    const meta = h("div", { class: "torrent__meta" }, badge, metaMain, metaStatus);

    const fill = h("i", { class: "torrent__bar-fill" });
    const bar = h(
      "div",
      {
        class: "torrent__bar",
        attrs: {
          role: "progressbar",
          "aria-valuemin": "0",
          "aria-valuemax": "100",
          "aria-valuenow": "0",
          "aria-label": "Прогресс загрузки",
        },
      },
      fill,
    );
    const percent = h("span", { class: "torrent__percent num" });
    const progress = h("div", { class: "torrent__progress" }, bar, percent);
    const main = h("div", { class: "torrent__main" }, name, meta, progress);

    const down = h("b", { class: "torrent__stat-value num" });
    const up = h("b", { class: "torrent__stat-value num" });
    const left = h("b", { class: "torrent__stat-value num" });
    const stats = h(
      "div",
      { class: "torrent__stats" },
      h(
        "div",
        { class: "torrent__stat" },
        h("span", { class: "torrent__stat-key", text: "Приём" }),
        down,
      ),
      h(
        "div",
        { class: "torrent__stat" },
        h("span", { class: "torrent__stat-key", text: "Отдача" }),
        up,
      ),
      h(
        "div",
        { class: "torrent__stat" },
        h("span", { class: "torrent__stat-key", text: "Осталось" }),
        left,
      ),
    );

    const pause = h("button", {
      class: "icon-btn icon-btn--sm torrent__pause",
      attrs: { type: "button" },
    });
    const menu = h("button", {
      class: "icon-btn icon-btn--sm",
      attrs: { type: "button", "aria-label": "Действия с торрентом", "aria-haspopup": "menu" },
    });
    menu.append(icon("more", 16));
    const actions = h("div", { class: "torrent__actions" }, pause, menu);

    const el = h(
      "div",
      {
        class: "torrent",
        attrs: {
          role: "option",
          "aria-selected": "false",
          tabindex: "-1",
          "data-id": String(torrent.id),
          id: `torrent-row-${torrent.id}`,
          "data-state": torrent.state,
        },
      },
      main,
      stats,
      actions,
    );

    const view: RowView = {
      el,
      name,
      metaMain,
      metaStatus,
      badge,
      bar,
      fill,
      percent,
      down,
      up,
      left,
      pause,
      scale: { value: -1 },
      badgeState: null,
      action: null,
      actionState: null,
    };

    el.addEventListener("click", (event) => {
      if (event.target instanceof HTMLElement && event.target.closest("button")) {
        return;
      }
      options.onSelect(torrent.id);
    });
    el.addEventListener("dblclick", (event) => {
      if (event.target instanceof HTMLElement && event.target.closest("button")) {
        return;
      }
      options.onOpen(torrent.id);
    });
    el.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      event.stopPropagation();
      options.onContextMenu(torrent.id, { x: event.clientX, y: event.clientY, restore: el });
    });
    // Кнопки строки не должны выделять строку и открывать панель (контракт 4.5).
    pause.addEventListener("click", (event) => {
      event.stopPropagation();
      options.onPauseToggle(torrent.id);
    });
    menu.addEventListener("click", (event) => {
      event.stopPropagation();
      options.onContextMenu(torrent.id, menu);
    });

    return view;
  }

  function updateRow(view: RowView, torrent: TorrentSnapshot, selectedId: number | null): void {
    setText(view.name, torrent.name);
    setAttr(view.el, "title", torrent.name);
    setAttr(view.el, "aria-selected", torrent.id === selectedId ? "true" : "false");
    setAttr(view.el, "data-state", torrent.state);

    const badge = BADGES[torrent.state];
    setText(view.badge, badge.label);
    if (view.badgeState !== torrent.state) {
      view.badgeState = torrent.state;
      view.badge.className = `badge badge--${badge.cls}`;
    }

    setText(view.metaMain, metaMainText(torrent));
    const status = metaStatusText(torrent);
    setText(view.metaStatus, status);
    if (torrent.state === "error") {
      setAttr(view.metaStatus, "title", status);
    }

    setScaleX(view.fill, torrent.progress, view.scale);
    setText(view.percent, fmtPercent(torrent.progress));
    setAttr(view.bar, "aria-valuenow", String(Math.round(torrent.progress * 100)));

    setText(view.down, statValue(torrent, "down"));
    setText(view.up, statValue(torrent, "up"));
    setText(view.left, statValue(torrent, "left"));

    const action = actionFor(torrent.state);
    if (action !== view.action || torrent.state !== view.actionState) {
      view.action = action;
      view.actionState = torrent.state;
      if (action) {
        view.pause.hidden = false;
        const iconName = action === "pause" ? "pause" : action === "retry" ? "refresh" : "play";
        view.pause.replaceChildren(icon(iconName, 15));
        const label = actionLabel(action, torrent.state);
        setAttr(view.pause, "aria-label", `${label}: ${torrent.name}`);
        setAttr(view.pause, "title", label);
      } else {
        view.pause.hidden = true;
      }
    }
  }

  function emptyVariant(state: AppState, visible: TorrentSnapshot[]): void {
    if (state.fatalError) {
      emptyIcon.replaceChildren(icon("alertCircle", 24));
      setText(emptyTitle, "Нет связи с движком");
      setText(emptyText, state.fatalError);
      setText(emptyHint, "");
      emptyAction.hidden = false;
      setText(emptyAction, "Повторить");
      emptyAction.onclick = () => options.onRetry();
      empty.hidden = false;
      return;
    }
    if (!state.ready) {
      emptyIcon.replaceChildren(icon("layers", 24));
      setText(emptyTitle, "Загрузка…");
      setText(emptyText, "Получаем состояние движка.");
      setText(emptyHint, "");
      emptyAction.hidden = true;
      empty.hidden = false;
      return;
    }
    if (visible.length > 0) {
      empty.hidden = true;
      return;
    }

    const total = state.payload?.torrents.length ?? 0;
    empty.hidden = false;
    if (total === 0) {
      emptyIcon.replaceChildren(icon("magnet", 24));
      setText(emptyTitle, "Пока пусто");
      setText(emptyText, "Добавьте .torrent или magnet-ссылку — загрузка появится здесь.");
      setText(emptyHint, "Ctrl+O — открыть файл · Ctrl+V — вставить ссылку");
      emptyAction.hidden = false;
      setText(emptyAction, "Добавить торрент");
      emptyAction.onclick = () => options.onAdd();
    } else {
      emptyIcon.replaceChildren(icon("search", 24));
      setText(emptyTitle, "Ничего не найдено");
      setText(
        emptyText,
        state.query.trim()
          ? `По запросу «${state.query.trim()}» ничего нет.`
          : "В этом фильтре нет торрентов.",
      );
      setText(emptyHint, "");
      emptyAction.hidden = false;
      setText(emptyAction, "Сбросить фильтры");
      emptyAction.onclick = () => options.onClearFilters();
    }
  }

  searchInput.addEventListener("input", () => options.onQuery(searchInput.value));
  searchInput.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      if (searchInput.value !== "") {
        event.preventDefault();
        event.stopPropagation();
        options.onQuery("");
      }
      return;
    }
    if (event.key === "ArrowDown") {
      event.preventDefault();
      focusSelected();
    }
  });
  searchClear.addEventListener("click", () => {
    options.onQuery("");
    searchInput.focus();
  });
  addButton.addEventListener("click", () => options.onAdd());

  let lastQuery = "";
  let lastSort: SortKey | null = null;

  function focusSelected(): void {
    const rows = Array.from(rowsEl.children) as HTMLElement[];
    if (rows.length === 0) {
      return;
    }
    const selected = rows.find((row) => row.getAttribute("aria-selected") === "true");
    (selected ?? rows[0])?.focus();
  }

  return {
    element: mount,
    focusSearch() {
      searchInput.focus();
      searchInput.select();
    },
    focusSelected,
    update(state) {
      const visible = visibleTorrents(state);
      const selectedId = state.selectedId;
      const selectedVisible = visible.some((torrent) => torrent.id === selectedId);

      setText(title, FILTER_LABELS[state.filter]);
      setText(
        subtitle,
        state.payload
          ? `${fmtTorrents(visible.length)} · ${fmtActive(activeCount(visible))}`
          : "Подключение к движку…",
      );

      if (lastQuery !== state.query && searchInput.value !== state.query) {
        searchInput.value = state.query;
      }
      lastQuery = state.query;
      searchClear.hidden = state.query === "";
      if (lastSort !== state.sort) {
        lastSort = state.sort;
        sortSelect.setValue(state.sort);
      }

      const seen = new Set<number>();
      let previous: HTMLElement | null = null;
      for (const torrent of visible) {
        seen.add(torrent.id);
        let view = rowsById.get(torrent.id);
        if (!view) {
          view = createRow(torrent);
          rowsById.set(torrent.id, view);
        }
        updateRow(view, torrent, selectedVisible ? selectedId : null);
        const reference: Node | null = previous ? previous.nextSibling : rowsEl.firstChild;
        if (reference !== view.el) {
          rowsEl.insertBefore(view.el, reference);
        }
        previous = view.el;
      }
      for (const [id, view] of rowsById) {
        if (!seen.has(id)) {
          view.el.remove();
          rowsById.delete(id);
        }
      }

      const ids = rowIds();
      for (let index = 0; index < ids.length; index += 1) {
        const view = rowsById.get(ids[index]);
        if (!view) {
          continue;
        }
        const tabbable = selectedVisible ? ids[index] === selectedId : index === 0;
        setAttr(view.el, "tabindex", tabbable ? "0" : "-1");
      }

      emptyVariant(state, visible);
    },
  };
}
