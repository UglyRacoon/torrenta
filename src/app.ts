import type { EngineApi } from "./api/engine";
import { createEngineApi, isTauriRuntime } from "./api/engine";
import type { AppPayload, FilterKey, SortKey, TorrentInput, TorrentSnapshot, TorrentState } from "./api/types";
import { messageOf } from "./lib/dom";
import { openMenu } from "./ui/menu";
import { createDetails, type DetailsPanel } from "./ui/details";
import { createList, type TorrentList } from "./ui/list";
import { createModalHost } from "./ui/modals";
import { createSidebar } from "./ui/sidebar";
import { createStatusbar } from "./ui/statusbar";
import { createToaster } from "./ui/toasts";
import { AppStore, type AppState, type Theme } from "./state/store";
import { activeCount, findTorrent, isActiveState } from "./state/view";

const THEME_KEY = "torrenta:theme";
const DARK_THEME_COLOR = "#0f1319";
const LIGHT_THEME_COLOR = "#f2f5f9";

function must<T extends Element>(selector: string): T {
  const node = document.querySelector<T>(selector);
  if (!node) {
    throw new Error(`Не найден элемент ${selector}`);
  }
  return node;
}

function readStoredTheme(): Theme | null {
  try {
    const value = window.localStorage.getItem(THEME_KEY);
    return value === "dark" || value === "light" ? value : null;
  } catch {
    return null;
  }
}

function persistTheme(theme: Theme): void {
  try {
    window.localStorage.setItem(THEME_KEY, theme);
  } catch {
    // Приватный режим без localStorage — тема просто не запомнится.
  }
}

function applyTheme(theme: Theme): void {
  const root = document.documentElement;
  root.dataset.theme = theme;
  root.style.colorScheme = theme;
  document
    .querySelector('meta[name="theme-color"]')
    ?.setAttribute("content", theme === "dark" ? DARK_THEME_COLOR : LIGHT_THEME_COLOR);
}

function isMagnet(link: string): boolean {
  return /^magnet:\?/i.test(link.trim());
}

