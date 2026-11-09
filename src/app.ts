import type { EngineApi } from "./api/engine";
import { createEngineApi, isTauriRuntime } from "./api/engine";
import type {
  AppPayload,
  AutoShutdownState,
  FilePriority,
  FilterKey,
  SettingsDto,
  SortKey,
  TorrentSnapshot,
  TorrentSource,
  TorrentState,
} from "./api/types";
import { NO_LIMIT, SPEED_STEPS } from "./api/types";
import { messageOf } from "./lib/dom";
import { closeMenu, openMenu, type MenuItem } from "./ui/menu";
import { createDetails, type DetailsPanel } from "./ui/details";
import { createList, type ContextAnchor, type TorrentList } from "./ui/list";
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

function isMagnetLink(value: string): boolean {
  return /^magnet:\?/i.test(value.trim());
}

/** Внешний источник приходит строкой: либо magnet-ссылка, либо путь к .torrent (1.4). */
function sourceFromText(text: string): TorrentSource {
  return isMagnetLink(text)
    ? { kind: "magnet", link: text.trim() }
    : { kind: "path", path: text };
}

/** Ступени ограничения скорости — общие для меню торрента (0 = без ограничения). */
function limitItems(current: number, apply: (value: number) => void): MenuItem[] {
  const items: MenuItem[] = SPEED_STEPS.map((step) => ({
    label: step === NO_LIMIT ? "Без ограничения" : `${step} КиБ/с`,
    checked: current === step,
    onSelect: () => apply(step),
  }));
  if (!SPEED_STEPS.includes(current)) {
    items.push({ label: `${current} КиБ/с`, checked: true, onSelect: () => apply(current) });
  }
  return items;
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
    detailsOpen: false,
    theme: initialTheme,
  });

  const toaster = createToaster(toastMount);
  const modals = createModalHost(modalRoot);

  let engine: EngineApi | null = null;
  let unsubscribe: (() => void) | null = null;
  let unsubscribeAuto: (() => void) | null = null;
  let unsubscribeOpen: (() => void) | null = null;
  let stopDrop: (() => void) | null = null;
  let tornDown = false;
  let autoState: AutoShutdownState = { enabled: false, pending_seconds: null };

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

  // --- Добавление торрентов (контракт 4.1) --------------------------------

  async function openAdd(source: TorrentSource | null = null): Promise<void> {
    let savePath = "";
    try {
      savePath = (await api().getSettings()).download_dir;
    } catch {
      // Каталог неизвестен — движок возьмёт значение по умолчанию.
    }
    modals.openAdd(
      { source, savePath },
      {
        onPreview: (src) => api().preview(src),
        onAdd: async (src, options) => {
          const result = await api().addTorrent(src, options);
          store.patch({ selectedId: result.id });
          toaster.push("success", "Торрент добавлен", result.name);
        },
        onPickSource: async () => {
          try {
            const sources = await api().pickTorrentFiles();
            return sources[0] ?? null;
          } catch (reason) {
            fail(reason, "Не удалось открыть файл");
            return null;
          }
        },
        onBrowseDir: () => api().pickDownloadDir(),
      },
    );
  }

  /** Несколько источников сразу (мультивыбор в диалоге / drag&drop): первый — в диалог. */
  async function addSources(sources: TorrentSource[]): Promise<void> {
    if (sources.length === 0) {
      return;
    }
    if (sources.length === 1) {
      const only = sources[0];
      if (only) {
        await openAdd(only);
      }
      return;
    }
    let added = 0;
    const problems: string[] = [];
    for (const source of sources) {
      try {
        await api().addTorrent(source, { savePath: null, files: null });
        added += 1;
      } catch (reason) {
        problems.push(messageOf(reason));
      }
    }
    if (added > 0) {
      toaster.push(
        "success",
        added === 1 ? "Торрент добавлен" : `Добавлено: ${added}`,
        "Загрузка начнётся автоматически",
      );
    }
    for (const problem of problems) {
      toaster.push("error", "Торрент не добавлен", problem);
    }
  }

  async function pasteFromClipboard(): Promise<void> {
    let text = "";
    try {
      text = (await navigator.clipboard.readText()).trim();
    } catch {
      await openAdd();
      return;
    }
    await openAdd(isMagnetLink(text) ? { kind: "magnet", link: text } : null);
  }

  // --- Внешние источники и выход (1.4) -------------------------------------

  /**
   * .torrent/magnet, открытый из проводника (или переданный вторым экземпляром):
   * забираем источник один раз и показываем обычный диалог добавления.
   */
  async function consumeOpenRequest(): Promise<void> {
    if (tornDown) {
      return;
    }
    try {
      const raw = await api().takeOpenRequest();
      if (!raw || tornDown) {
        return;
      }
      await openAdd(sourceFromText(raw));
    } catch (reason) {
      fail(reason, "Не удалось открыть торрент");
    }
  }

  /** Полное завершение процесса (кнопка «Выход» в сайдбаре). */
  async function quitApp(): Promise<void> {
    try {
      await api().quitApp();
    } catch (reason) {
      fail(reason, "Не удалось завершить приложение");
    }
  }

  // --- Настройки и тема ---------------------------------------------------

  async function openSettings(): Promise<void> {
    try {
      // set_settings — полная замена DTO: всегда отправляем все поля,
      // а auto_shutdown берём из живого состояния, чтобы не снять таймер.
      const settings: SettingsDto = { ...(await api().getSettings()), auto_shutdown: autoState.enabled };
      modals.openSettings(settings, {
        onSave: async (next) => {
          next.auto_shutdown = autoState.enabled;
          const saved = await api().saveSettings(next);
          // set_settings возвращает актуальный DTO: сразу обновляем настройки
          // в сторе, чтобы новые поля (1.5) не ждали следующего тика движка.
          const payload = state().payload;
          if (payload) {
            store.patch({ payload: { ...payload, settings: saved } });
          }
          const theme: Theme = saved.theme === "light" ? "light" : "dark";
          if (theme !== state().theme) {
            applyTheme(theme);
            persistTheme(theme);
            store.patch({ theme });
          }
          toaster.push("success", "Настройки сохранены");
        },
        onBrowseDir: () => api().pickDownloadDir(),
        onPickWatchDir: () => api().pickWatchDir(),
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
        .saveSettings({ ...payload.settings, auto_shutdown: autoState.enabled, theme: next })
        .catch(() => {
          // Тема применена локально; отказ движка не должен ломать переключатель.
        });
    }
  }

  // --- Автовыключение (контракт 4.2) --------------------------------------

  function applyAutoShutdown(next: AutoShutdownState): void {
    autoState = next;
    statusbar.setAutoShutdown(next);
  }

  async function toggleAutoShutdown(enabled: boolean): Promise<void> {
    try {
      const next = await api().setAutoShutdown(enabled);
      applyAutoShutdown(next);
      toaster.push(
        "info",
        enabled ? "Автовыключение включено" : "Автовыключение выключено",
        enabled ? "Компьютер выключится после завершения всех загрузок" : "Компьютер не будет выключен",
      );
    } catch (reason) {
      fail(reason, "Не удалось изменить автовыключение");
    }
  }

  async function cancelAutoShutdown(): Promise<void> {
    try {
      applyAutoShutdown(await api().cancelAutoShutdown());
      toaster.push("info", "Выключение отменено", "Компьютер останется включённым");
    } catch (reason) {
      fail(reason, "Не удалось отменить выключение");
    }
  }

  // --- Действия над торрентом ---------------------------------------------

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

  function openDetails(id: number): void {
    if (state().selectedId !== id || !state().detailsOpen) {
      store.patch({ selectedId: id, detailsOpen: true });
    }
  }

  function removeTorrent(id: number): void {
    const torrent = findTorrent(state(), id);
    if (!torrent) {
      return;
    }
    modals.confirmRemove(torrent.name, async (deleteFiles) => {
      await api().remove(id, deleteFiles);
      toaster.push("success", "Торрент удалён", deleteFiles ? `${torrent.name} — вместе с файлами` : torrent.name);
    });
  }

  function revealTorrent(id: number): void {
    void api()
      .revealTorrent(id)
      .catch((reason: unknown) => fail(reason, "Не удалось открыть каталог"));
  }

  function openExternal(url: string): void {
    void api()
      .openExternal(url)
      .catch((reason: unknown) => fail(reason, "Ссылка недоступна"));
  }

  async function setLimits(id: number, downKib: number, upKib: number): Promise<void> {
    try {
      await api().setTorrentLimits(id, downKib, upKib);
    } catch (reason) {
      fail(reason, "Не удалось изменить ограничение скорости");
    }
  }

  function recheckTorrent(id: number): void {
    void api()
      .recheckTorrent(id)
      .catch((reason: unknown) => fail(reason, "Не удалось перепроверить файлы"));
  }

  async function changeFilePriorities(id: number, priorities: FilePriority[]): Promise<void> {
    try {
      await api().setFilePriorities(id, priorities);
    } catch (reason) {
      fail(reason, "Не удалось изменить приоритет файлов");
      throw reason;
    }
  }

  function contextMenu(torrent: TorrentSnapshot, anchor: ContextAnchor): void {
    const checking = torrent.state === "checking";
    // Во время перепроверки торрент не на паузе, поэтому пункт остаётся «Пауза» —
    // просто недоступный: движок не принимает паузу во время проверки.
    const active = isActiveState(torrent.state) || checking;
    openMenu(anchor, [
      {
        label: active ? "Пауза" : "Возобновить",
        icon: active ? "pause" : "play",
        disabled: checking,
        onSelect: () => void togglePause(torrent.id),
      },
      {
        label: "Подробности",
        icon: "info",
        onSelect: () => openDetails(torrent.id),
      },
      {
        label: "Перепроверить файлы",
        icon: "refresh",
        disabled: checking,
        onSelect: () => recheckTorrent(torrent.id),
      },
      {
        label: "Ограничить скорость",
        icon: "clock",
        separatorBefore: true,
        children: limitItems(torrent.down_limit_kib, (value) => {
          void setLimits(torrent.id, value, torrent.up_limit_kib);
        }),
      },
      {
        label: "Открыть папку торрента",
        icon: "folderOpen",
        separatorBefore: true,
        onSelect: () => revealTorrent(torrent.id),
      },
      {
        label: "Удалить торрент",
        icon: "trash",
        danger: true,
        separatorBefore: true,
        onSelect: () => removeTorrent(torrent.id),
      },
    ]);
  }

  // --- Сборка интерфейса --------------------------------------------------

  const sidebar = createSidebar(sidebarMount, {
    onFilter: (filter: FilterKey) => store.patch({ filter }),
    onToggleTheme: toggleTheme,
    onOpenSettings: () => void openSettings(),
    onToggleAll: () => void toggleAll(),
    onQuit: () => void quitApp(),
  });

  const list: TorrentList = createList(listMount, {
    onSelect: (id) => {
      // Выбор строки — только подсветка; панель открывается отдельно (4.5).
      store.patch({ selectedId: id });
    },
    onOpen: (id) => openDetails(id),
    onPauseToggle: (id) => void togglePause(id),
    onContextMenu: (id, anchor) => {
      const torrent = findTorrent(state(), id);
      if (torrent) {
        contextMenu(torrent, anchor);
      }
    },
    onAdd: () => void openAdd(),
    onQuery: (query) => store.patch({ query }),
    onSort: (sort: SortKey) => store.patch({ sort }),
    onClearFilters: () => store.patch({ filter: "all", query: "" }),
    onRetry: () => void bootstrap(),
  });

  const details: DetailsPanel = createDetails(detailsMount, {
    onClose: () => store.patch({ detailsOpen: false }),
    onPauseToggle: (id) => void togglePause(id),
    onReveal: (id) => revealTorrent(id),
    onRemove: (id) => removeTorrent(id),
    onOpenExternal: (url) => openExternal(url),
    onFilePriorities: (id, priorities) => changeFilePriorities(id, priorities),
  });

  const statusbar = createStatusbar(statusbarMount, {
    onToggleAutoShutdown: (enabled) => void toggleAutoShutdown(enabled),
    onCancelAutoShutdown: () => void cancelAutoShutdown(),
  });

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
    setDetailsOpen(current.detailsOpen);
    if (current.payload) {
      notifyTransitions(current.payload);
    }
  });

  document.addEventListener("keydown", (event) => {
    if (modals.isOpen()) {
      return;
    }
    if (event.key === "Escape") {
      // Меню/подменю закрывает свой обработчик; здесь — только панель и выбор.
      if (closeMenu()) {
        return;
      }
      if (state().detailsOpen) {
        store.patch({ detailsOpen: false });
        return;
      }
      if (state().selectedId !== null) {
        store.patch({ selectedId: null });
      }
      return;
    }
    const mod = event.ctrlKey || event.metaKey;
    const key = event.key.toLowerCase();
    if (mod && key === "o") {
      event.preventDefault();
      void openAdd();
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
      if (unsubscribeAuto) {
        unsubscribeAuto();
        unsubscribeAuto = null;
      }
      unsubscribeAuto = await engineApi.subscribeAutoShutdown((next) => {
        if (!tornDown) {
          applyAutoShutdown(next);
        }
      });
      void engineApi.getAutoShutdown().then(applyAutoShutdown, () => {
        // Состояние автовыключения необязательно для работы интерфейса.
      });
      if (unsubscribeOpen) {
        unsubscribeOpen();
        unsubscribeOpen = null;
      }
      unsubscribeOpen = await engineApi.subscribeOpenRequest(() => {
        if (!tornDown) {
          void consumeOpenRequest();
        }
      });
      // Источник, переданный при первом запуске, мог появиться до подписки —
      // событие к этому моменту уже ушло в пустоту, поэтому забираем сразу.
      void consumeOpenRequest();
      if (!stopDrop) {
        stopDrop = engineApi.bindFileDrop({
          onEnter: () => {
            dropOverlay.hidden = false;
          },
          onLeave: () => {
            dropOverlay.hidden = true;
          },
          onDrop: (sources) => {
            dropOverlay.hidden = true;
            void addSources(sources);
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
    unsubscribeAuto?.();
    unsubscribeOpen?.();
    stopDrop?.();
  });

  void bootstrap();
}
