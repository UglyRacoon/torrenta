import type { AppState } from "../state/store";
import type { FilePriority, FileView, TorrentSnapshot, TorrentState } from "../api/types";
import {
  basename,
  fmtBytes,
  fmtCount,
  fmtEta,
  fmtPercent,
  fmtRate,
  truncateMiddle,
} from "../lib/format";
import { h, messageOf, setAttr, setScaleX, setText } from "../lib/dom";
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

const TABS = [
  { key: "files", label: "Файлы" },
  { key: "trackers", label: "Трекеры" },
  { key: "peers", label: "Пиры" },
  { key: "pieces", label: "Куски" },
] as const;

type TabKey = (typeof TABS)[number]["key"];

/** Приоритеты файла (1.3): «не скачивать» = skip, остальные — порядок загрузки. */
const PRIORITY_OPTIONS: SelectOption[] = [
  { value: "high", label: "Высокий" },
  { value: "normal", label: "Обычный" },
  { value: "low", label: "Низкий" },
  { value: "skip", label: "Не скачивать" },
];

const PIECE_COLS = 32;
const PIECE_MAX_BUCKETS = PIECE_COLS * 6;

export interface DetailsOptions {
  onClose(): void;
  onPauseToggle(id: number): void;
  onReveal(id: number): void;
  onRemove(id: number): void;
  onOpenExternal(url: string): void;
  /** Изменение приоритетов файлов; отклонение промиса означает ошибку движка. */
  onFilePriorities(id: number, priorities: FilePriority[]): Promise<void>;
  /** Открыть редактор трекеров (модалка из `modals.ts`, 1.7). */
  onEditTrackers(id: number): void;
  /** Принудительное обновление трекеров (reannounce, 1.7); reject — ошибка движка. */
  onReannounce(id: number): Promise<void>;
  /** Последовательная загрузка (1.7); reject — ошибка движка. */
  onSequentialChange(id: number, enabled: boolean): Promise<void>;
}

export interface DetailsPanel {
  update(state: AppState): void;
}

function toggleLabel(state: TorrentState): { label: string; icon: "pause" | "play" | "refresh" } {
  switch (state) {
    case "paused":
      return { label: "Продолжить", icon: "play" };
    case "completed":
      return { label: "Раздать", icon: "play" };
    case "checking":
      return { label: "Проверка…", icon: "refresh" };
    case "error":
      return { label: "Повторить", icon: "refresh" };
    default:
      return { label: "Пауза", icon: "pause" };
  }
}

function hashString(value: string): number {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return hash >>> 0;
}

/** Ratio из снимка (1.6): движок уже поделил отдано на скачано. */
function seedRatioText(ratio: number): string {
  // Два знака и десятичная запятая — как у остальных чисел интерфейса.
  const safe = Number.isFinite(ratio) && ratio > 0 ? ratio : 0;
  return `×${safe.toFixed(2).replace(".", ",")}`;
}

function progressText(torrent: TorrentSnapshot): string {
  const remaining = Math.max(0, torrent.wanted_bytes - torrent.downloaded_bytes);
  // Лимит раздачи (1.6): торрент остановлен движком, а не пользователем.
  if (torrent.seed_limit_reached) {
    return "Раздача остановлена по лимиту";
  }
  switch (torrent.state) {
    case "downloading":
      if (torrent.wanted_bytes === 0 && torrent.files.length > 0) {
        // Все файлы исключены: качать нечего, но торрент снят не с паузы.
        return "Файлы не выбраны";
      }
      return torrent.eta_seconds === null
        ? `осталось ${fmtBytes(remaining)}`
        : `осталось ${fmtBytes(remaining)} · ${fmtEta(torrent.eta_seconds)}`;
    case "metadata":
      return "Ожидание метаданных";
    case "queued":
      return "Ожидает запуска";
    case "paused":
      if (torrent.wanted_bytes === 0 && torrent.files.length > 0) {
        // Все файлы исключены: пауза тут ничего не меняет.
        return "Файлы не выбраны";
      }
      return remaining > 0 ? `осталось ${fmtBytes(remaining)}` : "Загрузка завершена";
    case "seeding":
      return "Раздаётся · прогресс полный";
    case "checking":
      return "Проверка целостности";
    case "completed":
      return "Загрузка завершена";
    case "error":
      return torrent.error ?? "Ошибка загрузки";
    default:
      return "";
  }
}

