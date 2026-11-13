import type { TorrentSnapshot, TorrentState } from "../api/types";
import type { AppState } from "../state/store";
import { FILTER_LABELS, activeCount, matchesFilter } from "../state/view";
import {
  fmtBytes,
  fmtCount,
  fmtDate,
  fmtEta,
  fmtPercent,
  fmtRate,
  fmtTorrents,
  fmtActive,
  remainingBytes,
} from "../lib/format";
import { h, setAttr, setScaleX, setText } from "../lib/dom";
import { icon } from "../lib/icons";
import { createSelect, type SelectControl, type SelectOption } from "./select";

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

// --- Мультисортировка (1.7) ---------------------------------------------
// До трёх ключей: порядок в массиве — приоритет (первый решает, второй
// разбивает равенства и т.д.). Спецификация живёт в localStorage, поэтому
// сохраняется между запусками; значения санитизируем при чтении.

export type SortField = "added" | "name" | "size" | "progress" | "speed" | "ratio" | "state";
export type SortDirection = "asc" | "desc";

export interface SortSpec {
  key: SortField;
  dir: SortDirection;
}

export const MAX_SORT_KEYS = 3;

const SORT_STORAGE_KEY = "torrenta.sort";

const SORT_FIELDS: SortField[] = ["added", "name", "size", "progress", "speed", "ratio", "state"];

export const SORT_LABELS: Record<SortField, string> = {
  added: "Сначала новые",
  name: "Имя",
  size: "Размер",
  progress: "Прогресс",
  speed: "Скорость",
  ratio: "Рейтинг",
  state: "Состояние",
};

/** Направление по умолчанию: «Сначала новые» — убывание даты добавления. */
const SORT_DEFAULT_DIR: Record<SortField, SortDirection> = {
  added: "desc",
  name: "asc",
  size: "desc",
  progress: "desc",
  speed: "desc",
  ratio: "desc",
  state: "asc",
};

const DEFAULT_SORT: SortSpec[] = [{ key: "added", dir: "desc" }];

/** Канонический порядок состояний для сортировки: активные сверху. */
const STATE_ORDER: Record<TorrentState, number> = {
  downloading: 0,
  seeding: 1,
  metadata: 2,
  queued: 3,
  checking: 4,
  completed: 5,
  paused: 6,
  error: 7,
};

const COLLATOR = new Intl.Collator("ru", { sensitivity: "base", numeric: true });

function copySpec(spec: SortSpec): SortSpec {
  return { key: spec.key, dir: spec.dir };
}

/** Поиск без учёта регистра и разницы «е»/«ё» — для названия, пути и хеша. */
function normalizeText(value: string): string {
  return value.toLowerCase().replace(/ё/g, "е");
}

function isSortField(value: unknown): value is SortField {
  return typeof value === "string" && (SORT_FIELDS as string[]).includes(value);
}

function normalizeSort(value: unknown): SortSpec[] {
  if (!Array.isArray(value)) {
    return [];
  }
  const seen = new Set<SortField>();
  const result: SortSpec[] = [];
  for (const item of value) {
    if (result.length >= MAX_SORT_KEYS) {
      break;
    }
    if (!item || typeof item !== "object") {
      continue;
    }
    const raw = item as { key?: unknown; dir?: unknown };
    if (!isSortField(raw.key) || seen.has(raw.key)) {
      continue;
    }
    seen.add(raw.key);
    result.push({ key: raw.key, dir: raw.dir === "asc" ? "asc" : "desc" });
  }
  return result;
}

function readSort(): SortSpec[] {
  try {
    const raw = window.localStorage.getItem(SORT_STORAGE_KEY);
    const parsed = raw ? normalizeSort(JSON.parse(raw)) : [];
    return parsed.length > 0 ? parsed : DEFAULT_SORT.map(copySpec);
  } catch {
    return DEFAULT_SORT.map(copySpec);
  }
}

function storeSort(spec: SortSpec[]): void {
  try {
    window.localStorage.setItem(SORT_STORAGE_KEY, JSON.stringify(spec));
  } catch {
    // Приватный режим или запрет хранилища — сортировка просто не запомнится.
  }
}

/**
 * Короткая подпись направления для строки ключа. У «Сначала новые» стрелка
 * сама показывает смысл — текстом его дублировать не нужно.
 */
