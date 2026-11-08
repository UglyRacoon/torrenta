import type {
  AddOptions,
  AddResult,
  AppInfoDto,
  AppPayload,
  AutoShutdownState,
  FilePriority,
  FileView,
  GlobalTotals,
  PeerView,
  SettingsDto,
  TorrentPreview,
  TorrentSnapshot,
  TorrentSource,
  TorrentState,
} from "./types";
import type { EngineApi, FileDropHandlers } from "./engine";
import { basename, clamp } from "../lib/format";

/**
 * Демо-движок для браузера: те же DTO, что отдаёт Rust, и живой тикер.
 * Нужен, чтобы весь интерфейс можно было открыть без Tauri.
 */

const DOWNLOADS = String.raw`C:\Users\Пользователь\Downloads`;
const TICK_MS = 700;
const GB = 1_000_000_000;

interface Runtime {
  downTarget: number;
  upTarget: number;
  phase: number;
  metaUntil: number;
  /** Торрент вытеснен лимитом активности (1.3) и ждёт свободный слот. */
  waiting?: "download" | "seed" | null;
  /** Идёт перепроверка (1.3): к чему вернуться и сколько кусков пройдено. */
  check?: { resume: TorrentState; total: number; done: number } | null;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

/** Простой xorshift, чтобы «случайные» хеши/размеры были стабильны в рамках сессии. */
function seedFrom(text: string): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < text.length; i += 1) {
    hash ^= text.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

function hashHex(seed: string): string {
  let state = seedFrom(seed) || 1;
  let result = "";
  for (let i = 0; i < 40; i += 1) {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    result += "0123456789abcdef"[(state >>> 0) % 16];
  }
  return result;
}

function plausibleSize(seed: string): number {
  const value = seedFrom(seed);
  return Math.round((0.8 + (value % 3400) / 1000) * GB);
}

function baseName(name: string): string {
  const cleaned = basename(name).replace(/\.torrent$/i, "").trim();
  return cleaned || "Новый торрент";
}

function peer(
  addr: string,
  client: string,
  down: number,
  up: number,
  progress: number,
  isSeed: boolean,
): PeerView {
  return { addr, client, down_rate: down, up_rate: up, progress, is_seed: isSeed };
}

/** Приоритет и выбор файла всегда согласованы: «не скачивать» = skip. */
function setPriority(file: FileView, priority: FilePriority): void {
  file.priority = priority;
  file.selected = priority !== "skip";
}

interface FixtureFile {
  path: string;
  length: number;
  downloaded: number;
  selected?: boolean;
  priority?: FilePriority;
}

interface FixtureOptions {
  id: number;
  name: string;
  seed: string;
  state: TorrentState;
  total: number;
  downloaded: number;
  uploaded: number;
  pieces: number;
  piecesHave: number;
  peersConnected: number;
  peersKnown: number;
  down: number;
  up: number;
  eta: number | null;
  minutesAgo: number;
  files: FixtureFile[];
  trackers: string[];
  peers: PeerView[];
  error?: string;
}

function fixture(options: FixtureOptions, now: number): TorrentSnapshot {
  const files: FileView[] = options.files.map((file) => ({
    path: file.path,
    length: file.length,
    downloaded: file.downloaded,
    selected: file.selected ?? true,
    priority: file.priority ?? (file.selected === false ? "skip" : "normal"),
  }));
  const wanted =
    files.length > 0
      ? files.reduce((sum, file) => sum + (file.selected ? file.length : 0), 0)
      : options.total;
  return {
    id: options.id,
    name: options.name,
    info_hash: hashHex(options.seed || options.name),
    state: options.state,
    error: options.error ?? null,
    total_bytes: options.total,
    wanted_bytes: wanted,
    downloaded_bytes: options.downloaded,
    uploaded_bytes: options.uploaded,
    progress: wanted > 0 ? clamp(options.downloaded / wanted, 0, 1) : 0,
    piece_count: options.pieces,
    piece_length: 2_097_152,
    pieces_have: options.piecesHave,
    peers_connected: options.peersConnected,
    peers_known: options.peersKnown,
    down_rate: options.down,
    up_rate: options.up,
    down_limit_kib: 0,
    up_limit_kib: 0,
    eta_seconds: options.eta,
    save_path: DOWNLOADS,
    added_at: Math.floor(now / 1000) - options.minutesAgo * 60,
    files,
    trackers: options.trackers,
    peers: options.peers,
  };
}

function buildFixtures(now: number): TorrentSnapshot[] {
  return [
    fixture(
      {
        id: 1,
        name: "ubuntu-24.04.1-desktop-amd64.iso",
        seed: "ubuntu",
        state: "downloading",
        total: 6_100_000_000,
        downloaded: 2_623_000_000,
        uploaded: 318_000_000,
        pieces: 2_910,
        piecesHave: 1_251,
        peersConnected: 27,
        peersKnown: 84,
        down: 11_300_000,
        up: 1_150_000,
        eta: 308,
        minutesAgo: 8,
        files: [
          {
            path: "ubuntu-24.04.1-desktop-amd64.iso",
            length: 6_099_999_846,
            downloaded: 2_622_999_846,
          },
          { path: "SHA256SUMS", length: 154, downloaded: 154 },
        ],
        trackers: [
          "udp://tracker.opentrackr.org:1337/announce",
          "https://torrent.ubuntu.com/announce",
          "udp://open.demonii.com:1337/announce",
        ],
        peers: [
          peer("185.125.190.36:6881", "qBittorrent 4.6.5", 2_400_000, 120_000, 0.92, true),
          peer("91.219.212.12:51413", "Transmission 4.0.5", 1_850_000, 80_000, 0.71, false),
          peer("78.46.84.17:49152", "Deluge 2.1.1", 1_240_000, 40_000, 0.38, false),
          peer("176.9.41.203:6881", "libtorrent 2.0.9", 980_000, 0, 0.15, false),
          peer("45.12.4.88:12345", "BitTorrent 7.10", 620_000, 0, 0.06, false),
        ],
      },
      now,
    ),
    fixture(
      {
        id: 2,
        name: "Архив документации 2024 [PDF, DJVU]",
        seed: "archive-2024",
        state: "metadata",
        total: 0,
        downloaded: 0,
        uploaded: 0,
        pieces: 0,
        piecesHave: 0,
        peersConnected: 4,
        peersKnown: 12,
        down: 0,
        up: 0,
        eta: null,
        minutesAgo: 26,
        files: [],
        trackers: [
          "udp://tracker.opentrackr.org:1337/announce",
          "wss://tracker.openwebtorrent.com",
        ],
        peers: [
          peer("93.184.216.34:6881", "qBittorrent 4.6.5", 0, 0, 0, false),
          peer("212.113.119.5:51413", "Transmission 4.0.5", 0, 0, 0, false),
          peer("5.9.84.14:49152", "Deluge 2.1.1", 0, 0, 0, false),
          peer("37.48.65.9:6881", "libtorrent 2.0.9", 0, 0, 0, false),
        ],
      },
      now,
    ),
    fixture(
      {
        id: 3,
        name: "Дюна. Часть вторая (2024) 2160p HDR10 HEVC.mkv",
        seed: "dune-part-two",
        state: "seeding",
        total: 19_800_000_000,
        downloaded: 19_800_000_000,
        uploaded: 27_320_000_000,
        pieces: 4_720,
        piecesHave: 4_720,
        peersConnected: 14,
        peersKnown: 41,
        down: 0,
        up: 3_240_000,
        eta: null,
        minutesAgo: 2 * 24 * 60,
        files: [
          {
            path: "Дюна. Часть вторая (2024) 2160p HDR10 HEVC.mkv",
            length: 19_799_900_000,
            downloaded: 19_799_900_000,
          },
          {
            path: "subtitles/Дюна. Часть вторая (2024).ru.srt",
            length: 100_000,
            downloaded: 100_000,
          },
        ],
        trackers: [
          "udp://tracker.opentrackr.org:1337/announce",
          "http://bt.t-ru.org/announce",
          "udp://open.demonii.com:1337/announce",
        ],
        peers: [
          peer("188.43.20.11:51413", "qBittorrent 4.6.2", 0, 1_240_000, 0.64, false),
          peer("95.31.18.44:6881", "Transmission 4.0.5", 0, 860_000, 0.41, false),
          peer("46.29.8.77:12345", "BitTorrent 7.10", 0, 420_000, 0.22, false),
          peer("109.172.4.91:49152", "Deluge 2.1.1", 0, 260_000, 0.08, false),
        ],
      },
      now,
    ),
    fixture(
      {
        id: 4,
        name: "Подкаст «Точка» — сезон 3 (MP3).zip",
        seed: "tochka-season-3",
        state: "completed",
        total: 1_240_000_000,
        downloaded: 1_240_000_000,
        uploaded: 2_284_000_000,
        pieces: 1_184,
        piecesHave: 1_184,
        peersConnected: 0,
        peersKnown: 9,
        down: 0,
        up: 0,
        eta: null,
        minutesAgo: 4 * 24 * 60,
        files: [
          {
            path: "Подкаст «Точка» — сезон 3 (MP3).zip",
            length: 1_240_000_000,
            downloaded: 1_240_000_000,
          },
        ],
        trackers: [
          "udp://tracker.opentrackr.org:1337/announce",
          "https://tracker.gbitt.info/announce",
        ],
        peers: [],
      },
      now,
    ),
    fixture(
      {
        id: 5,
        name: "linuxmint-22-cinnamon-64bit.iso",
        seed: "linux-mint-22",
        state: "paused",
        total: 3_100_000_000,
        downloaded: 1_985_000_000,
        uploaded: 412_000_000,
        pieces: 1_479,
        piecesHave: 947,
        peersConnected: 0,
        peersKnown: 37,
        down: 0,
        up: 0,
        eta: null,
        minutesAgo: 6 * 24 * 60,
        files: [
          {
            path: "linuxmint-22-cinnamon-64bit.iso",
            length: 3_099_999_872,
            downloaded: 1_984_999_872,
          },
          { path: "sha256sum.txt", length: 128, downloaded: 128 },
        ],
        trackers: [
          "udp://tracker.opentrackr.org:1337/announce",
          "https://torrent.linuxmint.com/announce",
        ],
        peers: [],
      },
      now,
    ),
    fixture(
      {
        id: 6,
        name: "Big Buck Bunny 4K (2008).mp4",
        seed: "big-buck-bunny-4k",
        state: "error",
        total: 734_000_000,
        downloaded: 97_500_000,
        uploaded: 0,
        pieces: 1_400,
        piecesHave: 233,
        peersConnected: 0,
        peersKnown: 14,
        down: 0,
        up: 0,
        eta: null,
        minutesAgo: 9 * 24 * 60,
        error: "Трекер недоступен: истекло время ожидания (30 с)",
        files: [
          {
            path: "Big Buck Bunny 4K (2008).mp4",
            length: 734_000_000,
            downloaded: 97_500_000,
          },
        ],
        trackers: ["udp://tracker.example.org:6969/announce"],
        peers: [],
      },
      now,
    ),
  ];
}

function stateOf(torrent: TorrentSnapshot): TorrentState {
  return torrent.state;
}

/**
 * Демо-разбор источника: без сети, но с той же формой ответа, что у Rust.
 * Для магнита имя берётся из dn, для файла — из имени файла.
 */
function previewOf(source: TorrentSource): TorrentPreview {
  if (source.kind === "magnet") {
    const link = source.link.trim();
    if (!/^magnet:\?/i.test(link)) {
      throw new Error("Ссылка должна начинаться с magnet:?");
    }
    const params = new URLSearchParams(link.slice(link.indexOf("?") + 1));
    const xt = params.get("xt") ?? "";
    const match = /urn:btih:([0-9a-z]+)/i.exec(xt);
    if (!match) {
      throw new Error("В magnet-ссылке нет info-hash (xt=urn:btih)");
    }
    const name = params.get("dn")?.trim() || "Magnet-загрузка";
    const total = plausibleSize(name);
    return {
      name,
      info_hash: match[1].toLowerCase(),
      total_bytes: total,
      piece_length: 2_097_152,
      piece_count: Math.ceil(total / 2_097_152),
      files: [{ index: 0, path: name, length: total }],
      trackers: params.getAll("tr").filter(Boolean).slice(0, 6),
    };
  }

  const path = source.path.trim();
  if (!/\.torrent$/i.test(path)) {
    throw new Error("Это не .torrent-файл");
  }
  const name = baseName(path);
  const main = plausibleSize(name);
  const files = [
    { index: 0, path: name, length: main },
    { index: 1, path: "readme.txt", length: 2_048 },
  ];
  const total = main + 2_048;
  return {
    name,
    info_hash: hashHex(`preview:${name}`),
    total_bytes: total,
    piece_length: 2_097_152,
    piece_count: Math.ceil(total / 2_097_152),
    files,
    trackers: [
      "udp://tracker.opentrackr.org:1337/announce",
      "https://tracker.gbitt.info/announce",
    ],
  };
}

export async function createMockApi(): Promise<EngineApi> {
  const now = Date.now();
  const torrents = buildFixtures(now);
  const runtimes = new Map<number, Runtime>([
    [1, { downTarget: 11_300_000, upTarget: 1_150_000, phase: 0.4, metaUntil: 0 }],
    [2, { downTarget: 4_600_000, upTarget: 380_000, phase: 1.7, metaUntil: now + 20_000 }],
    [3, { downTarget: 0, upTarget: 3_240_000, phase: 2.9, metaUntil: 0 }],
  ]);

  const settings: SettingsDto = {
    download_dir: DOWNLOADS,
    max_peers: 50,
    download_limit_kib: 0,
    upload_limit_kib: 0,
    listen_port: 51413,
    seed_after_complete: true,
    auto_shutdown: false,
    autostart: false,
    start_minimized: false,
    close_to_tray: false,
    max_active_downloads: 0,
    max_active_seeds: 0,
    theme: "dark",
  };

  const app: AppInfoDto = {
    name: "Torrenta",
    version: "1.3.0",
    engine: "torrent-core 0.2.0",
    listen_port: 51413,
    platform: "windows",
  };

  const listeners = new Set<(payload: AppPayload) => void>();
  const autoListeners = new Set<(state: AutoShutdownState) => void>();
  let nextId = Math.max(...torrents.map((torrent) => torrent.id)) + 1;
  let timer = 0;
  let autoTimer = 0;
  let autoPending: number | null = null;

  function deriveTotals(): GlobalTotals {
    let active = 0;
    let downRate = 0;
    let upRate = 0;
    let downloaded = 0;
    let uploaded = 0;
    for (const torrent of torrents) {
      const state = stateOf(torrent);
      if (state === "downloading" || state === "seeding" || state === "metadata" || state === "queued") {
        active += 1;
      }
      downRate += torrent.down_rate;
      upRate += torrent.up_rate;
      downloaded += torrent.downloaded_bytes;
      uploaded += torrent.uploaded_bytes;
    }
    return {
      torrents: torrents.length,
      active,
      down_rate: downRate,
      up_rate: upRate,
      downloaded_bytes: downloaded,
      uploaded_bytes: uploaded,
    };
  }

  function snapshot(): AppPayload {
    return {
      torrents: torrents.slice(),
      totals: deriveTotals(),
      settings: { ...settings },
      app: { ...app },
    };
  }

  function emit(): void {
    const payload = snapshot();
    for (const listener of [...listeners]) {
      listener(payload);
    }
  }

  function find(id: number): TorrentSnapshot {
    const torrent = torrents.find((item) => item.id === id);
    if (!torrent) {
      throw new Error("Торрент не найден");
    }
    return torrent;
  }

  function syncWanted(torrent: TorrentSnapshot): void {
    torrent.wanted_bytes =
      torrent.files.length > 0
        ? torrent.files.reduce((sum, file) => sum + (file.selected ? file.length : 0), 0)
        : torrent.total_bytes;
  }

  /** Раскладывает скачанное по выбранным файлам пропорционально их размеру. */
  function syncFiles(torrent: TorrentSnapshot): void {
    syncWanted(torrent);
    const wanted = torrent.wanted_bytes;
    if (wanted <= 0) {
      return;
    }
    const ratio = clamp(torrent.downloaded_bytes / wanted, 0, 1);
    for (const file of torrent.files) {
      file.downloaded = file.selected ? Math.min(file.length, Math.round(file.length * ratio)) : 0;
    }
  }

  function refreshProgress(torrent: TorrentSnapshot): void {
    const wanted = torrent.wanted_bytes;
    torrent.progress = wanted > 0 ? clamp(torrent.downloaded_bytes / wanted, 0, 1) : 0;
    torrent.pieces_have = Math.min(
      torrent.piece_count,
      Math.floor(torrent.progress * torrent.piece_count),
    );
    if (torrent.state === "downloading") {
      torrent.eta_seconds =
        torrent.down_rate > 0
          ? Math.ceil(Math.max(0, wanted - torrent.downloaded_bytes) / torrent.down_rate)
          : null;
    }
  }

  /** Эффективный лимит: минимум из глобального и лимита торрента, 0 — без ограничения. */
  function capRate(rate: number, torrentLimit: number, globalLimit: number): number {
    const limits = [torrentLimit, globalLimit].filter((value) => value > 0);
    if (limits.length === 0) {
      return rate;
    }
    const limit = Math.min(...limits) * 1024;
    return Math.min(rate, limit);
  }

  function complete(torrent: TorrentSnapshot): void {
    torrent.state = "completed";
    torrent.progress = 1;
    torrent.pieces_have = torrent.piece_count;
    torrent.down_rate = 0;
    torrent.up_rate = 0;
    torrent.eta_seconds = null;
    torrent.peers_connected = 0;
    torrent.downloaded_bytes = torrent.wanted_bytes;
    for (const file of torrent.files) {
      file.downloaded = file.selected ? file.length : 0;
    }
  }

  /** Пересчёт прогресса после смены выбора/приоритетов файлов (как в бэкенде). */
  function applyFileChange(torrent: TorrentSnapshot, before: number[]): void {
    syncWanted(torrent);
    if (torrent.wanted_bytes <= 0 && torrent.files.length > 0) {
      // Ничего не выбрано: качать нечего, поэтому обнуляем прогресс и скорости.
      torrent.downloaded_bytes = 0;
      torrent.progress = 0;
      torrent.pieces_have = 0;
      torrent.down_rate = 0;
      torrent.up_rate = 0;
      torrent.eta_seconds = null;
      for (const file of torrent.files) {
        file.downloaded = 0;
      }
      return;
    }
    const kept = torrent.files.reduce(
      (sum, file, index) => sum + (file.selected ? Math.min(file.length, before[index]) : 0),
      0,
    );
    torrent.downloaded_bytes = Math.min(kept, torrent.wanted_bytes);
    syncFiles(torrent);
    refreshProgress(torrent);
    if (torrent.wanted_bytes > 0 && torrent.downloaded_bytes >= torrent.wanted_bytes) {
      complete(torrent);
    } else if (torrent.state === "completed" || torrent.state === "seeding") {
      // Часть файлов отключили — загрузка вернулась в незавершённое состояние.
      torrent.state = "paused";
      torrent.down_rate = 0;
      torrent.up_rate = 0;
      torrent.eta_seconds = null;
    }
  }

  function autoState(): AutoShutdownState {
    return { enabled: settings.auto_shutdown, pending_seconds: autoPending };
  }

  function emitAuto(): void {
    const state = autoState();
    for (const listener of [...autoListeners]) {
      listener(state);
    }
  }

  /**
   * Условие выключения как в бэкенде (`auto_shutdown::should_shutdown`):
   * есть хотя бы один завершённый/раздающий торрент и никто не мешает —
   * ни очередь, ни метаданные, ни загрузка, ни пауза (пауза — осознанное
   * действие пользователя, за его спиной ПК не выключаем; ошибка не мешает).
   */
  function downloadsFinished(): boolean {
    const finished = torrents.some(
      (torrent) => torrent.state === "completed" || torrent.state === "seeding",
    );
    if (!finished) {
      return false;
    }
    return torrents.every(
      (torrent) =>
        torrent.state !== "queued" &&
        torrent.state !== "metadata" &&
        torrent.state !== "downloading" &&
        torrent.state !== "paused",
    );
  }

  function cancelPending(): void {
    if (autoPending === null) {
      return;
    }
    autoPending = null;
    emitAuto();
  }

  function autoTick(): void {
    if (!settings.auto_shutdown) {
      cancelPending();
      return;
    }
    if (autoPending === null) {
      if (!downloadsFinished()) {
        return;
      }
      autoPending = 60;
      emitAuto();
      return;
    }
    autoPending -= 1;
    if (autoPending > 0) {
      emitAuto();
      return;
    }
    // Демо-режим не выключает машину: показываем 0 и снимаем отсчёт.
    autoPending = null;
    settings.auto_shutdown = false;
    emitAuto();
    emit();
  }

  function startAuto(): void {
    if (autoTimer === 0) {
      autoTimer = window.setInterval(autoTick, 1000);
    }
  }

  function resolveMetadata(torrent: TorrentSnapshot, runtime: Runtime): void {
    torrent.state = "downloading";
    torrent.total_bytes = 2_400_000_000;
    torrent.piece_count = 1_144;
    torrent.pieces_have = 0;
    torrent.progress = 0;
    torrent.peers_connected = 11;
    torrent.peers_known = 26;
    torrent.down_rate = runtime.downTarget;
    torrent.up_rate = runtime.upTarget;
    torrent.eta_seconds = Math.ceil(2_400_000_000 / runtime.downTarget);
    torrent.files = [
      {
        path: "Архив документации 2024.pdf",
        length: 1_850_000_000,
        downloaded: 0,
        selected: true,
        priority: "normal",
      },
      {
        path: "Приложения/Справочник.djv",
        length: 550_000_000,
        downloaded: 0,
        selected: true,
        priority: "normal",
      },
    ];
    syncWanted(torrent);
    torrent.wanted_bytes = 2_400_000_000;
    if (torrent.trackers.length === 0) {
      torrent.trackers = ["udp://tracker.opentrackr.org:1337/announce"];
    }
  }

  /**
   * Лимиты активности (1.3): одновременно качают/раздают не больше N торрентов,
   * остальные ждут в очереди и возвращаются сами, как только слот освободится.
   * 0 — без ограничения, как в бэкенде.
   */
  function applyActiveLimits(): void {
    const downCap =
      settings.max_active_downloads > 0 ? settings.max_active_downloads : Number.POSITIVE_INFINITY;
    const seedCap =
      settings.max_active_seeds > 0 ? settings.max_active_seeds : Number.POSITIVE_INFINITY;
    let downloading = 0;
    let seeding = 0;
    for (const torrent of torrents) {
      const runtime = runtimes.get(torrent.id);
      if (!runtime) {
        continue;
      }
      const waitsForDown = torrent.state === "queued" && runtime.waiting === "download";
      const waitsForSeed = torrent.state === "queued" && runtime.waiting === "seed";
      if (torrent.state === "downloading" || waitsForDown) {
        if (downloading < downCap) {
          downloading += 1;
          if (waitsForDown) {
            runtime.waiting = null;
            torrent.state = "downloading";
          }
        } else if (torrent.state === "downloading") {
          runtime.waiting = "download";
          torrent.state = "queued";
          torrent.down_rate = 0;
          torrent.up_rate = 0;
          torrent.eta_seconds = null;
        }
        continue;
      }
      if (torrent.state === "seeding" || waitsForSeed) {
        if (seeding < seedCap) {
          seeding += 1;
          if (waitsForSeed) {
            runtime.waiting = null;
            torrent.state = "seeding";
          }
        } else if (torrent.state === "seeding") {
          runtime.waiting = "seed";
          torrent.state = "queued";
          torrent.down_rate = 0;
          torrent.up_rate = 0;
        }
      }
    }
  }

  /** Перепроверка (1.3): прогресс считается по кускам, затем возврат в прежнее состояние. */
  function startRecheck(torrent: TorrentSnapshot): void {
    const runtime =
      runtimes.get(torrent.id) ??
      { downTarget: 6_000_000, upTarget: 400_000, phase: 0, metaUntil: 0 };
    runtimes.set(torrent.id, runtime);
    runtime.waiting = null;
    runtime.check = {
      resume: torrent.state,
      total: Math.max(1, torrent.piece_count),
      done: 0,
    };
    torrent.state = "checking";
    torrent.progress = 0;
    torrent.down_rate = 0;
    torrent.up_rate = 0;
    torrent.eta_seconds = null;
    start();
  }

  function tick(): void {
    const stamp = Date.now();
    applyActiveLimits();
    for (const torrent of torrents) {
      const runtime = runtimes.get(torrent.id);
      if (!runtime) {
        continue;
      }
      const state = stateOf(torrent);
      if (state === "checking") {
        const check = runtime.check;
        if (!check) {
          continue;
        }
        const step = Math.max(1, Math.ceil(check.total / 20));
        check.done = Math.min(check.total, check.done + step);
        torrent.pieces_have = check.done;
        torrent.progress = check.done / check.total;
        if (check.done >= check.total) {
          runtime.check = null;
          torrent.state = check.resume;
          refreshProgress(torrent);
        }
        continue;
      }
      if (state === "metadata") {
        if (runtime.metaUntil > 0 && stamp >= runtime.metaUntil) {
          runtime.metaUntil = 0;
          resolveMetadata(torrent, runtime);
        }
        continue;
      }
      if (state === "downloading") {
        if (torrent.wanted_bytes <= 0) {
          // Все файлы исключены (приоритет «не скачивать»): качать нечего,
          // движок простаивает — нули вместо скорости и ETA.
          torrent.down_rate = 0;
          torrent.up_rate = 0;
          torrent.eta_seconds = null;
          continue;
        }
        const wobble = 0.78 + 0.44 * (0.5 + 0.5 * Math.sin(stamp / 2600 + runtime.phase));
        const rawDown = runtime.downTarget * wobble;
        const rawUp = runtime.upTarget * (0.7 + 0.6 * (0.5 + 0.5 * Math.sin(stamp / 3900 + runtime.phase)));
        const down = capRate(rawDown, torrent.down_limit_kib, settings.download_limit_kib);
        const up = capRate(rawUp, torrent.up_limit_kib, settings.upload_limit_kib);
        torrent.down_rate = down;
        torrent.up_rate = up;
        torrent.downloaded_bytes = Math.min(
          torrent.wanted_bytes,
          torrent.downloaded_bytes + (down * TICK_MS) / 1000,
        );
        torrent.uploaded_bytes += (up * TICK_MS) / 1000;
        syncFiles(torrent);
        refreshProgress(torrent);
        if (torrent.downloaded_bytes >= torrent.wanted_bytes) {
          complete(torrent);
        }
        continue;
      }
      if (state === "seeding") {
        const rawUp = runtime.upTarget * (0.72 + 0.55 * (0.5 + 0.5 * Math.sin(stamp / 3300 + runtime.phase)));
        torrent.up_rate = capRate(rawUp, torrent.up_limit_kib, settings.upload_limit_kib);
        torrent.down_rate = 0;
        torrent.uploaded_bytes += (torrent.up_rate * TICK_MS) / 1000;
      }
    }
    emit();
  }

  function start(): void {
    if (timer === 0) {
      timer = window.setInterval(tick, TICK_MS);
    }
  }

  function addSnapshot(
    name: string,
    state: TorrentState,
    total: number,
    trackers: string[],
    options: { savePath: string | null; files: { path: string; length: number }[] | null; selected: number[] | null },
  ): AddResult {
    const id = nextId;
    nextId += 1;
    const pieces = total > 0 ? Math.ceil(total / 2_097_152) : 0;
    const sourceFiles =
      options.files && options.files.length > 0
        ? options.files
        : total > 0
          ? [{ path: name, length: total }]
          : [];
    const files: FileView[] = sourceFiles.map((file, index) => {
      const selected = options.selected === null ? true : options.selected.includes(index);
      return {
        path: file.path,
        length: file.length,
        downloaded: 0,
        selected,
        priority: selected ? "normal" : "skip",
      };
    });
    const torrent: TorrentSnapshot = {
      id,
      name,
      info_hash: hashHex(`${name}:${id}`),
      state,
      error: null,
      total_bytes: total,
      wanted_bytes: 0,
      downloaded_bytes: 0,
      uploaded_bytes: 0,
      progress: 0,
      piece_count: pieces,
      piece_length: 2_097_152,
      pieces_have: 0,
      peers_connected: state === "metadata" ? 5 : 0,
      peers_known: state === "metadata" ? 14 : 18,
      down_rate: 0,
      up_rate: 0,
      down_limit_kib: 0,
      up_limit_kib: 0,
      eta_seconds: null,
      save_path: options.savePath ?? settings.download_dir,
      added_at: Math.floor(Date.now() / 1000),
      files,
      trackers,
      peers:
        state === "metadata"
          ? [
              peer("93.184.216.34:6881", "qBittorrent 4.6.5", 0, 0, 0, false),
              peer("212.113.119.5:51413", "Transmission 4.0.5", 0, 0, 0, false),
              peer("5.9.84.14:49152", "Deluge 2.1.1", 0, 0, 0, false),
            ]
          : [],
    };
    syncWanted(torrent);
    torrents.push(torrent);
    runtimes.set(id, {
      downTarget: 8_400_000,
      upTarget: 900_000,
      phase: Math.random() * Math.PI * 2,
      metaUntil: state === "metadata" ? Date.now() + 14_000 : 0,
    });
    // Новая загрузка отменяет отсчёт автовыключения.
    cancelPending();
    emit();
    return { id, name };
  }

  start();
  startAuto();

  return {
    mode: "mock",

    init: async () => snapshot(),

    subscribe: async (listener) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },

    preview: async (source: TorrentSource): Promise<TorrentPreview> => {
      // Магнит «ждёт метаданные» заметно дольше — интерфейс успевает показать спиннер.
      await delay(source.kind === "magnet" ? 1_100 : 200);
      return previewOf(source);
    },

    addTorrent: async (source: TorrentSource, options: AddOptions): Promise<AddResult> => {
      const preview = previewOf(source);
      await delay(360);
      if (options.files !== null && options.files.length === 0) {
        throw new Error("Не выбран ни один файл");
      }
      const existing = torrents.find((torrent) => torrent.name === preview.name);
      if (existing) {
        throw new Error("Такой торрент уже добавлен");
      }
      const trackers =
        preview.trackers.length > 0
          ? preview.trackers
          : ["udp://tracker.opentrackr.org:1337/announce"];
      if (source.kind === "magnet") {
        return addSnapshot(preview.name, "metadata", 0, trackers, {
          savePath: options.savePath,
          files: null,
          selected: null,
        });
      }
      return addSnapshot(preview.name, "downloading", preview.total_bytes, trackers, {
        savePath: options.savePath,
        files: options.files === null ? preview.files : preview.files.map((file) => ({
          path: file.path,
          length: file.length,
        })),
        selected: options.files,
      });
    },

    setFileSelection: async (id, files) => {
      const torrent = find(id);
      await delay(140);
      if (files.length === 0) {
        throw new Error("Не выбран ни один файл");
      }
      const wanted = new Set(files);
      const before = torrent.files.map((file) => (file.selected ? file.downloaded : 0));
      torrent.files.forEach((file, index) => {
        const selected = wanted.has(index);
        // 1.3: выбранный файл получает обычный приоритет, снятый — «не скачивать».
        setPriority(file, selected ? (file.priority === "skip" ? "normal" : file.priority) : "skip");
      });
      applyFileChange(torrent, before);
      emit();
    },

    setFilePriorities: async (id, priorities) => {
      const torrent = find(id);
      await delay(140);
      if (priorities.length !== torrent.files.length) {
        throw new Error("Список приоритетов не совпадает с числом файлов");
      }
      for (const priority of priorities) {
        if (
          priority !== "skip" &&
          priority !== "low" &&
          priority !== "normal" &&
          priority !== "high"
        ) {
          throw new Error(`Неизвестный приоритет файла: ${String(priority)}`);
        }
      }
      if (torrent.files.every((file, index) => file.priority === priorities[index])) {
        return;
      }
      const before = torrent.files.map((file) => (file.selected ? file.downloaded : 0));
      torrent.files.forEach((file, index) => setPriority(file, priorities[index]));
      applyFileChange(torrent, before);
      emit();
    },

    recheckTorrent: async (id) => {
      const torrent = find(id);
      await delay(120);
      if (torrent.state === "checking") {
        return;
      }
      startRecheck(torrent);
      emit();
    },

    setTorrentLimits: async (id, downKib, upKib) => {
      const torrent = find(id);
      await delay(100);
      torrent.down_limit_kib = Math.max(0, Math.round(downKib));
      torrent.up_limit_kib = Math.max(0, Math.round(upKib));
      emit();
    },

    getAutoShutdown: async () => {
      startAuto();
      return autoState();
    },

    setAutoShutdown: async (enabled: boolean) => {
      await delay(120);
      settings.auto_shutdown = enabled;
      if (!enabled) {
        autoPending = null;
      }
      startAuto();
      emitAuto();
      emit();
      return autoState();
    },

    cancelAutoShutdown: async () => {
      await delay(80);
      // Как в бэкенде (`commands::cancel_auto_shutdown`): отмена отсчёта
      // заодно снимает настройку, иначе наблюдатель запустит отсчёт снова.
      settings.auto_shutdown = false;
      autoPending = null;
      emitAuto();
      emit();
      return autoState();
    },

    subscribeAutoShutdown: async (listener) => {
      autoListeners.add(listener);
      startAuto();
      listener(autoState());
      return () => {
        autoListeners.delete(listener);
      };
    },

    pickTorrentFiles: async () => {
      const input = document.createElement("input");
      input.type = "file";
      input.accept = ".torrent,application/x-bittorrent";
      input.multiple = true;
      input.hidden = true;
      document.body.append(input);

      const files = await new Promise<File[]>((resolve) => {
        let settled = false;
        let focusTimer = 0;
        const cleanup = () => {
          input.removeEventListener("change", onChange);
          window.removeEventListener("focus", onFocus);
          window.clearTimeout(focusTimer);
          input.remove();
        };
        const finish = (result: File[]) => {
          if (settled) {
            return;
          }
          settled = true;
          cleanup();
          resolve(result);
        };
        const onChange = () => finish(Array.from(input.files ?? []));
        const onFocus = () => {
          focusTimer = window.setTimeout(() => finish([]), 400);
        };
        input.addEventListener("change", onChange);
        window.addEventListener("focus", onFocus, { once: true });
        input.click();
      });

      // В браузере реальный путь недоступен — отдаём имя файла, как его видит демо-движок.
      return files.map((file) => ({ kind: "path" as const, path: file.name }));
    },

    pickDownloadDir: async () => {
      await delay(150);
      return settings.download_dir;
    },

    pause: async (id) => {
      const torrent = find(id);
      await delay(120);
      torrent.state = "paused";
      torrent.down_rate = 0;
      torrent.up_rate = 0;
      torrent.eta_seconds = null;
      // Пауза прерывает и перепроверку, и ожидание слота активности (1.3).
      const runtime = runtimes.get(id);
      if (runtime) {
        runtime.check = null;
        runtime.waiting = null;
      }
      emit();
    },

    resume: async (id) => {
      const torrent = find(id);
      await delay(120);
      if (torrent.state !== "paused" && torrent.state !== "completed" && torrent.state !== "error") {
        return;
      }
      // Возобновление тоже отменяет отсчёт автовыключения.
      cancelPending();
      // Из ошибки возвращаемся в загрузку: движок заново опрашивает трекер.
      if (torrent.state === "error") {
        torrent.error = null;
      }
      if (torrent.total_bytes <= 0) {
        torrent.state = "metadata";
        const runtime = runtimes.get(id);
        if (runtime) {
          runtime.metaUntil = Date.now() + 6_000;
        }
      } else if (torrent.wanted_bytes > 0 && torrent.downloaded_bytes >= torrent.wanted_bytes) {
        torrent.state = "seeding";
        const runtime = runtimes.get(id) ?? {
          downTarget: 0,
          upTarget: 1_200_000,
          phase: Math.random() * Math.PI * 2,
          metaUntil: 0,
        };
        if (runtime.upTarget <= 0) {
          runtime.upTarget = 1_200_000;
        }
        runtimes.set(id, runtime);
      } else {
        torrent.state = "downloading";
        const runtime = runtimes.get(id) ?? {
          downTarget: 6_400_000,
          upTarget: 700_000,
          phase: Math.random() * Math.PI * 2,
          metaUntil: 0,
        };
        if (runtime.downTarget <= 0) {
          runtime.downTarget = 6_400_000;
        }
        runtimes.set(id, runtime);
      }
      emit();
    },

    pauseAll: async () => {
      await delay(120);
      for (const torrent of torrents) {
        if (
          torrent.state === "downloading" ||
          torrent.state === "seeding" ||
          torrent.state === "metadata" ||
          torrent.state === "queued"
        ) {
          torrent.state = "paused";
          torrent.down_rate = 0;
          torrent.up_rate = 0;
          torrent.eta_seconds = null;
          const runtime = runtimes.get(torrent.id);
          if (runtime) {
            runtime.check = null;
            runtime.waiting = null;
          }
        }
      }
      emit();
    },

    resumeAll: async () => {
      await delay(120);
      for (const torrent of torrents) {
        if (torrent.state !== "paused") {
          continue;
        }
        if (torrent.total_bytes <= 0) {
          torrent.state = "metadata";
          continue;
        }
        if (torrent.wanted_bytes > 0 && torrent.downloaded_bytes >= torrent.wanted_bytes) {
          torrent.state = "seeding";
          continue;
        }
        torrent.state = "downloading";
        if (!runtimes.has(torrent.id)) {
          runtimes.set(torrent.id, {
            downTarget: 6_400_000,
            upTarget: 700_000,
            phase: Math.random() * Math.PI * 2,
            metaUntil: 0,
          });
        }
      }
      emit();
    },

    remove: async (id, deleteFiles) => {
      await delay(150);
      const index = torrents.findIndex((torrent) => torrent.id === id);
      if (index < 0) {
        throw new Error("Торрент не найден");
      }
      torrents.splice(index, 1);
      runtimes.delete(id);
      // Демо-режим ничего не удаляет с диска: deleteFiles только для совместимости контракта.
      void deleteFiles;
      emit();
    },

    getSettings: async () => ({ ...settings }),

    saveSettings: async (next) => {
      await delay(180);
      const port = Math.round(next.listen_port);
      settings.download_dir = next.download_dir.trim() || DOWNLOADS;
      settings.max_peers = clamp(Math.round(next.max_peers), 1, 500) || 50;
      settings.download_limit_kib = Math.max(0, Math.round(next.download_limit_kib));
      settings.upload_limit_kib = Math.max(0, Math.round(next.upload_limit_kib));
      settings.listen_port = port >= 0 && port <= 65535 ? port : 51413;
      settings.seed_after_complete = Boolean(next.seed_after_complete);
      settings.auto_shutdown = Boolean(next.auto_shutdown);
      // 1.2: трей и автозагрузка. В демо-режиме ничего не делаем с системой,
      // значения просто сохраняются, чтобы окно настроек показывало выбор.
      settings.autostart = Boolean(next.autostart);
      settings.start_minimized = Boolean(next.start_minimized);
      settings.close_to_tray = Boolean(next.close_to_tray);
      // 1.3: лимиты очереди, 0 — без ограничения.
      settings.max_active_downloads = Math.min(
        100,
        Math.max(0, Math.round(Number(next.max_active_downloads) || 0)),
      );
      settings.max_active_seeds = Math.min(
        100,
        Math.max(0, Math.round(Number(next.max_active_seeds) || 0)),
      );
      if (!settings.auto_shutdown) {
        autoPending = null;
      }
      settings.theme = next.theme === "light" ? "light" : "dark";
      app.listen_port = settings.listen_port === 0 ? 51413 : settings.listen_port;
      emitAuto();
      emit();
      return { ...settings };
    },

    revealTorrent: async () => {
      await delay(100);
    },

    openExternal: async () => {
      await delay(100);
    },

    bindFileDrop: (handlers: FileDropHandlers) => {
      let depth = 0;

      const hasFiles = (event: DragEvent): boolean =>
        Boolean(event.dataTransfer && event.dataTransfer.types.includes("Files"));

      const onDragEnter = (event: DragEvent) => {
        if (!hasFiles(event)) {
          return;
        }
        event.preventDefault();
        depth += 1;
        if (depth === 1) {
          handlers.onEnter();
        }
      };

      const onDragOver = (event: DragEvent) => {
        if (!hasFiles(event)) {
          return;
        }
        event.preventDefault();
        if (event.dataTransfer) {
          event.dataTransfer.dropEffect = "copy";
        }
      };

      const onDragLeave = () => {
        if (depth === 0) {
          return;
        }
        depth = Math.max(0, depth - 1);
        if (depth === 0) {
          handlers.onLeave();
        }
      };

      const onDrop = (event: DragEvent) => {
        event.preventDefault();
        depth = 0;
        handlers.onLeave();
        const files = Array.from(event.dataTransfer?.files ?? []);
        const sources = files
          .filter((file) => /\.torrent$/i.test(file.name))
          .map((file) => ({ kind: "path" as const, path: file.name }));
        handlers.onDrop(sources);
      };

      const onDragEnd = () => {
        depth = 0;
        handlers.onLeave();
      };

      window.addEventListener("dragenter", onDragEnter);
      window.addEventListener("dragover", onDragOver);
      window.addEventListener("dragleave", onDragLeave);
      window.addEventListener("drop", onDrop);
      window.addEventListener("dragend", onDragEnd);

      return () => {
        window.removeEventListener("dragenter", onDragEnter);
        window.removeEventListener("dragover", onDragOver);
        window.removeEventListener("dragleave", onDragLeave);
        window.removeEventListener("drop", onDrop);
        window.removeEventListener("dragend", onDragEnd);
      };
    },
  };
}
