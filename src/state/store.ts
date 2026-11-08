import type { AppPayload, EngineMode, FilterKey, SortKey } from "../api/types";

export type Theme = "dark" | "light";

export interface AppState {
  mode: EngineMode;
  payload: AppPayload | null;
  ready: boolean;
  fatalError: string | null;
  filter: FilterKey;
  query: string;
  sort: SortKey;
  selectedId: number | null;
  /** Панель подробностей — отдельное состояние: выбор строки её не открывает. */
  detailsOpen: boolean;
  theme: Theme;
}

export type Listener = (state: AppState) => void;

/**
 * Одно хранилище состояния.
 *
 * Тики движка (700 мс) приходят пачками и буферизуются в requestAnimationFrame,
 * чтобы несколько событий подряд не перерисовывали DOM несколько раз.
 * Действия пользователя (фильтр, поиск, выбор) применяются синхронно — отклик
 * не должен ждать кадра.
 */
export class AppStore {
  #state: AppState;
  #listeners = new Set<Listener>();
  #pending: AppPayload | null = null;
  #frame = 0;

  constructor(initial: AppState) {
    this.#state = initial;
  }

  get state(): AppState {
    return this.#state;
  }

  subscribe(listener: Listener): () => void {
    this.#listeners.add(listener);
    return () => {
      this.#listeners.delete(listener);
    };
  }

  /** Первичная загрузка — без ожидания кадра, чтобы не мигнуть пустым списком. */
  hydrate(payload: AppPayload): void {
    this.#applyPayload(payload);
    this.#emit();
  }

  setPayload(payload: AppPayload): void {
    this.#pending = payload;
    if (this.#frame !== 0) {
      return;
    }
    this.#frame = requestAnimationFrame(() => {
      this.#frame = 0;
      const next = this.#pending;
      this.#pending = null;
      if (!next) {
        return;
      }
      this.#applyPayload(next);
      this.#emit();
    });
  }

  patch(partial: Partial<AppState>): void {
    this.#state = { ...this.#state, ...partial };
    this.#emit();
  }

  #applyPayload(payload: AppPayload): void {
    const selected = this.#state.selectedId;
    const stillExists = selected !== null && payload.torrents.some((torrent) => torrent.id === selected);
    this.#state = {
      ...this.#state,
      payload,
      ready: true,
      fatalError: null,
      selectedId: stillExists ? selected : null,
      detailsOpen: stillExists ? this.#state.detailsOpen : false,
    };
  }

  #emit(): void {
    for (const listener of [...this.#listeners]) {
      listener(this.#state);
    }
  }
}
