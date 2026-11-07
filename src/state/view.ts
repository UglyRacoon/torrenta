import type { FilterKey, SortKey, TorrentSnapshot, TorrentState } from "../api/types";
import type { AppState } from "./store";

export const FILTER_LABELS: Record<FilterKey, string> = {
  all: "Все торренты",
  downloading: "Загрузки",
  seeding: "Раздачи",
  paused: "На паузе",
  completed: "Завершённые",
};

export type FilterCounts = Record<FilterKey, number>;

/** «Активен» = движок что-то делает: качает, раздаёт или ищет метаданные. */
export function isActiveState(state: TorrentState): boolean {
  return state === "downloading" || state === "seeding" || state === "metadata" || state === "queued";
}

export function matchesFilter(torrent: TorrentSnapshot, filter: FilterKey): boolean {
  switch (filter) {
    case "downloading":
      return torrent.state === "downloading" || torrent.state === "metadata" || torrent.state === "queued";
    case "seeding":
      return torrent.state === "seeding";
    case "paused":
      return torrent.state === "paused";
    case "completed":
      return torrent.state === "completed";
    default:
      return true;
  }
}

export function filterCounts(torrents: TorrentSnapshot[]): FilterCounts {
  const counts: FilterCounts = { all: torrents.length, downloading: 0, seeding: 0, paused: 0, completed: 0 };
  for (const torrent of torrents) {
    if (matchesFilter(torrent, "downloading")) {
      counts.downloading += 1;
    }
    if (matchesFilter(torrent, "seeding")) {
      counts.seeding += 1;
    }
    if (matchesFilter(torrent, "paused")) {
      counts.paused += 1;
    }
    if (matchesFilter(torrent, "completed")) {
      counts.completed += 1;
    }
  }
  return counts;
}

export function activeCount(torrents: TorrentSnapshot[]): number {
  let count = 0;
  for (const torrent of torrents) {
    if (isActiveState(torrent.state)) {
      count += 1;
    }
  }
  return count;
}

function compare(a: TorrentSnapshot, b: TorrentSnapshot, sort: SortKey): number {
  switch (sort) {
    case "name":
      return a.name.localeCompare(b.name, "ru", { sensitivity: "base" });
    case "progress":
      return b.progress - a.progress;
    case "speed":
      return b.down_rate + b.up_rate - (a.down_rate + a.up_rate);
    case "size":
      return b.total_bytes - a.total_bytes;
    default:
      return b.added_at - a.added_at;
  }
}

export function sortTorrents(torrents: TorrentSnapshot[], sort: SortKey): TorrentSnapshot[] {
  return torrents.slice().sort((a, b) => {
    const primary = compare(a, b, sort);
    if (primary !== 0) {
      return primary;
    }
    const added = b.added_at - a.added_at;
    return added !== 0 ? added : b.id - a.id;
  });
}

export function visibleTorrents(state: AppState): TorrentSnapshot[] {
  const payload = state.payload;
  if (!payload) {
    return [];
  }
  const query = state.query.trim().toLowerCase();
  const filtered = payload.torrents.filter((torrent) => {
    if (!matchesFilter(torrent, state.filter)) {
      return false;
    }
    if (!query) {
      return true;
    }
    return (
      torrent.name.toLowerCase().includes(query) ||
      torrent.info_hash.toLowerCase().includes(query)
    );
  });
  return sortTorrents(filtered, state.sort);
}

export function findTorrent(state: AppState, id: number | null): TorrentSnapshot | null {
  if (id === null || !state.payload) {
    return null;
  }
  return state.payload.torrents.find((torrent) => torrent.id === id) ?? null;
}
