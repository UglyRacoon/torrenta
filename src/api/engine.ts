import type {
  AddOptions,
  AddResult,
  AppPayload,
  AssociationDto,
  AutoShutdownState,
  DiagnosticsDto,
  EngineMode,
  FilePriority,
  PortableInfoDto,
  SettingsDto,
  TorrentPreview,
  TorrentSource,
  UpdateStateDto,
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
  /** Последовательная загрузка торрента (1.7): куски качаются по порядку. */
  setTorrentSequential(id: number, on: boolean): Promise<void>;
  /** Трекеры торрента в порядке добавления (1.7); может быть пустым списком. */
  torrentTrackers(id: number): Promise<string[]>;
  /** Замена списка трекеров (1.7); пустой список допустим — останутся DHT и PEX. */
  setTorrentTrackers(id: number, urls: string[]): Promise<void>;
  /** Принудительный анонс на трекерах (1.7). */
  reannounceTorrent(id: number): Promise<void>;
  /** Срез состояния движка для окна «Диагностика» (1.7). */
  engineDiagnostics(): Promise<DiagnosticsDto>;
  /** Портативный режим и каталог данных (1.7). */
  portableInfo(): Promise<PortableInfoDto>;
  /** Проверка обновлений (1.7); без ключа подписи — configured = false. */
  checkUpdate(): Promise<UpdateStateDto>;
  /** Кто сейчас открывает `.torrent` (1.7, §4.5). */
  associationStatus(): Promise<AssociationDto>;
  /** Перезаписать привязку `.torrent` на текущий exe и вернуть новое состояние. */
  repairAssociation(): Promise<AssociationDto>;
  pickTorrentFiles(): Promise<TorrentSource[]>;
  pickDownloadDir(): Promise<string | null>;
  /** Диалог выбора папки наблюдения (1.5); null — пользователь отменил. */
  pickWatchDir(): Promise<string | null>;
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

/**
 * Клиент, созданный приложением при старте (1.7).
 *
 * Вспомогательные окна — диагностика и редактор трекеров — создаются вне
 * `app.ts`, но им нужен тот же движок: в демо-режиме второй клиент означал бы
 * второй набор торрентов, и правки трекеров не дошли бы до списка.
 */
let active: EngineApi | null = null;

/** Уже созданный клиент или null, если приложение ещё не стартовало. */
export function currentEngineApi(): EngineApi | null {
  return active;
}

/** Тот же клиент, что у приложения; если его ещё нет — создаём и запоминаем. */
export async function requireEngineApi(): Promise<EngineApi> {
  if (!active) {
    active = await createEngineApi();
  }
  return active;
}

export async function createEngineApi(): Promise<EngineApi> {
  if (isTauriRuntime()) {
    const { createTauriApi } = await import("./tauri");
    active = await createTauriApi();
    return active;
  }
  const { createMockApi } = await import("./mock");
  active = await createMockApi();
  return active;
}