function directionText(spec: SortSpec): string {
  if (spec.key === "name") {
    return spec.dir === "asc" ? "А→Я" : "Я→А";
  }
  if (spec.key === "added") {
    return "";
  }
  return spec.dir === "asc" ? "возр." : "убыв.";
}

/** Подпись ключа с учётом направления — для кнопки в тулбаре. */
function fieldText(spec: SortSpec): string {
  if (spec.key === "added") {
    return spec.dir === "desc" ? "Сначала новые" : "Сначала старые";
  }
  return SORT_LABELS[spec.key];
}

function directionLabel(spec: SortSpec): string {
  if (spec.key === "name") {
    return spec.dir === "asc" ? "по алфавиту" : "в обратном порядке";
  }
  if (spec.key === "added") {
    return spec.dir === "desc" ? "сначала новые" : "сначала старые";
  }
  return spec.dir === "asc" ? "по возрастанию" : "по убыванию";
}

function compareField(a: TorrentSnapshot, b: TorrentSnapshot, field: SortField): number {
  switch (field) {
    case "name":
      return COLLATOR.compare(normalizeText(a.name), normalizeText(b.name));
    case "size":
      return a.total_bytes - b.total_bytes;
    case "progress":
      return a.progress - b.progress;
    case "speed":
      return a.down_rate + a.up_rate - (b.down_rate + b.up_rate);
    case "ratio": {
      // Ещё ничего не скачано — рейтинг неизвестен, такие всегда в конце.
      const left = a.downloaded_bytes > 0 ? a.seed_ratio : Number.NEGATIVE_INFINITY;
      const right = b.downloaded_bytes > 0 ? b.seed_ratio : Number.NEGATIVE_INFINITY;
      if (left === right) {
        return 0;
      }
      return left < right ? -1 : 1;
    }
    case "state":
      return STATE_ORDER[a.state] - STATE_ORDER[b.state];
    default:
      return a.added_at - b.added_at;
  }
}

function sortTorrents(torrents: TorrentSnapshot[], spec: SortSpec[]): TorrentSnapshot[] {
  const keys = spec.length > 0 ? spec : DEFAULT_SORT;
  return torrents.slice().sort((a, b) => {
    for (const key of keys) {
      const delta = compareField(a, b, key.key);
      if (delta !== 0) {
        return key.dir === "desc" ? -delta : delta;
      }
    }
    // Устойчивый хвост: свежие выше, затем по id — порядок не «дрожит»
    // при обновлении payload.
    const added = b.added_at - a.added_at;
    return added !== 0 ? added : b.id - a.id;
  });
}

function visibleTorrents(state: AppState, spec: SortSpec[]): TorrentSnapshot[] {
  const payload = state.payload;
  if (!payload) {
    return [];
  }
  const query = normalizeText(state.query.trim());
  const filtered = payload.torrents.filter((torrent) => {
    if (!matchesFilter(torrent, state.filter)) {
      return false;
    }
    if (!query) {
      return true;
    }
    return (
      normalizeText(torrent.name).includes(query) ||
      normalizeText(torrent.save_path).includes(query) ||
      normalizeText(torrent.info_hash).includes(query)
    );
  });
  return sortTorrents(filtered, spec);
}

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

/**
 * Мировой размер роя по данным трекера: сиды + личеры.
 * Это НЕ число наших сокетов: в рое есть пиры, к которым мы не подключены,
 * поэтому величина не ограничена лимитом подключений. 0 — анонса ещё не было.
 */
function swarmSize(torrent: TorrentSnapshot): number {
  const seeds = Number.isFinite(torrent.swarm_seeds) ? torrent.swarm_seeds : 0;
  const leechers = Number.isFinite(torrent.swarm_leechers) ? torrent.swarm_leechers : 0;
  return Math.max(0, seeds) + Math.max(0, leechers);
}

function metaMainText(torrent: TorrentSnapshot): string {
  const size = torrent.total_bytes > 0 ? fmtBytes(torrent.total_bytes) : "Размер неизвестен";
  // В скобках — размер роя, как Seeds/Peers у qBittorrent: иначе «12» читается
  // как «всего 12 пиров», хотя это только наши сокеты.
  const swarm = swarmSize(torrent);
  const peers =
    swarm > 0
      ? `${fmtCount(torrent.peers_connected)} (${fmtCount(swarm)})`
      : fmtCount(torrent.peers_connected);
  return `${size} · пиры ${peers}`;
}

