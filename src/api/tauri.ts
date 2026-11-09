import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AddOptions,
  AddResult,
  AppPayload,
  AutoShutdownState,
  SettingsDto,
  TorrentPreview,
  TorrentSource,
} from "./types";
import type { EngineApi, FileDropHandlers } from "./engine";
import { basename } from "../lib/format";

/** На проводе источник — просто строка: путь к файлу или magnet-ссылка. */
function sourceOf(source: TorrentSource): string {
  return source.kind === "path" ? source.path : source.link;
}

function nameOf(source: TorrentSource): string {
  if (source.kind === "magnet") {
    const query = source.link.slice(source.link.indexOf("?") + 1);
    const dn = new URLSearchParams(query).get("dn");
    return dn?.trim() || "Magnet-загрузка";
  }
  return basename(source.path).replace(/\.torrent$/i, "") || "Новый торрент";
}

/**
 * Реальный движок: команды Tauri и события engine://state, engine://auto-shutdown
 * и engine://open-source (1.4: сигнал «открыли .torrent/magnet извне»).
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

    preview: (source) => invoke<TorrentPreview>("preview_torrent", { source: sourceOf(source) }),

    addTorrent: async (source: TorrentSource, options: AddOptions): Promise<AddResult> => {
      const id = await invoke<number>("add_torrent", {
        source: sourceOf(source),
        savePath: options.savePath,
        files: options.files,
      });
      return { id, name: nameOf(source) };
    },

    setFileSelection: (id, files) => invoke<void>("set_file_selection", { id, files }),

    setFilePriorities: (id, priorities) =>
      invoke<void>("set_file_priorities", { id, priorities }),

    recheckTorrent: (id) => invoke<void>("recheck_torrent", { id }),

    setTorrentLimits: (id, downKib, upKib) =>
      invoke<void>("set_torrent_limits", { id, downKib, upKib }),

    pickTorrentFiles: async () => {
      const paths = await invoke<string[]>("pick_torrent_files");
      return paths.map((path) => ({ kind: "path" as const, path }));
    },

    pickDownloadDir: () => invoke<string | null>("pick_download_dir"),
    pickWatchDir: () => invoke<string | null>("pick_watch_dir"),

    pause: (id) => invoke<void>("pause_torrent", { id }),
    resume: (id) => invoke<void>("resume_torrent", { id }),
    pauseAll: () => invoke<void>("pause_all"),
    resumeAll: () => invoke<void>("resume_all"),
    remove: (id, deleteFiles) => invoke<void>("remove_torrent", { id, deleteFiles }),

    getSettings: () => invoke<SettingsDto>("get_settings"),
    saveSettings: (settings) => invoke<SettingsDto>("set_settings", { settings }),

    getAutoShutdown: () => invoke<AutoShutdownState>("get_auto_shutdown"),
    setAutoShutdown: (enabled) => invoke<AutoShutdownState>("set_auto_shutdown", { enabled }),
    cancelAutoShutdown: () => invoke<AutoShutdownState>("cancel_auto_shutdown"),

    subscribeAutoShutdown: async (listener) => {
      const unlisten = await listen<AutoShutdownState>("engine://auto-shutdown", (event) => {
        listener(event.payload);
      });
      return unlisten;
    },

    revealTorrent: (id) => invoke<void>("reveal_torrent", { id }),
    openExternal: (url) => invoke<void>("open_external", { url }),

    // 1.4: .torrent/magnet, открытый из проводника. Событие — только сигнал,
    // сам источник забирается командой (он одноразовый).
    takeOpenRequest: () => invoke<string | null>("take_open_request"),
    subscribeOpenRequest: async (listener) => {
      const unlisten = await listen("engine://open-source", () => {
        listener();
      });
      return unlisten;
    },
    quitApp: () => invoke<void>("quit_app"),

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
            handlers.onDrop(paths.map((path) => ({ kind: "path" as const, path })));
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
