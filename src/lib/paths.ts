/**
 * Работа с путями в диалоге добавления торрента.
 *
 * Windows не разрешает в именах папок символы `<>:"/\|?*` и управляющие байты,
 * а ещё запрещает имена вроде `CON` и заканчивающиеся точкой/пробелом. Название
 * торрента приходит извне, поэтому его нельзя использовать как имя каталога
 * «как есть»: пользователь получил бы ошибку создания папки.
 */

/** Символы, которые Windows не принимает в имени файла или папки. */
const INVALID_CHARS = /[<>:"/\\|?*\u0000-\u001F]/g;
/** Зарезервированные имена устройств (без учёта регистра и расширения). */
const RESERVED_NAMES = /^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/i;
/** Разумный предел имени папки: путь внутри торрента тоже должен уложиться. */
const MAX_FOLDER_LEN = 80;

/**
 * Имя папки для торрента по его названию.
 *
 * Запрещённые символы заменяются пробелом, повторы пробелов схлопываются,
 * хвостовые точки и пробелы срезаются (Windows их не любит), а пустое или
 * зарезервированное имя заменяется на безопасное.
 */
export function torrentFolderName(name: string): string {
  let cleaned = name.replace(INVALID_CHARS, " ").replace(/\s+/g, " ").trim();
  cleaned = cleaned.replace(/[. ]+$/u, "");
  if (cleaned.length > MAX_FOLDER_LEN) {
    cleaned = cleaned.slice(0, MAX_FOLDER_LEN).trim().replace(/[. ]+$/u, "");
  }
  if (cleaned === "") {
    return "torrent";
  }
  // `CON` и подобные опасны и с расширением (`CON.txt`), поэтому добавляем хвост.
  if (RESERVED_NAMES.test(cleaned)) {
    return `${cleaned}-torrent`;
  }
  return cleaned;
}

/**
 * Склеивает каталог и имя папки, не плодя двойных разделителей.
 *
 * Разделитель берётся по тому, что уже есть в пути: если пользователь ввёл
 * прямой слэш (как в проводнике тоже допустимо), его и используем.
 */
export function joinDir(base: string, name: string): string {
  const trimmed = base.replace(/[\\/]+$/u, "");
  if (trimmed === "") {
    return name;
  }
  const separator = trimmed.includes("/") && !trimmed.includes("\\") ? "/" : "\\";
  return `${trimmed}${separator}${name}`;
}

/**
 * Нужна ли отдельная папка под торрент.
 *
 * Несколько файлов — всегда папка: иначе они рассыпаются по общему каталогу
 * загрузок. Один файл кладём прямо в каталог: он и так назван именем торрента,
 * и лишний уровень «папка/файл с тем же именем» только мешает.
 */
export function needsTorrentFolder(fileCount: number): boolean {
  return fileCount > 1;
}

/** Путь целиком: каталог загрузки плюс папка торрента, когда она нужна. */
export function torrentTargetPath(base: string, name: string, fileCount: number): string {
  if (!needsTorrentFolder(fileCount) || name.trim() === "") {
    return base;
  }
  return joinDir(base, torrentFolderName(name));
}
