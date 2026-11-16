/**
 * История каталогов загрузки для поля «Папка загрузки».
 *
 * Храним именно базовые каталоги (тот, что выбирает пользователь), а не
 * итоговый путь с папкой торрента: иначе в списке быстро появились бы десятки
 * одноразовых путей вида `D:\Загрузки\Фильм (2024)`. Память — в `localStorage`,
 * чтобы список переживал перезапуск и не занимал место в `settings.json`
 * (это удобство конкретного WebView-профиля, а не настройка движка).
 */

const STORAGE_KEY = "torrenta.downloadDirs";
/** Сколько последних каталогов показываем. */
export const RECENT_LIMIT = 10;

/** Читает историю: недавние сверху, мусор и дубликаты отбрасываются. */
export function loadRecentDirs(): string[] {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return [];
    }
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) {
      return [];
    }
    const seen = new Set<string>();
    const dirs: string[] = [];
    for (const item of parsed) {
      if (typeof item !== "string") {
        continue;
      }
      const dir = item.trim();
      const key = dir.toLowerCase();
      if (dir === "" || seen.has(key)) {
        continue;
      }
      seen.add(key);
      dirs.push(dir);
      if (dirs.length >= RECENT_LIMIT) {
        break;
      }
    }
    return dirs;
  } catch {
    // Битый JSON или запрет на localStorage — просто без истории.
    return [];
  }
}

/**
 * Добавляет каталог в историю (последний — первый).
 *
 * Возвращает обновлённый список; ошибки записи не влияют на добавление
 * торрента, потому что это вспомогательная функция.
 */
export function rememberDownloadDir(dir: string): string[] {
  const trimmed = dir.trim();
  if (trimmed === "") {
    return loadRecentDirs();
  }
  const key = trimmed.toLowerCase();
  const next = [trimmed, ...loadRecentDirs().filter((item) => item.toLowerCase() !== key)].slice(
    0,
    RECENT_LIMIT,
  );
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Квота или запрет записи: история просто не сохранится.
  }
  return next;
}
