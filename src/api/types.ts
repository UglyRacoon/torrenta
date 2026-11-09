/** DTO движка. Имена полей повторяют serde-представление Rust как есть. */

export type TorrentState =
  | "queued"
  | "metadata"
  | "downloading"
  | "seeding"
  | "paused"
  | "completed"
  | "checking"
  | "error";

/**
 * Приоритет файла (1.3). Не скачивать = "skip": такой файл не входит
 * в wanted_bytes, поэтому selected всегда равен `priority !== "skip"`.
 */
export type FilePriority = "skip" | "low" | "normal" | "high";

/** Файл внутри торрента. selected — выбран ли файл для скачивания. */
export interface FileView {
  path: string;
  length: number;
  downloaded: number;
  selected: boolean;
  priority: FilePriority;
}

export interface PeerView {
  addr: string;
  client: string;
  down_rate: number;
  up_rate: number;
  progress: number;
  is_seed: boolean;
}

export interface TorrentSnapshot {
  id: number;
  name: string;
  info_hash: string;
  state: TorrentState;
  error: string | null;
  /** Полный размер торрента, не зависит от выбранных файлов. */
  total_bytes: number;
  /** Размер только выбранных файлов — база для прогресса. */
  wanted_bytes: number;
  downloaded_bytes: number;
  uploaded_bytes: number;
  progress: number;
  piece_count: number;
  piece_length: number;
  pieces_have: number;
  peers_connected: number;
  peers_known: number;
  down_rate: number;
  up_rate: number;
  /** 0 — без ограничения. */
  down_limit_kib: number;
  /** 0 — без ограничения. */
  up_limit_kib: number;
  eta_seconds: number | null;
  /**
   * Соотношение отдано/скачано (1.6): 0.0, если скачано ещё ничего.
   * Считается движком, поэтому NaN здесь не бывает.
   */
  seed_ratio: number;
  /** Сработал лимит раздачи (1.6): торрент остановлен и ждёт ручного возобновления. */
  seed_limit_reached: boolean;
  save_path: string;
  added_at: number;
  files: FileView[];
  trackers: string[];
  peers: PeerView[];
}

export interface GlobalTotals {
  torrents: number;
  active: number;
  down_rate: number;
  up_rate: number;
  downloaded_bytes: number;
  uploaded_bytes: number;
}

export interface SettingsDto {
  download_dir: string;
  max_peers: number;
  download_limit_kib: number;
  upload_limit_kib: number;
  listen_port: number;
  seed_after_complete: boolean;
  auto_shutdown: boolean;
  /** Автозапуск при входе в Windows (1.2). */
  autostart: boolean;
  /** Запуск свёрнутым в трей (1.2). */
  start_minimized: boolean;
  /** Крестик окна сворачивает в трей, а не закрывает приложение (1.2). */
  close_to_tray: boolean;
  /** Сколько торрентов одновременно скачивать (1.3); 0 — без ограничения. */
  max_active_downloads: number;
  /** Сколько торрентов одновременно раздавать (1.3); 0 — без ограничения. */
  max_active_seeds: number;
  /** Папка наблюдения (1.5): новые .torrent из неё добавляются автоматически. */
  watch_dir: string;
  /** Показывать уведомление о завершении загрузки (1.5). */
  notify_on_complete: boolean;
  /** Восстанавливать список торрентов при запуске (1.5). */
  restore_session: boolean;
  /** Искать пиров через DHT без трекера, BEP 5 (1.6). */
  dht: boolean;
  /** Обмениваться адресами пиров с соседями, BEP 11 (1.6). */
  pex: boolean;
  /** UDP-порт DHT (1.6); 0 — выбрать свободный автоматически. */
  dht_port: number;
  /** Прокси (1.6): пусто — выключен, иначе socks5://host:port или http(s)://host:port. */
  proxy: string;
  /** Пробрасывать порт на роутере через UPnP/NAT-PMP (1.6). */
  port_mapping: boolean;
  /** Качать данные по HTTP из url-list в .torrent, BEP 19 (1.6). */
  web_seeds: boolean;
  /** Останавливать раздачу при ratio ≥ N (1.6); 0 — без ограничения. */
  seed_ratio_limit: number;
  /** Останавливать раздачу через N минут (1.6); 0 — без ограничения. */
  seed_time_limit_min: number;
  theme: ThemeName;
}

export interface AppInfoDto {
  name: string;
  version: string;
  engine: string;
  listen_port: number;
  platform: string;
}

export interface AppPayload {
  torrents: TorrentSnapshot[];
  totals: GlobalTotals;
  settings: SettingsDto;
  app: AppInfoDto;
}

export type EngineMode = "tauri" | "mock";

export type FilterKey = "all" | "downloading" | "seeding" | "paused" | "completed";

export type SortKey = "added" | "name" | "progress" | "speed" | "size";

export type ThemeName = "dark" | "light";

export type ToastKind = "info" | "success" | "warning" | "error";

/**
 * Источник торрента: путь к .torrent или magnet-ссылка.
 * В демо-режиме вместо пути приходит имя выбранного файла.
 */
export type TorrentSource =
  | { kind: "path"; path: string }
  | { kind: "magnet"; link: string };

/** Предпросмотр до добавления: имя, размер, хеш и список файлов. */
export interface PreviewFile {
  index: number;
  path: string;
  length: number;
}

export interface TorrentPreview {
  name: string;
  info_hash: string;
  total_bytes: number;
  piece_length: number;
  piece_count: number;
  files: PreviewFile[];
  trackers: string[];
}

export interface AddOptions {
  /** Каталог сохранения; null — каталог по умолчанию из настроек. */
  savePath: string | null;
  /** Индексы выбранных файлов; null — все файлы. */
  files: number[] | null;
}

export interface AddResult {
  id: number;
  name: string;
}

/** Состояние автовыключения: включено ли и сколько секунд до выключения. */
export interface AutoShutdownState {
  enabled: boolean;
  pending_seconds: number | null;
}

export const NO_LIMIT = 0;

/** Ступени ограничения скорости в КиБ/с — общие для настроек и меню торрента. */
export const SPEED_STEPS: number[] = [NO_LIMIT, 64, 256, 1024, 4096];