export function createDetails(mount: HTMLElement, options: DetailsOptions): DetailsPanel {
  mount.classList.add("details");

  const name = h("h2", { class: "details__name", attrs: { id: "details-name" } });
  const badge = h("span", { class: "badge" });
  const hash = h("span", { class: "details__hash num" });
  const close = h("button", {
    class: "icon-btn icon-btn--sm details__close",
    attrs: { type: "button", "aria-label": "Закрыть детали (Esc)" },
  });
  close.append(icon("close", 16));
  const meta = h("div", { class: "details__meta" }, badge, hash);
  const head = h(
    "header",
    { class: "details__head" },
    h("div", { class: "details__heading" }, name, meta),
    close,
  );
  const error = h("p", { class: "details__error", attrs: { role: "alert", hidden: true } });
  // Лимит раздачи (1.6): торрент остановлен движком и ждёт ручного возобновления.
  const limit = h("p", { class: "details__error", attrs: { role: "status", hidden: true } });

  const toggle = h("button", { class: "btn btn--soft details__toggle", attrs: { type: "button" } });
  const toggleIcon = h("span", { class: "btn__icon" });
  const toggleLabelEl = h("span");
  toggle.append(toggleIcon, toggleLabelEl);
  const reveal = h("button", {
    class: "btn btn--ghost details__reveal",
    attrs: { type: "button", title: "Показать файл в проводнике" },
  });
  reveal.append(icon("folderOpen", 15), h("span", { text: "Каталог" }));
  const remove = h("button", {
    class: "icon-btn icon-btn--sm icon-btn--danger details__remove",
    attrs: { type: "button", "aria-label": "Удалить торрент", title: "Удалить торрент" },
  });
  remove.append(icon("trash", 16));
  const actions = h("div", { class: "details__actions" }, toggle, reveal, remove);

  const fill = h("i", { class: "details__bar-fill" });
  const bar = h(
    "div",
    {
      class: "details__bar",
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
  const percent = h("b", { class: "details__percent num" });
  const progressNote = h("span", { class: "details__progress-note" });
  const progressRow = h("div", { class: "details__progress-row" }, percent, progressNote);
  const progress = h("div", { class: "details__progress" }, bar, progressRow);

  function statCard(key: string): { card: HTMLElement; value: HTMLElement; sub: HTMLElement } {
    const value = h("b", { class: "stat__value num" });
    const sub = h("span", { class: "stat__sub" });
    const card = h(
      "div",
      { class: "stat" },
      h("span", { class: "stat__key", text: key }),
      value,
      sub,
    );
    return { card, value, sub };
  }
  const downloaded = statCard("Скачано");
  const uploaded = statCard("Отдано");
  const ratio = statCard("Рейтинг");
  const left = statCard("Осталось");
  const stats = h(
    "div",
    { class: "details__stats" },
    downloaded.card,
    uploaded.card,
    ratio.card,
    left.card,
  );

  // --- Опции торрента (1.7) ----------------------------------------------
  const sequentialKnob = h("span", { class: "switch__knob" });
  const sequentialTrack = h(
    "span",
    { class: "switch__track", attrs: { "aria-hidden": "true" } },
    sequentialKnob,
  );
  const sequentialSwitch = h(
    "button",
    {
      class: "switch details__switch",
      attrs: {
        type: "button",
        role: "switch",
        "aria-checked": "false",
        "aria-label": "Последовательная загрузка",
        title: "Последовательная загрузка",
      },
    },
    sequentialTrack,
  );
  const sequentialNote = h("p", {
    class: "details__option-note",
    attrs: { role: "status", "aria-live": "polite", hidden: true },
  });
  const sequentialRow = h(
    "div",
    { class: "details__option" },
    h(
      "div",
      { class: "details__option-text" },
      h("span", { class: "details__option-label", text: "Последовательная загрузка" }),
      h("span", {
        class: "details__option-hint",
        text: "Куски качаются по порядку — можно смотреть по ходу загрузки.",
      }),
    ),
    sequentialSwitch,
  );
  const optionsBlock = h("div", { class: "details__options" }, sequentialRow, sequentialNote);

  const tabButtons = new Map<TabKey, HTMLButtonElement>();
  const tabCounts = new Map<TabKey, HTMLElement>();
  const tablist = h("div", {
    class: "tabs",
    attrs: { role: "tablist", "aria-label": "Разделы торрента" },
  });
  const panels = new Map<TabKey, HTMLElement>();

  function panel(key: TabKey, ariaLabel: string): HTMLElement {
    const node = h("div", {
      class: "tabpanel",
      attrs: {
        role: "tabpanel",
        id: `details-panel-${key}`,
        "aria-labelledby": `details-tab-${key}`,
        "aria-label": ariaLabel,
        hidden: true,
      },
    });
    panels.set(key, node);
    return node;
  }

  for (const tab of TABS) {
    const count = h("span", { class: "tabs__count num", hidden: true });
    tabCounts.set(tab.key, count);
    const button = h("button", {
      class: "tabs__tab",
      attrs: {
        type: "button",
        role: "tab",
        id: `details-tab-${tab.key}`,
        "aria-controls": `details-panel-${tab.key}`,
        "aria-selected": "false",
        tabindex: "-1",
      },
    });
    button.append(h("span", { text: tab.label }), count);
    tabButtons.set(tab.key, button);
    tablist.append(button);
  }

  // --- Файлы -------------------------------------------------------------
  const filesSummary = h("p", { class: "files__summary num" });
  const selectAllButton = h("button", {
    class: "btn btn--xs btn--ghost",
    attrs: { type: "button" },
    text: "Выбрать все",
  });
  const selectNoneButton = h("button", {
    class: "btn btn--xs btn--ghost",
    attrs: { type: "button" },
    text: "Снять все",
  });
  const filesHead = h(
    "div",
    { class: "files__head", attrs: { hidden: "" } },
    filesSummary,
    h("div", { class: "files__tools" }, selectAllButton, selectNoneButton),
  );
  const filesEmpty = h("p", { class: "tabpanel__empty", text: "Файлы появятся после получения метаданных", hidden: true });
  const filesList = h("div", { class: "files", attrs: { role: "list" } });
  const filesPanel = panel("files", "Файлы торрента");
  filesPanel.append(filesHead, filesList, filesEmpty);

  // --- Трекеры -----------------------------------------------------------
  // 1.7: список URL из движка + «Изменить» (редактор) и «Обновить трекеры»
  // (reannounce). Пустой список — это норма: работают DHT и PEX.
  const trackersEmpty = h("p", {
    class: "tabpanel__empty",
    text: "Трекеров нет: DHT и PEX",
    hidden: true,
  });
  const trackersEdit = h("button", {
    class: "btn btn--xs btn--ghost",
    attrs: { type: "button", title: "Изменить список трекеров" },
    text: "Изменить",
  });
  const trackersRefresh = h("button", {
    class: "btn btn--xs btn--ghost",
    attrs: { type: "button", title: "Запросить трекеры заново" },
  });
  trackersRefresh.append(icon("refresh", 13), h("span", { text: "Обновить трекеры" }));
  const trackersHead = h("div", { class: "trackers__head" }, trackersEdit, trackersRefresh);
  const trackersList = h("div", { class: "trackers", attrs: { role: "list" } });
  const trackersNote = h("p", {
    class: "trackers__note",
    attrs: { role: "status", "aria-live": "polite", hidden: true },
  });
  const trackersPanel = panel("trackers", "Трекеры");
  trackersPanel.append(trackersHead, trackersList, trackersEmpty, trackersNote);

  // --- Пиры --------------------------------------------------------------
  const peersEmpty = h("p", { class: "tabpanel__empty", text: "Подключённых пиров нет", hidden: true });
  const peersList = h("div", { class: "peers", attrs: { role: "list" } });
  const peersNote = h("p", { class: "tabpanel__note", hidden: true });
  const peersPanel = panel("peers", "Пиры");
  peersPanel.append(peersNote, peersList, peersEmpty);

  // --- Куски -------------------------------------------------------------
  const piecesSummary = h("p", { class: "pieces__summary" });
  const piecesMap = h("div", { class: "pieces__map", attrs: { role: "img" } });
  const legend = h(
    "div",
    { class: "pieces__legend" },
    h("span", { class: "pieces__legend-item pieces__legend-item--full", text: "Скачано" }),
    h("span", { class: "pieces__legend-item pieces__legend-item--part", text: "Частично" }),
    h("span", { class: "pieces__legend-item pieces__legend-item--none", text: "Нет" }),
  );
  const piecesEmpty = h("p", { class: "tabpanel__empty", text: "Карта появится после получения метаданных", hidden: true });
  const piecesPanel = panel("pieces", "Карта кусков");
  piecesPanel.append(piecesSummary, piecesMap, legend, piecesEmpty);

  mount.replaceChildren(
    head,
    error,
    limit,
    actions,
    progress,
    stats,
    optionsBlock,
    tablist,
    filesPanel,
    trackersPanel,
    peersPanel,
    piecesPanel,
  );
  mount.hidden = true;

  let activeTab: TabKey = "files";
  let currentId: number | null = null;
  let badgeState: TorrentState | null = null;
  let toggleState: TorrentState | null = null;
  const scale = { value: -1 };
  // 1.7: состояние опций и трекеров помним отдельно от снимка — движок отвечает
  // на команды не мгновенно, а мигать интерфейсом на каждом тике нельзя.
  let currentTrackers: string[] = [];
  let trackersBusy = false;
  let sequentialShown: boolean | null = null;
  let sequentialBusy = false;

  function selectTab(key: TabKey, focus = false): void {
    activeTab = key;
    for (const tab of TABS) {
      const button = tabButtons.get(tab.key);
      const node = panels.get(tab.key);
      if (!button || !node) {
        continue;
      }
      const selected = tab.key === key;
      setAttr(button, "aria-selected", selected ? "true" : "false");
      setAttr(button, "tabindex", selected ? "0" : "-1");
      node.hidden = !selected;
      if (selected && focus) {
        button.focus();
      }
    }
  }

  tablist.addEventListener("keydown", (event) => {
    const keys = TABS.map((tab) => tab.key);
    const index = keys.indexOf(activeTab);
    let next: TabKey | null = null;
    switch (event.key) {
      case "ArrowRight":
        next = keys[(index + 1) % keys.length];
        break;
      case "ArrowLeft":
        next = keys[(index - 1 + keys.length) % keys.length];
        break;
      case "Home":
        next = keys[0];
        break;
      case "End":
        next = keys[keys.length - 1];
        break;
      default:
        break;
    }
    if (next) {
      event.preventDefault();
      selectTab(next, true);
    }
  });
  for (const tab of TABS) {
    tabButtons.get(tab.key)?.addEventListener("click", () => selectTab(tab.key));
  }
  selectTab("files");

  close.addEventListener("click", () => options.onClose());
  toggle.addEventListener("click", () => {
    if (currentId !== null) {
      options.onPauseToggle(currentId);
    }
  });
  reveal.addEventListener("click", () => {
    if (currentId !== null) {
      options.onReveal(currentId);
    }
  });
  remove.addEventListener("click", () => {
    if (currentId !== null) {
      options.onRemove(currentId);
    }
  });

  // --- Трекеры: пояснение и обновление (1.7) ------------------------------
  function setTrackersNote(text: string, isError: boolean): void {
    setText(trackersNote, text);
    trackersNote.classList.toggle("is-error", isError);
    trackersNote.hidden = text === "";
  }

  function syncTrackersTools(): void {
    // Пустой список обновлять нечем: движок ответит ошибкой «нет трекеров».
    trackersRefresh.disabled = trackersBusy || currentTrackers.length === 0;
    trackersEdit.disabled = currentId === null;
  }

  trackersEdit.addEventListener("click", () => {
    if (currentId === null) {
      return;
    }
    options.onEditTrackers(currentId);
  });

  trackersRefresh.addEventListener("click", () => {
    const id = currentId;
    if (id === null || trackersBusy || currentTrackers.length === 0) {
      return;
    }
    trackersBusy = true;
    syncTrackersTools();
    setTrackersNote("Обновляем трекеры…", false);
    options
      .onReannounce(id)
      .then(() => {
        // Пока ждали ответа, могли выбрать другой торрент — не врём чужой карточке.
        if (currentId !== id) {
          return;
        }
        trackersBusy = false;
        syncTrackersTools();
        setTrackersNote("Трекеры обновлены", false);
      })
      .catch((reason: unknown) => {
        if (currentId !== id) {
          return;
        }
        trackersBusy = false;
        syncTrackersTools();
        setTrackersNote(messageOf(reason), true);
      });
  });

  // --- Последовательная загрузка (1.7) ------------------------------------
  function applySequential(value: boolean): void {
    sequentialShown = value;
    setAttr(sequentialSwitch, "aria-checked", value ? "true" : "false");
  }

  function setSequentialNote(text: string, isError: boolean): void {
    setText(sequentialNote, text);
    sequentialNote.classList.toggle("is-error", isError);
    sequentialNote.hidden = text === "";
  }

  sequentialSwitch.addEventListener("click", () => {
    const id = currentId;
    if (id === null || sequentialBusy) {
      return;
    }
    const enabled = sequentialShown !== true;
    // Оптимистично: переключатель отзывается сразу, а снимок движка догонит.
    applySequential(enabled);
    sequentialBusy = true;
    sequentialSwitch.disabled = true;
    setSequentialNote("", false);
    options
      .onSequentialChange(id, enabled)
      .then(() => {
        if (currentId !== id) {
          return;
        }
        sequentialBusy = false;
        sequentialSwitch.disabled = false;
      })
      .catch((reason: unknown) => {
        if (currentId !== id) {
          return;
        }
        sequentialBusy = false;
        sequentialSwitch.disabled = false;
        applySequential(!enabled);
        setSequentialNote(messageOf(reason), true);
      });
  });

  // --- Ключевые строки ---------------------------------------------------
  interface FileRow {
    el: HTMLElement;
    name: HTMLElement;
    meta: HTMLElement;
    size: HTMLElement;
    select: SelectControl;
    /** Текущий приоритет строки — источник правды до ответа движка. */
    priority: FilePriority;
  }
  const fileRows = new Map<string, FileRow>();
  const peerRows = new Map<
    string,
    { el: HTMLElement; addr: HTMLElement; meta: HTMLElement; progress: HTMLElement; down: HTMLElement; up: HTMLElement }
  >();
  const trackerRows = new Map<string, { el: HTMLElement; url: HTMLElement }>();

  function clearKeyed(): void {
    for (const row of fileRows.values()) {
      row.select.destroy();
    }
    fileRows.clear();
    peerRows.clear();
    trackerRows.clear();
    filesList.replaceChildren();
    peersList.replaceChildren();
    trackersList.replaceChildren();
    currentFiles = [];
    lastKnownPriorities = [];
    filesHead.hidden = true;
  }

  function createFileRow(path: string): FileRow {
    const fileName = h("span", { class: "file-row__name" });
    const fileMeta = h("span", { class: "file-row__meta" });
    const fileSize = h("span", { class: "file-row__size num" });
    const iconEl = h("span", { class: "file-row__icon" }, icon("file", 15));
    const mainEl = h("div", { class: "file-row__main" }, fileName, fileMeta);
    const el = h("div", {
      class: "file-row",
      attrs: { role: "listitem", "data-priority": "normal" },
    });
    const row: FileRow = {
      el,
      name: fileName,
      meta: fileMeta,
      size: fileSize,
      priority: "normal",
      select: createSelect({
        label: `Приоритет файла ${basename(path)}`,
        options: PRIORITY_OPTIONS,
        value: "normal",
        className: "file-row__priority",
        menuWidth: 176,
        onChange: (value) => {
          // Меняется только эта строка; движку уходит весь массив по порядку файлов.
          row.priority = value as FilePriority;
          setAttr(row.el, "data-priority", row.priority);
          commitFilePriorities();
        },
      }),
    };
    el.append(iconEl, mainEl, row.select.element, fileSize);
    return row;
  }

  function renderFiles(files: FileView[]): void {
    currentFiles = files;
    lastKnownPriorities = files.map((file) => file.priority);
    const seen = new Set<string>();
    let previous: HTMLElement | null = null;
    for (const file of files) {
      seen.add(file.path);
      let row = fileRows.get(file.path);
      if (!row) {
        row = createFileRow(file.path);
        fileRows.set(file.path, row);
      }
      setText(row.name, basename(file.path));
      setAttr(row.name, "title", file.path);
      setText(row.meta, file.downloaded >= file.length ? "готово" : `скачано ${fmtBytes(file.downloaded)}`);
      setText(row.size, fmtBytes(file.length));
      if (row.priority !== file.priority) {
        row.priority = file.priority;
        row.select.setValue(file.priority);
      }
      setAttr(row.el, "data-priority", file.priority);
      const reference: Node | null = previous ? previous.nextSibling : filesList.firstChild;
      if (reference !== row.el) {
        filesList.insertBefore(row.el, reference);
      }
      previous = row.el;
    }
    for (const [path, row] of fileRows) {
      if (!seen.has(path)) {
        row.select.destroy();
        row.el.remove();
        fileRows.delete(path);
      }
    }
    filesEmpty.hidden = files.length > 0;
    filesHead.hidden = files.length === 0;
    selectAllButton.disabled = files.length === 0;
    selectNoneButton.disabled = files.length === 0;
    refreshFilesSummary();
  }

  /** Набор файлов из последнего снимка движка — база для отката при ошибке. */
  let currentFiles: FileView[] = [];
  let lastKnownPriorities: FilePriority[] = [];

  function refreshFilesSummary(): void {
    let count = 0;
    let bytes = 0;
    for (const file of currentFiles) {
      const row = fileRows.get(file.path);
      if (row && row.priority !== "skip") {
        count += 1;
        bytes += file.length;
      }
    }
    setText(filesSummary, `Скачивается ${count} из ${currentFiles.length} · ${fmtBytes(bytes)}`);
  }

  function restoreFilePriorities(values: FilePriority[]): void {
    currentFiles.forEach((file, index) => {
      const row = fileRows.get(file.path);
      const value = values[index] ?? file.priority;
      if (row && row.priority !== value) {
        row.priority = value;
        row.select.setValue(value);
        setAttr(row.el, "data-priority", value);
      }
    });
    refreshFilesSummary();
  }

  function commitFilePriorities(): void {
    const id = currentId;
    refreshFilesSummary();
    if (id === null) {
      return;
    }
    // Движок принимает только полный набор приоритетов в порядке файлов.
    const priorities = currentFiles.map((file) => fileRows.get(file.path)?.priority ?? file.priority);
    void options
      .onFilePriorities(id, priorities)
      .catch(() => restoreFilePriorities(lastKnownPriorities));
  }

  function applyPriority(priority: FilePriority): void {
    for (const row of fileRows.values()) {
      row.priority = priority;
      row.select.setValue(priority);
      setAttr(row.el, "data-priority", priority);
    }
    commitFilePriorities();
  }

  selectAllButton.addEventListener("click", () => applyPriority("normal"));
  selectNoneButton.addEventListener("click", () => applyPriority("skip"));

  function renderTrackers(trackers: string[]): void {
    currentTrackers = trackers;
    const seen = new Set<string>();
    let previous: HTMLElement | null = null;
    for (const url of trackers) {
      seen.add(url);
      let row = trackerRows.get(url);
      if (!row) {
        const urlEl = h("span", { class: "tracker-row__url" });
        const el = h("div", { class: "tracker-row", attrs: { role: "listitem" } }, urlEl);
        if (/^https?:/i.test(url)) {
          const open = h("button", {
            class: "icon-btn icon-btn--sm",
            attrs: { type: "button", "aria-label": "Открыть трекер в браузере", title: "Открыть в браузере" },
          });
          open.append(icon("external", 14));
          open.addEventListener("click", () => options.onOpenExternal(url));
          el.append(open);
        }
        row = { el, url: urlEl };
        trackerRows.set(url, row);
      }
      setText(row.url, url);
      setAttr(row.url, "title", url);
      const reference: Node | null = previous ? previous.nextSibling : trackersList.firstChild;
      if (reference !== row.el) {
        trackersList.insertBefore(row.el, reference);
      }
      previous = row.el;
    }
    for (const [url, row] of trackerRows) {
      if (!seen.has(url)) {
        row.el.remove();
        trackerRows.delete(url);
      }
    }
    trackersEmpty.hidden = trackers.length > 0;
    syncTrackersTools();
  }

  function renderPeers(torrent: TorrentSnapshot): void {
    const peers = torrent.peers;
    const seen = new Set<string>();
    let previous: HTMLElement | null = null;
    for (const peer of peers) {
      seen.add(peer.addr);
      let row = peerRows.get(peer.addr);
      if (!row) {
        const addr = h("span", { class: "peer-row__addr num" });
        const metaEl = h("span", { class: "peer-row__meta" });
        const progressEl = h("span", { class: "peer-row__progress num" });
        const downEl = h("span", { class: "peer-row__rate num" });
        const upEl = h("span", { class: "peer-row__rate num" });
        const mainEl = h("div", { class: "peer-row__main" }, addr, metaEl);
        const rates = h("div", { class: "peer-row__rates" }, downEl, upEl);
        const el = h("div", { class: "peer-row", attrs: { role: "listitem" } }, mainEl, progressEl, rates);
        row = { el, addr, meta: metaEl, progress: progressEl, down: downEl, up: upEl };
        peerRows.set(peer.addr, row);
      }
      setText(row.addr, peer.addr);
      setText(row.meta, `${peer.client}${peer.is_seed ? " · сид" : ""}`);
      setText(row.progress, fmtPercent(peer.progress));
      setText(row.down, `↓ ${fmtRate(peer.down_rate)}`);
      setText(row.up, `↑ ${fmtRate(peer.up_rate)}`);
      const reference: Node | null = previous ? previous.nextSibling : peersList.firstChild;
      if (reference !== row.el) {
        peersList.insertBefore(row.el, reference);
      }
      previous = row.el;
    }
    for (const [addr, row] of peerRows) {
      if (!seen.has(addr)) {
        row.el.remove();
        peerRows.delete(addr);
      }
    }
    peersEmpty.hidden = peers.length > 0;
    const missing = torrent.peers_connected - peers.length;
    peersNote.hidden = peers.length === 0 || missing <= 0;
    if (!peersNote.hidden) {
      setText(peersNote, `Показаны ${peers.length} из ${torrent.peers_connected} подключённых`);
    }
  }

  // --- Карта кусков ------------------------------------------------------
  const orderCache = new Map<string, Uint32Array>();
  let bucketCells: HTMLElement[] = [];
  let bucketStateKey = "";
  let bucketClasses: string[] = [];

  function pieceOrder(infoHash: string, pieceCount: number): Uint32Array {
    const key = `${infoHash}:${pieceCount}`;
    const cached = orderCache.get(key);
    if (cached) {
      return cached;
    }
    if (orderCache.size > 4) {
      orderCache.clear();
    }
    const order = new Uint32Array(pieceCount);
    const values = new Uint32Array(pieceCount);
    let state = hashString(infoHash) || 1;
    for (let index = 0; index < pieceCount; index += 1) {
      order[index] = index;
      state ^= state << 13;
      state >>>= 0;
      state ^= state >>> 17;
      state ^= state << 5;
      state >>>= 0;
      values[index] = state;
    }
    order.sort((left, right) => values[left] - values[right]);
    orderCache.set(key, order);
    return order;
  }

  function renderPieces(torrent: TorrentSnapshot): void {
    const hasPieces = torrent.piece_count > 0;
    piecesMap.hidden = !hasPieces;
    piecesEmpty.hidden = hasPieces;
    legend.hidden = !hasPieces;
    if (!hasPieces) {
      clearPieces();
      setText(
        piecesSummary,
        torrent.state === "metadata" ? "Ожидание метаданных" : "Нет данных о кусках",
      );
      return;
    }

    const bucketCount = Math.min(PIECE_MAX_BUCKETS, torrent.piece_count);
    const bucketSize = Math.ceil(torrent.piece_count / bucketCount);
    const actualBuckets = Math.ceil(torrent.piece_count / bucketSize);
    const order = pieceOrder(torrent.info_hash, torrent.piece_count);
    const counts = new Array<number>(actualBuckets).fill(0);
    const sizes = new Array<number>(actualBuckets).fill(0);
    for (let bucket = 0; bucket < actualBuckets; bucket += 1) {
      sizes[bucket] = Math.min(bucketSize, torrent.piece_count - bucket * bucketSize);
    }
    for (let rank = 0; rank < torrent.pieces_have && rank < order.length; rank += 1) {
      const bucket = Math.floor(order[rank] / bucketSize);
      if (bucket >= 0 && bucket < actualBuckets) {
        counts[bucket] += 1;
      }
    }

    ensureBuckets(actualBuckets);
    const classes: string[] = [];
    for (let bucket = 0; bucket < actualBuckets; bucket += 1) {
      const ratioValue = sizes[bucket] > 0 ? counts[bucket] / sizes[bucket] : 0;
      const cls = ratioValue >= 1 ? "full" : ratioValue > 0 ? "part" : "none";
      classes.push(cls);
      const cell = bucketCells[bucket];
      if (!cell) {
        continue;
      }
      if (bucketClasses[bucket] !== cls) {
        cell.className = `pieces__cell pieces__cell--${cls}`;
      }
      const from = bucket * bucketSize + 1;
      const to = bucket * bucketSize + sizes[bucket];
      const title = `Куски ${fmtCount(from)}–${fmtCount(to)}: ${fmtCount(counts[bucket])} из ${fmtCount(sizes[bucket])}`;
      if (cell.title !== title) {
        cell.title = title;
      }
    }
    bucketClasses = classes;
    setAttr(
      piecesMap,
      "aria-label",
      `Карта кусков: скачано ${fmtCount(torrent.pieces_have)} из ${fmtCount(torrent.piece_count)}`,
    );
    setText(
      piecesSummary,
      `${fmtCount(torrent.pieces_have)} из ${fmtCount(torrent.piece_count)} кусков · ${fmtBytes(
        torrent.piece_length,
      )} на кусок`,
    );
  }

  function ensureBuckets(count: number): void {
    const key = `${count}`;
    if (bucketStateKey === key) {
      return;
    }
    bucketStateKey = key;
    bucketClasses = [];
    bucketCells = [];
    const nodes: HTMLElement[] = [];
    for (let index = 0; index < count; index += 1) {
      const cell = h("i", { class: "pieces__cell pieces__cell--none" });
      bucketCells.push(cell);
      nodes.push(cell);
    }
    piecesMap.replaceChildren(...nodes);
  }

  function clearPieces(): void {
    bucketCells = [];
    bucketClasses = [];
    bucketStateKey = "";
    piecesMap.replaceChildren();
  }

  function update(state: AppState): void {
    const payload = state.payload;
    const torrent = payload && state.selectedId !== null
      ? payload.torrents.find((item) => item.id === state.selectedId) ?? null
      : null;
    mount.hidden = torrent === null || !state.detailsOpen;
    if (!torrent) {
      currentId = null;
      sequentialShown = null;
      return;
    }

    if (torrent.id !== currentId) {
      currentId = torrent.id;
      clearKeyed();
      clearPieces();
      badgeState = null;
      toggleState = null;
      // Ответы на команды прошлого торрента больше не наши (1.7).
      trackersBusy = false;
      setTrackersNote("", false);
      sequentialBusy = false;
      sequentialSwitch.disabled = false;
      setSequentialNote("", false);
    }

    // Переключатель опирается на снимок, пока пользователь его не тронул (1.7).
    // `=== true` — на случай старого движка без поля sequential.
    const sequentialSnapshot = torrent.sequential === true;
    if (sequentialShown === null || sequentialShown === sequentialSnapshot) {
      applySequential(sequentialSnapshot);
    }

    setText(name, torrent.name);
    setAttr(name, "title", torrent.name);
    const badgeMeta = BADGES[torrent.state];
    setText(badge, badgeMeta.label);
    if (badgeState !== torrent.state) {
      badgeState = torrent.state;
      badge.className = `badge badge--${badgeMeta.cls}`;
    }
    setText(hash, truncateMiddle(torrent.info_hash, 22));
    setAttr(hash, "title", torrent.info_hash);
    if (torrent.state === "error" && torrent.error) {
      error.hidden = false;
      setText(error, torrent.error);
    } else {
      error.hidden = true;
    }
    // Лимит раздачи (1.6): отдельная строка вместо ошибки — движок сам остановил
    // раздачу, и снять это можно только кнопкой «Продолжить»/«Раздать».
    limit.hidden = !torrent.seed_limit_reached;
    if (torrent.seed_limit_reached) {
      setText(limit, "Достигнут лимит раздачи — возобновите вручную");
    }

    const actionState = toggleLabel(torrent.state);
    if (toggleState !== torrent.state) {
      toggleState = torrent.state;
      toggleIcon.replaceChildren(icon(actionState.icon, 15));
      setText(toggleLabelEl, actionState.label);
      setAttr(toggle, "aria-label", `${actionState.label}: ${torrent.name}`);
    }
    // Во время перепроверки движок не принимает паузу (1.3).
    toggle.disabled = torrent.state === "checking";

    setScaleX(fill, torrent.progress, scale);
    setText(percent, fmtPercent(torrent.progress));
    setText(progressNote, progressText(torrent));
    setAttr(bar, "aria-valuenow", String(Math.round(torrent.progress * 100)));

    const remaining = Math.max(0, torrent.wanted_bytes - torrent.downloaded_bytes);
    // Все файлы исключены (1.3): качать нечего, поэтому «осталось 0» — это ещё не «завершено».
    const allSkipped = torrent.wanted_bytes === 0 && torrent.files.length > 0;
    setText(downloaded.value, fmtBytes(torrent.downloaded_bytes));
    setText(
      downloaded.sub,
      torrent.wanted_bytes > 0
        ? `из ${fmtBytes(torrent.wanted_bytes)}`
        : torrent.total_bytes > 0
          ? // Все файлы исключены: размер известен, но скачивать нечего.
            `из ${fmtBytes(torrent.total_bytes)}`
          : "размер неизвестен",
    );
    setText(uploaded.value, fmtBytes(torrent.uploaded_bytes));
    setText(uploaded.sub, "за сессию");
    // Ratio из снимка (1.6): если ещё ничего не скачано, движок отдаёт 0.0 —
    // показываем прочерк, а не «×0,00».
    setText(ratio.value, torrent.downloaded_bytes > 0 ? seedRatioText(torrent.seed_ratio) : "—");
    setText(ratio.sub, "Отдано/скачано");
    setText(left.value, remaining > 0 ? fmtBytes(remaining) : "—");
    setText(
      left.sub,
      remaining > 0
        ? torrent.eta_seconds !== null
          ? fmtEta(torrent.eta_seconds)
          : "осталось неизвестно"
        : allSkipped
          ? "файлы не выбраны"
          : "завершено",
    );

    renderFiles(torrent.files);
    renderTrackers(torrent.trackers);
    renderPeers(torrent);
    renderPieces(torrent);

    const filesCount = tabCounts.get("files");
    if (filesCount) {
      setText(filesCount, String(torrent.files.length));
      filesCount.hidden = torrent.files.length === 0;
    }
    const trackersCount = tabCounts.get("trackers");
    if (trackersCount) {
      setText(trackersCount, String(torrent.trackers.length));
      trackersCount.hidden = torrent.trackers.length === 0;
    }
    const peersCount = tabCounts.get("peers");
    if (peersCount) {
      setText(peersCount, String(torrent.peers_connected));
      peersCount.hidden = torrent.peers_connected === 0;
    }
  }

  selectTab(activeTab);

  return { update };
}
