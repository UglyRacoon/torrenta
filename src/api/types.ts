/** Типы данных, которые движок Torrenta отдаёт фронтенду (snake_case — контракт CORE_API). */

export type TorrentState =
  | "queued"
  | "metadata"
  | "downloading"
  | "seeding"
  | "paused"
  | "completed"
  | "error";

export interface FileView {
  path: string;
  length: number;
  downloaded: number;
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
  total_bytes: number;
  downloaded_bytes: number;
  uploaded_bytes: number;
  /** Прогресс 0..1. */
  progress: number;
  piece_count: number;
  piece_length: number;
  pieces_have: number;
  peers_connected: number;
  peers_known: number;
  /** Байт/с. */
  down_rate: number;
  /** Байт/с. */
  up_rate: number;
  eta_seconds: number | null;
  save_path: string;
  /** Unix-время, секунды. */
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
  theme: string;
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

export type EngineMode = "mock" | "tauri";

export type FilterKey = "all" | "downloading" | "seeding" | "paused" | "completed";
export type SortKey = "added" | "name" | "progress" | "speed" | "size";
export type ThemeName = "dark" | "light";
export type ToastKind = "success" | "error" | "info";

/** Вход для добавления торрента: путь к .torrent на диске или уже прочитанные байты. */
export type TorrentInput =
  | { kind: "file"; path: string }
  | { kind: "bytes"; bytes: Uint8Array; name: string };

export interface AddResult {
  id: number;
  name: string;
}