export function mountApp(): void {
  const appEl = must<HTMLElement>("#app");
  const sidebarMount = must<HTMLElement>("#sidebar");
  const listMount = must<HTMLElement>("#list");
  const detailsMount = must<HTMLElement>("#details");
  const statusbarMount = must<HTMLElement>("#statusbar");
  const toastMount = must<HTMLElement>("#toasts");
  const modalRoot = must<HTMLElement>("#modal-root");
  const dropOverlay = must<HTMLElement>("#drop-overlay");

  const initialTheme = readStoredTheme() ?? "dark";
  applyTheme(initialTheme);

  const store = new AppStore({
    mode: isTauriRuntime() ? "tauri" : "mock",
    payload: null,
    ready: false,
    fatalError: null,
    filter: "all",
    query: "",
    sort: "added",
    selectedId: null,
    theme: initialTheme,
  });

  const toaster = createToaster(toastMount);
  const modals = createModalHost(modalRoot);

  let engine: EngineApi | null = null;
  let unsubscribe: (() => void) | null = null;
  let stopDrop: (() => void) | null = null;
  let tornDown = false;

  const api = (): EngineApi => {
    if (!engine) {
      throw new Error("Движок ещё подключается");
    }
    return engine;
  };

  const fail = (reason: unknown, title = "Не удалось выполнить"): void => {
    toaster.push("error", title, messageOf(reason));
  };

  const state = (): AppState => store.state;

  function setDetailsOpen(open: boolean): void {
    appEl.dataset.details = open ? "open" : "closed";
  }

  async function pickFiles(): Promise<void> {
    try {
      const inputs = await api().pickTorrentFiles();
      if (inputs.length === 0) {
        return;
      }
      await addInputs(inputs);
    } catch (reason) {
      fail(reason, "Не удалось открыть файл");
    }
  }

  async function addInputs(inputs: TorrentInput[]): Promise<void> {
    const engineApi = api();
    let added = 0;
    const problems: string[] = [];
    for (const input of inputs) {
      try {
        await engineApi.addTorrent(input);
        added += 1;
      } catch (reason) {
        problems.push(messageOf(reason));
      }
    }
    if (added > 0) {
      toaster.push("success", added === 1 ? "Торрент добавлен" : `Добавлено: ${added}`, "Загрузка начнётся автоматически");
    }
    for (const problem of problems) {
      toaster.push("error", "Торрент не добавлен", problem);
    }
  }

  async function readFiles(files: File[]): Promise<void> {
    const inputs: TorrentInput[] = [];
    for (const file of files) {
      const buffer = new Uint8Array(await file.arrayBuffer());
      inputs.push({ kind: "bytes", bytes: buffer, name: file.name });
    }
    await addInputs(inputs);
  }

  async function addMagnet(link: string): Promise<void> {
    const result = await api().addMagnet(link);
    toaster.push("success", "Magnet добавлен", result.name);
  }

  function openAdd(prefill?: string): void {
    modals.openAdd({
      prefill,
      onAddMagnet: (link) => addMagnet(link),
      onFiles: (files) => readFiles(files),
      onPick: pickFiles,
    });
  }

  async function openSettings(): Promise<void> {
    try {
      const settings = await api().getSettings();
      modals.openSettings(settings, {
        onSave: async (next) => {
          const saved = await api().saveSettings(next);
          const theme: Theme = saved.theme === "light" ? "light" : "dark";
          if (theme !== state().theme) {
            applyTheme(theme);
            persistTheme(theme);
            store.patch({ theme });
          }
          toaster.push("success", "Настройки сохранены");
        },
        onBrowseDir: () => api().pickDownloadDir(),
      });
    } catch (reason) {
      fail(reason, "Настройки недоступны");
    }
  }

  function toggleTheme(): void {
    const next: Theme = state().theme === "dark" ? "light" : "dark";
    applyTheme(next);
    persistTheme(next);
    store.patch({ theme: next });
    const payload = state().payload;
    if (payload) {
      void api()
        .saveSettings({ ...payload.settings, theme: next })
        .catch(() => {
          // Тема применена локально; отказ движка не должен ломать переключатель.
        });
    }
  }

  async function togglePause(id: number): Promise<void> {
    const torrent = findTorrent(state(), id);
    if (!torrent) {
      return;
    }
    try {
      if (isActiveState(torrent.state)) {
        await api().pause(id);
      } else {
        await api().resume(id);
      }
    } catch (reason) {
      fail(reason, "Не удалось изменить состояние");
    }
  }

  async function toggleAll(): Promise<void> {
    const payload = state().payload;
    if (!payload) {
      return;
    }
    const running = activeCount(payload.torrents) > 0;
    try {
      if (running) {
        await api().pauseAll();
        toaster.push("info", "Все загрузки приостановлены");
      } else {
        await api().resumeAll();
        toaster.push("info", "Все загрузки возобновлены");
      }
    } catch (reason) {
      fail(reason);
    }
  }

  function removeTorrent(id: number): void {
    const torrent = findTorrent(state(), id);
    if (!torrent) {
      return;
    }
    modals.confirmRemove(torrent.name, async (deleteFiles) => {
      await api().remove(id, deleteFiles);
      toaster.push("success", "Торрент удалён", torrent.name);
    });
  }

  function revealTorrent(id: number): void {
    void api()
      .revealTorrent(id)
      .catch((reason: unknown) => fail(reason, "Не удалось открыть каталог"));
  }

  function openDownloadDir(): void {
    void api()
      .openDownloadDir()
      .catch((reason: unknown) => fail(reason, "Не удалось открыть каталог"));
  }

  function openExternal(url: string): void {
    void api()
      .openExternal(url)
      .catch((reason: unknown) => fail(reason, "Ссылка недоступна"));
  }

  function contextMenu(torrent: TorrentSnapshot, anchor: HTMLElement): void {
    const active = isActiveState(torrent.state);
    openMenu(anchor, [
      {
        label: active ? "Пауза" : torrent.state === "completed" ? "Раздать" : "Продолжить",
        icon: active ? "pause" : "play",
        onSelect: () => void togglePause(torrent.id),
      },
      {
        label: "Показать в проводнике",
        icon: "folderOpen",
        separatorBefore: true,
        onSelect: () => revealTorrent(torrent.id),
      },
      {
        label: "Открыть каталог загрузок",
        icon: "folder",
        onSelect: openDownloadDir,
      },
      {
        label: "Удалить",
        icon: "trash",
        danger: true,
        separatorBefore: true,
        onSelect: () => removeTorrent(torrent.id),
      },
    ]);
  }

  async function pasteFromClipboard(): Promise<void> {
    let text = "";
    try {
      text = (await navigator.clipboard.readText()).trim();
    } catch {
      openAdd();
      return;
    }
    if (isMagnet(text)) {
      openAdd(text);
    } else {
      openAdd();
    }
  }

  const sidebar = createSidebar(sidebarMount, {
    onFilter: (filter: FilterKey) => store.patch({ filter }),
    onToggleTheme: toggleTheme,
    onOpenSettings: () => void openSettings(),
    onToggleAll: () => void toggleAll(),
  });

  const list: TorrentList = createList(listMount, {
    onSelect: (id) => {
      store.patch({ selectedId: id });
      setDetailsOpen(true);
    },
    onPauseToggle: (id) => void togglePause(id),
    onContextMenu: (id, anchor) => {
      const torrent = findTorrent(state(), id);
      if (torrent) {
        contextMenu(torrent, anchor);
      }
    },
    onAdd: () => openAdd(),
    onQuery: (query) => store.patch({ query }),
    onSort: (sort: SortKey) => store.patch({ sort }),
    onClearFilters: () => store.patch({ filter: "all", query: "" }),
    onRetry: () => void bootstrap(),
  });

  const details: DetailsPanel = createDetails(detailsMount, {
    onClose: () => {
      store.patch({ selectedId: null });
      setDetailsOpen(false);
    },
    onPauseToggle: (id) => void togglePause(id),
    onReveal: (id) => revealTorrent(id),
    onRemove: (id) => removeTorrent(id),
    onOpenExternal: (url) => openExternal(url),
  });

  const statusbar = createStatusbar(statusbarMount);

  // Переходы состояний для уведомлений: первый payload — не событие.
  let previousStates = new Map<number, TorrentState>();
  let notifiedPayload: AppPayload | null = null;

  function notifyTransitions(payload: AppPayload): void {
    if (notifiedPayload === payload) {
      return;
    }
    const next = new Map<number, TorrentState>();
    for (const torrent of payload.torrents) {
      next.set(torrent.id, torrent.state);
      const before = previousStates.get(torrent.id);
      if (before === undefined || before === torrent.state) {
        continue;
      }
      if (torrent.state === "completed") {
        toaster.push("success", "Загрузка завершена", torrent.name);
      } else if (torrent.state === "error") {
        toaster.push("error", "Ошибка загрузки", torrent.error ?? torrent.name);
      }
    }
    previousStates = next;
    notifiedPayload = payload;
  }

  store.subscribe((current) => {
    sidebar.update(current);
    list.update(current);
    details.update(current);
    statusbar.update(current);
    setDetailsOpen(current.selectedId !== null);
    if (current.payload) {
      notifyTransitions(current.payload);
    }
  });

  document.addEventListener("keydown", (event) => {
    if (modals.isOpen()) {
      return;
    }
    const mod = event.ctrlKey || event.metaKey;
    const key = event.key.toLowerCase();
    if (mod && key === "o") {
      event.preventDefault();
      void pickFiles();
      return;
    }
    if (mod && event.key === ",") {
      event.preventDefault();
      void openSettings();
      return;
    }
    if (mod && key === "v") {
      const target = event.target;
      if (target instanceof HTMLElement && (target.isContentEditable || /^(input|textarea)$/i.test(target.tagName))) {
        return;
      }
      void pasteFromClipboard();
      return;
    }
    if (event.key === "/" && !mod) {
      const target = event.target;
      if (target instanceof HTMLElement && /^(input|textarea|select)$/i.test(target.tagName)) {
        return;
      }
      event.preventDefault();
      list.focusSearch();
      return;
    }
    if (event.key === "Escape" && state().selectedId !== null) {
      store.patch({ selectedId: null });
      setDetailsOpen(false);
    }
  });

  let bootstrapAttempts = 0;

  async function bootstrap(): Promise<void> {
    bootstrapAttempts += 1;
    if (!engine) {
      try {
        engine = await createEngineApi();
        store.patch({ mode: engine.mode });
      } catch (reason) {
        store.patch({ fatalError: `Не удалось подключить движок: ${messageOf(reason)}` });
        return;
      }
    }
    const engineApi = engine;
    try {
      const payload = await engineApi.init();
      store.hydrate(payload);
      if (unsubscribe) {
        unsubscribe();
        unsubscribe = null;
      }
      unsubscribe = await engineApi.subscribe((next) => {
        if (!tornDown) {
          store.setPayload(next);
        }
      });
      if (!stopDrop) {
        stopDrop = engineApi.bindFileDrop({
          onEnter: () => {
            dropOverlay.hidden = false;
          },
          onLeave: () => {
            dropOverlay.hidden = true;
          },
          onDrop: (inputs) => {
            dropOverlay.hidden = true;
            void addInputs(inputs);
          },
        });
      }
      if (store.state.selectedId === null && payload.torrents.length > 0) {
        store.patch({ selectedId: payload.torrents[0].id });
      }
    } catch (reason) {
      store.patch({ fatalError: `Движок не ответил: ${messageOf(reason)}` });
      if (bootstrapAttempts === 1) {
        toaster.push("error", "Нет связи с движком", messageOf(reason));
      }
    }
  }

  window.addEventListener("beforeunload", () => {
    tornDown = true;
    unsubscribe?.();
    stopDrop?.();
  });

  void bootstrap();
}
