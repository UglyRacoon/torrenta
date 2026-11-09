import type {
  AddOptions,
  AddResult,
  AppPayload,
  AutoShutdownState,
  EngineMode,
  FilePriority,
  SettingsDto,
  TorrentPreview,
  TorrentSource,
} from "./types";

/** Что делать при перетаскивании файлов в окно. */
export interface FileDropHandlers {
  onEnter(): void;
  onLeave(): void;
  onDrop(sources: TorrentSource[]): void;
}

/**
 * Абстракция движка. В браузере работает демо-реализация на setInterval,
 * внутри Tauri — invoke/listen. Остальной UI о разнице не знает.
 */
export interface EngineApi {
  readonly mode: EngineMode;
  init(): Promise<AppPayload>;
  subscribe(listener: (payload: AppPayload) => void): Promise<() => void>;
  /** Разбор .torrent-файла или magnet: имя, размер, файлы. Ничего не добавляет. */
  preview(source: TorrentSource): Promise<TorrentPreview>;
  addTorrent(source: TorrentSource, options: AddOptions): Promise<AddResult>;
  setFileSelection(id: number, files: number[]): Promise<void>;
  /** Приоритеты всех файлов по порядку (1.3); изменённый — только редактируемый. */
  setFilePriorities(id: number, priorities: FilePriority[]): Promise<void>;
  /** Перепроверка локальных данных; на время проверки состояние — "checking". */
  recheckTorrent(id: number): Promise<void>;
  /** 0 — без ограничения; действует как минимум с глобальным лимитом. */
  setTorrentLimits(id: number, downKib: number, upKib: number): Promise<void>;
  pickTorrentFiles(): Promise<TorrentSource[]>;
  pickDownloadDir(): Promise<string | null>;
  pause(id: number): Promise<void>;
  resume(id: number): Promise<void>;
  pauseAll(): Promise<void>;
  resumeAll(): Promise<void>;
  remove(id: number, deleteFiles: boolean): Promise<void>;
  getSettings(): Promise<SettingsDto>;
  saveSettings(settings: SettingsDto): Promise<SettingsDto>;
  getAutoShutdown(): Promise<AutoShutdownState>;
  setAutoShutdown(enabled: boolean): Promise<AutoShutdownState>;
  cancelAutoShutdown(): Promise<AutoShutdownState>;
  /** Возвращает функцию отписки. */
  subscribeAutoShutdown(listener: (state: AutoShutdownState) => void): Promise<() => void>;
  revealTorrent(id: number): Promise<void>;
  openExternal(url: string): Promise<void>;
  /** Источник извне (файл .torrent или magnet из проводника), забирается один раз. */
  takeOpenRequest(): Promise<string | null>;
  /** Сигнал «появился внешний источник»: после него вызови takeOpenRequest. */
  subscribeOpenRequest(listener: () => void): Promise<() => void>;
  /** Полное завершение приложения (иконка «Выход» в сайдбаре). */
  quitApp(): Promise<void>;
  /** Возвращает функцию отписки. */
  bindFileDrop(handlers: FileDropHandlers): () => void;
}

export function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export async function createEngineApi(): Promise<EngineApi> {
  if (isTauriRuntime()) {
    const { createTauriApi } = await import("./tauri");
    return createTauriApi();
  }
  const { createMockApi } = await import("./mock");
  return createMockApi();
}
