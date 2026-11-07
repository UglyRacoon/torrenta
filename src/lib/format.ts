/**
 * Форматирование чисел для интерфейса.
 *
 * Форматтеры кэшируются: за один тик одни и те же скорости/размеры
 * запрашиваются десятки раз (список, детали, статус-бар), а Intl.NumberFormat
 * — не бесплатный. Кэш ограничен, чтобы не расти бесконечно.
 */

const BYTE_UNITS = ["Б", "КБ", "МБ", "ГБ", "ТБ", "ПБ"];

const numberFormats = new Map<number, Intl.NumberFormat>();

function nf(digits: number): Intl.NumberFormat {
  let format = numberFormats.get(digits);
  if (!format) {
    format = new Intl.NumberFormat("ru-RU", {
      maximumFractionDigits: digits,
      minimumFractionDigits: 0,
    });
    numberFormats.set(digits, format);
  }
  return format;
}

const CACHE_LIMIT = 4000;
const cache = new Map<string, string>();

function cached(key: string, compute: () => string): string {
  const hit = cache.get(key);
  if (hit !== undefined) {
    return hit;
  }
  const value = compute();
  if (cache.size >= CACHE_LIMIT) {
    cache.clear();
  }
  cache.set(key, value);
  return value;
}

export function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/** 0 → «0 Б», 1536 → «1,5 КБ», 6.1e9 → «5,7 ГБ». */
export function fmtBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 Б";
  }
  const rounded = Math.round(bytes);
  return cached(`bytes:${rounded}`, () => {
    let value = rounded;
    let unit = 0;
    while (value >= 1024 && unit < BYTE_UNITS.length - 1) {
      value /= 1024;
      unit += 1;
    }
    const digits = unit === 0 ? 0 : value < 10 ? 1 : 0;
    return `${nf(digits).format(value)} ${BYTE_UNITS[unit]}`;
  });
}

/** Байт/с → «12,4 МБ/с». */
export function fmtRate(bytesPerSecond: number): string {
  if (!Number.isFinite(bytesPerSecond) || bytesPerSecond <= 0) {
    return "0 Б/с";
  }
  return `${fmtBytes(bytesPerSecond)}/с`;
}

/** null → «—» (данных нет), 0 → «0 с» (реальный ноль). */
export function fmtEta(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds)) {
    return "—";
  }
  const total = Math.max(0, Math.floor(seconds));
  if (total < 1) {
    return "0 с";
  }
  if (total < 60) {
    return `${total} с`;
  }
  const minutes = Math.floor(total / 60);
  if (total < 3600) {
    return `${minutes} мин`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours} ч ${minutes % 60} мин`;
  }
  const days = Math.floor(hours / 24);
  return `${days} д ${hours % 24} ч`;
}

/** 0.384 → «38%», 1 → «100%». */
export function fmtPercent(progress: number, digits = 0): string {
  if (!Number.isFinite(progress)) {
    return "0%";
  }
  const value = clamp(progress, 0, 1) * 100;
  const rounded = digits === 0 ? Math.round(value) : Math.round(value * 10 ** digits) / 10 ** digits;
  return `${nf(digits).format(rounded)}%`;
}

export function fmtCount(value: number): string {
  if (!Number.isFinite(value)) {
    return "0";
  }
  return nf(0).format(Math.round(value));
}

/** Отношение отдано/получено с двумя знаками; «—», если ещё ничего не скачано. */
export function fmtRatio(uploaded: number, downloaded: number): string {
  if (!Number.isFinite(uploaded) || !Number.isFinite(downloaded) || downloaded <= 0) {
    return "—";
  }
  return nf(2).format(uploaded / downloaded);
}

/** Русская плюрализация: plural(2, "торрент", "торрента", "торрентов"). */
export function plural(count: number, one: string, few: string, many: string): string {
  const abs = Math.abs(count) % 100;
  const last = abs % 10;
  if (abs > 10 && abs < 20) {
    return many;
  }
  if (last > 1 && last < 5) {
    return few;
  }
  if (last === 1) {
    return one;
  }
  return many;
}

export function fmtTorrents(count: number): string {
  return `${fmtCount(count)} ${plural(count, "торрент", "торрента", "торрентов")}`;
}

export function fmtActive(count: number): string {
  return `${fmtCount(count)} ${plural(count, "активный", "активных", "активных")}`;
}

export function fmtPeers(count: number): string {
  return `${fmtCount(count)} ${plural(count, "пир", "пира", "пиров")}`;
}

export function fmtFiles(count: number): string {
  return `${fmtCount(count)} ${plural(count, "файл", "файла", "файлов")}`;
}

const dateFormat = new Intl.DateTimeFormat("ru-RU", {
  day: "numeric",
  month: "short",
  year: "numeric",
});

export function fmtDate(unixSeconds: number): string {
  if (!Number.isFinite(unixSeconds) || unixSeconds <= 0) {
    return "—";
  }
  return cached(`date:${Math.round(unixSeconds)}`, () =>
    dateFormat.format(new Date(unixSeconds * 1000)),
  );
}

/** «a1b2c3…1234» для узких мест; полное значение остаётся в title. */
export function truncateMiddle(text: string, max: number): string {
  if (text.length <= max) {
    return text;
  }
  const keep = Math.floor((max - 1) / 2);
  return `${text.slice(0, keep)}…${text.slice(-keep)}`;
}

export function isMagnetLink(value: string): boolean {
  return /^magnet:\?/i.test(value.trim());
}

export function basename(path: string): string {
  const normalized = path.replace(/\\/g, "/");
  const name = normalized.slice(normalized.lastIndexOf("/") + 1);
  return name || path;
}
