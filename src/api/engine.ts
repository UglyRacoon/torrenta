import type {
  AddResult,
  AppPayload,
  EngineMode,
  SettingsDto,
  TorrentInput,
} from "./types";

/** Что делать при перетаскивании файлов в окно. */
export interface FileDropHandlers {
  onEnter(): void;
  onLeave(): void;
  onDrop(inputs: TorrentInput[]): void;
}

/**
 * Абстракция движка. В браузере работает демо-реализация на setInterval,
 * внутри Tauri — invoke/listen. Остальной UI о разнице не знает.
 */
export interface EngineApi {
  readonly mode: EngineMode;
  init(): Promise<AppPayload>;
  subscribe(listener: (payload: AppPayload) => void): Promise<() => void>;
  addTorrent(input: TorrentInput): Promise<AddResult>;
  addMagnet(link: string): Promise<AddResult>;
  pickTorrentFiles(): Promise<TorrentInput[]>;
  pickDownloadDir(): Promise<string | null>;
  pause(id: number): Promise<void>;
  resume(id: number): Promise<void>;
  pauseAll(): Promise<void>;
  resumeAll(): Promise<void>;
  remove(id: number, deleteFiles: boolean): Promise<void>;
  getSettings(): Promise<SettingsDto>;
  saveSettings(settings: SettingsDto): Promise<SettingsDto>;
  revealTorrent(id: number): Promise<void>;
  openDownloadDir(): Promise<void>;
  openExternal(url: string): Promise<void>;
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