/** Ratio из снимка (1.6): движок уже поделил отдано на скачано. */
function seedRatioText(ratio: number): string {
  // Два знака и десятичная запятая — как у остальных чисел интерфейса.
  const safe = Number.isFinite(ratio) && ratio > 0 ? ratio : 0;
  return `×${safe.toFixed(2).replace(".", ",")}`;
}

function metaStatusText(torrent: TorrentSnapshot): string {
  // Лимит раздачи (1.6) перебивает состояние: движок уже остановил торрент
  // и ждёт ручного возобновления, поэтому ETA и даты здесь ни о чём не говорят.
  if (torrent.seed_limit_reached) {
    return `лимит раздачи · ${seedRatioText(torrent.seed_ratio)}`;
  }
  switch (torrent.state) {
    case "downloading":
      // Все файлы исключены приоритетом «не скачивать»: качать нечего.
      if (torrent.wanted_bytes === 0 && torrent.files.length > 0) {
        return "файлы не выбраны";
      }
      return torrent.eta_seconds === null ? "ETA неизвестно" : `осталось ${fmtEta(torrent.eta_seconds)}`;
    case "seeding":
      return `отдано ${fmtBytes(torrent.uploaded_bytes)} · раздача ${seedRatioText(torrent.seed_ratio)}`;
    case "completed":
      return `завершён ${fmtDate(torrent.added_at)} · раздача ${seedRatioText(torrent.seed_ratio)}`;
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
    // Осталось скачать только выбранные файлы; считает движок (1.8).
    const remaining = remainingBytes(torrent);
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
      placeholder: "Поиск по названию, пути или хешу",
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

  // --- Сортировка (1.7) --------------------------------------------------
  // Панель с ключами вместо одиночного списка: до трёх ключей, порядок в
  // списке задаёт приоритет, у каждого ключа своё направление.
  let sortSpec = readSort();
  let sortOpen = false;
  let lastState: AppState | null = null;

  type SortFocus = { action: "dir" | "up" | "down" | "remove"; index: number };

  const sortValue = h("span", { class: "sort__value" });
  const sortButton = h("button", {
    class: "sort__button",
    attrs: {
      type: "button",
      "aria-haspopup": "dialog",
      "aria-expanded": "false",
      "aria-label": "Сортировка списка",
      title: "Сортировка списка",
    },
  });
  sortButton.append(
    h("span", { class: "sort__label", text: "Сортировка" }),
    sortValue,
    icon("chevronDown", 13),
  );

  const sortPanelTitle = h("h2", {
    class: "sort-panel__title",
    attrs: { id: "sort-panel-title" },
    text: "Сортировка списка",
  });
  const sortKeysList = h("ol", { class: "sort-panel__keys" });
  /** Пункт-заглушка: без него повторный выбор «того же» значения молчал бы. */
  const SORT_ADD_PLACEHOLDER: SelectOption = { value: "", label: "Выберите поле" };

  const sortAddSelect: SelectControl = createSelect({
    label: "Добавить ключ сортировки",
    options: [SORT_ADD_PLACEHOLDER],
    value: "",
    onChange: (value) => {
      if (isSortField(value)) {
        addSortKey(value);
      }
    },
    className: "sort-panel__select",
    menuWidth: 188,
  });
  const sortAddRow = h(
    "div",
    { class: "sort-panel__add-row" },
    h("span", { class: "sort-panel__add-label", text: "Добавить ключ" }),
    sortAddSelect.element,
  );
  const sortHint = h("p", {
    class: "sort-panel__hint",
    text: "Первый ключ — главный, остальные уточняют порядок.",
  });
  const sortReset = h("button", {
    class: "btn btn--xs btn--ghost sort-panel__reset",
    attrs: { type: "button" },
    text: "Сбросить",
  });
  const sortPanel = h(
    "div",
    {
      class: "sort-panel",
      attrs: {
        role: "dialog",
        "aria-modal": "false",
        "aria-labelledby": "sort-panel-title",
        hidden: "",
      },
    },
    sortPanelTitle,
    sortKeysList,
    sortAddRow,
    h("div", { class: "sort-panel__foot" }, sortHint, sortReset),
  );
  const sort = h("div", { class: "sort" }, sortButton, sortPanel);

  function syncSortButton(): void {
    const first = sortSpec[0] ?? { key: "added" as SortField, dir: "desc" as SortDirection };
    const extra = sortSpec.length > 1 ? ` +${sortSpec.length - 1}` : "";
    setText(sortValue, `${fieldText(first)}${extra}`);
    const chain = sortSpec
      .map((item) => {
        const short = directionText(item);
        return short ? `${fieldText(item)} ${short}` : fieldText(item);
      })
      .join(", ");
    setAttr(sortButton, "aria-label", `Сортировка списка: ${chain}`);
    setAttr(sortButton, "title", `Сортировка списка: ${chain}`);
    setAttr(sortButton, "aria-expanded", sortOpen ? "true" : "false");
  }

  function moveSortKey(index: number, delta: number): void {
    const target = index + delta;
    const item = sortSpec[index];
    if (!item || target < 0 || target >= sortSpec.length) {
      return;
    }
    sortSpec.splice(index, 1);
    sortSpec.splice(target, 0, item);
  }

  function addSortKey(field: SortField): void {
    if (sortSpec.length >= MAX_SORT_KEYS || sortSpec.some((item) => item.key === field)) {
      return;
    }
    sortSpec.push({ key: field, dir: SORT_DEFAULT_DIR[field] });
    applySort({ action: "dir", index: sortSpec.length - 1 });
  }

  function sortKeyRow(item: SortSpec, index: number): HTMLElement {
    const label = SORT_LABELS[item.key];

    const dirButton = h("button", {
      class: "sort-panel__dir",
      attrs: {
        type: "button",
        "data-action": "dir",
        "aria-label": `Направление сортировки: ${directionLabel(item)}`,
        title: "Изменить направление",
      },
    });
    dirButton.append(icon(item.dir === "asc" ? "arrowUp" : "arrowDown", 12));
    const dirText = directionText(item);
    if (dirText) {
      dirButton.append(h("span", { text: dirText }));
    }
    dirButton.addEventListener("click", () => {
      sortSpec[index] = { key: item.key, dir: item.dir === "asc" ? "desc" : "asc" };
      applySort({ action: "dir", index });
    });

    const upButton = h("button", {
      class: "sort-panel__move",
      attrs: {
        type: "button",
        "data-action": "up",
        "aria-label": `Выше приоритетом: ${label}`,
        title: "Выше приоритетом",
      },
    });
    upButton.append(icon("arrowUp", 12));
    upButton.disabled = index === 0;
    upButton.addEventListener("click", () => {
      moveSortKey(index, -1);
      applySort({ action: "up", index: index - 1 });
    });

    const downButton = h("button", {
      class: "sort-panel__move",
      attrs: {
        type: "button",
        "data-action": "down",
        "aria-label": `Ниже приоритетом: ${label}`,
        title: "Ниже приоритетом",
      },
    });
    downButton.append(icon("arrowDown", 12));
    downButton.disabled = index === sortSpec.length - 1;
    downButton.addEventListener("click", () => {
      moveSortKey(index, 1);
      applySort({ action: "down", index: index + 1 });
    });

    const removeButton = h("button", {
      class: "sort-panel__remove",
      attrs: {
        type: "button",
        "data-action": "remove",
        "aria-label": `Убрать ключ: ${label}`,
        title: "Убрать ключ",
      },
    });
    removeButton.append(icon("close", 12));
    removeButton.disabled = sortSpec.length === 1;
    removeButton.addEventListener("click", () => {
      sortSpec.splice(index, 1);
      applySort({ action: "remove", index: Math.max(0, Math.min(index, sortSpec.length - 1)) });
    });

    return h(
      "li",
      { class: "sort-panel__key", attrs: { "data-index": String(index) } },
      h("span", { class: "sort-panel__rank", attrs: { "aria-hidden": "true" }, text: String(index + 1) }),
      h("span", { class: "sort-panel__name", text: label }),
      h("span", { class: "sort-panel__controls" }, dirButton, upButton, downButton, removeButton),
    );
  }

  function renderSortKeys(focus?: SortFocus): void {
    sortKeysList.replaceChildren(...sortSpec.map((item, index) => sortKeyRow(item, index)));
    const used = new Set(sortSpec.map((item) => item.key));
    const unused = SORT_FIELDS.filter((field) => !used.has(field));
    const canAdd = sortSpec.length < MAX_SORT_KEYS && unused.length > 0;
    sortAddRow.hidden = !canAdd;
    if (canAdd) {
      sortAddSelect.setOptions(
        [SORT_ADD_PLACEHOLDER, ...unused.map((field) => ({ value: field, label: SORT_LABELS[field] }))],
        "",
      );
    }
    syncSortButton();
    if (focus) {
      const selector = `[data-index="${focus.index}"] [data-action="${focus.action}"]`;
      const node = sortKeysList.querySelector<HTMLElement>(selector) ?? sortButton;
      node.focus();
    }
  }

  function applySort(focus?: SortFocus): void {
    storeSort(sortSpec);
    renderSortKeys(focus);
    if (lastState) {
      render(lastState);
    }
  }

  function onSortOutside(event: MouseEvent): void {
    const target = event.target;
    if (!(target instanceof Element)) {
      closeSortPanel(false);
      return;
    }
    if (sort.contains(target) || target.closest(".select__menu")) {
      return;
    }
    closeSortPanel(false);
  }

  function onSortKeydown(event: KeyboardEvent): void {
    if (event.key !== "Escape") {
      return;
    }
    const active = document.activeElement;
    if (!(active instanceof Node) || !(sortPanel.contains(active) || active === sortButton)) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    closeSortPanel();
  }

  function openSortPanel(): void {
    if (sortOpen) {
      return;
    }
    sortOpen = true;
    sortPanel.hidden = false;
    sortPanel.classList.add("is-open");
    renderSortKeys();
    document.addEventListener("mousedown", onSortOutside, true);
    document.addEventListener("keydown", onSortKeydown, true);
    const first = sortKeysList.querySelector<HTMLElement>("[data-action='dir']");
    (first ?? sortReset).focus();
  }

  function closeSortPanel(restoreFocus = true): void {
    if (!sortOpen) {
      return;
    }
    sortOpen = false;
    sortPanel.hidden = true;
    sortPanel.classList.remove("is-open");
    sortAddSelect.close();
    document.removeEventListener("mousedown", onSortOutside, true);
    document.removeEventListener("keydown", onSortKeydown, true);
    syncSortButton();
    if (restoreFocus) {
      sortButton.focus();
    }
  }

  sortButton.addEventListener("click", () => {
    if (sortOpen) {
      closeSortPanel();
    } else {
      openSortPanel();
    }
  });
  sortReset.addEventListener("click", () => {
    sortSpec = DEFAULT_SORT.map(copySpec);
    applySort({ action: "dir", index: 0 });
  });
  syncSortButton();

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
    // Расшифровка скобок: сколько подключено и сколько всего в рое. Без размера
    // роя подсказка не нужна — цифра в строке и так читается однозначно.
    const swarm = swarmSize(torrent);
    setAttr(
      view.metaMain,
      "title",
      swarm > 0
        ? `подключено ${fmtCount(torrent.peers_connected)} из ${fmtCount(swarm)} в рое`
        : null,
    );
    const status = metaStatusText(torrent);
    setText(view.metaStatus, status);
    if (torrent.state === "error") {
      setAttr(view.metaStatus, "title", status);
    } else {
      // Подсказка к «лимит раздачи»: как состояние снимается (1.6).
      setAttr(
        view.metaStatus,
        "title",
        torrent.seed_limit_reached ? "Лимит раздачи достигнут — возобновите вручную" : null,
      );
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

  function focusSelected(): void {
    const rows = Array.from(rowsEl.children) as HTMLElement[];
    if (rows.length === 0) {
      return;
    }
    const selected = rows.find((row) => row.getAttribute("aria-selected") === "true");
    (selected ?? rows[0])?.focus();
  }

  function render(state: AppState): void {
    lastState = state;
    const visible = visibleTorrents(state, sortSpec);
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
  }

  return {
    element: mount,
    focusSearch() {
      searchInput.focus();
      searchInput.select();
    },
    focusSelected,
    update: render,
  };
}
