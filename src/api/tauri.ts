import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { AddResult, AppPayload, SettingsDto, TorrentInput } from "./types";
import type { EngineApi, FileDropHandlers } from "./engine";
import { basename } from "../lib/format";

/**
 * Реальный движок: команды Tauri и событие engine://state.
 * Все аргументы — camelCase, как их ждёт #[tauri::command] на стороне Rust.
 */
export async function createTauriApi(): Promise<EngineApi> {
  return {
    mode: "tauri",

    init: () => invoke<AppPayload>("get_state"),

    subscribe: async (listener) => {
      const unlisten = await listen<AppPayload>("engine://state", (event) => {
        listener(event.payload);
      });
      return unlisten;
    },

    addTorrent: async (input: TorrentInput): Promise<AddResult> => {
      if (input.kind === "file") {
        const id = await invoke<number>("add_torrent_file", { path: input.path });
        return { id, name: basename(input.path).replace(/\.torrent$/i, "") };
      }
      const id = await invoke<number>("add_torrent_bytes", {
        bytes: Array.from(input.bytes),
        nameHint: input.name.replace(/\.torrent$/i, ""),
      });
      return { id, name: basename(input.name).replace(/\.torrent$/i, "") };
    },

    addMagnet: async (link) => {
      const id = await invoke<number>("add_magnet", { link });
      return { id, name: "Magnet-загрузка" };
    },

    pickTorrentFiles: async () => {
      const paths = await invoke<string[]>("pick_torrent_files");
      return paths.map((path) => ({ kind: "file" as const, path }));
    },

    pickDownloadDir: () => invoke<string | null>("pick_download_dir"),

    pause: (id) => invoke<void>("pause_torrent", { id }),
    resume: (id) => invoke<void>("resume_torrent", { id }),
    pauseAll: () => invoke<void>("pause_all"),
    resumeAll: () => invoke<void>("resume_all"),
    remove: (id, deleteFiles) => invoke<void>("remove_torrent", { id, deleteFiles }),

    getSettings: () => invoke<SettingsDto>("get_settings"),
    saveSettings: (settings) => invoke<SettingsDto>("set_settings", { settings }),

    revealTorrent: (id) => invoke<void>("reveal_torrent", { id }),
    openDownloadDir: () => invoke<void>("open_download_dir"),
    openExternal: (url) => invoke<void>("open_external", { url }),

    /**
     * Нативное перетаскивание Tauri: event.payload содержит пути.
     * Если API webview недоступен — молча остаёмся без drag&drop.
     */
    bindFileDrop: (handlers: FileDropHandlers) => {
      let unlisten: (() => void) | undefined;
      let disposed = false;

      void (async () => {
        try {
          const { getCurrentWebview } = await import("@tauri-apps/api/webview");
          const webview = getCurrentWebview();
          const stop = await webview.onDragDropEvent((event) => {
            if (disposed) {
              return;
            }
            const payload = event.payload;
            if (payload.type === "enter" || payload.type === "over") {
              handlers.onEnter();
              return;
            }
            if (payload.type === "leave") {
              handlers.onLeave();
              return;
            }
            handlers.onLeave();
            const paths = payload.paths.filter((path) => /\.torrent$/i.test(path));
            handlers.onDrop(paths.map((path) => ({ kind: "file" as const, path })));
          });
          if (disposed) {
            stop();
          } else {
            unlisten = stop;
          }
        } catch {
          // webview API недоступен — оставляем без нативного drag&drop.
        }
      })();

      return () => {
        disposed = true;
        unlisten?.();
        unlisten = undefined;
      };
    },
  };
}
