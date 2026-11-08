//! Хранилище кусков: раскладка торрента по файлам на диске.
//!
//! Торрент — это непрерывный поток байт, нарезанный на куски (piece). Multi-file
//! торрент раскладывает этот поток по нескольким файлам, границы кусков при этом
//! не совпадают с границами файлов.
//!
//! [`Storage`] переводит глобальное смещение торрента в (файл, смещение внутри
//! файла) и делает запись/чтение без загрузки целиком.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::metainfo::Info;

/// Флаги совместного доступа Windows (`share_mode`). Значения совпадают с
/// `FILE_SHARE_*` из WinAPI; отдельный крейт ради трёх констант не тянем.
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(windows)]
const FILE_SHARE_DELETE: u32 = 0x0000_0004;

/// Режим совместного доступа для файлов торрента: пока клиент держит файл, его
/// должны мочь читать другие программы (просмотр видео, антивирус), а после
/// завершения — переименовывать и удалять в проводнике.
#[cfg(windows)]
const TORRENT_SHARE_MODE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Открывает файл торрента для чтения и записи, создавая его при необходимости.
///
/// Существующий файл не обрезается (`truncate(false)`): данные пользователя не
/// удаляются. На Windows режим совместного доступа задаётся явно.
fn open_read_write(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    options.share_mode(TORRENT_SHARE_MODE);
    options.open(path)
}

/// Открывает существующий файл торрента только для чтения.
///
/// На Windows это то же, что [`File::open`], но с явно заданным общим доступом.
fn open_read_only(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.share_mode(TORRENT_SHARE_MODE);
    options.open(path)
}

/// Один файл торрента.
struct Slot {
    /// Абсолютный путь (root + относительный путь торрента).
    path: PathBuf,
    /// Глобальное смещение начала файла в потоке торрента.
    offset: u64,
    length: u64,
    file: File,
    /// Дескриптор открыт только на чтение: запись запрещена до
    /// [`Storage::acquire_writes`].
    read_only: bool,
}

/// Хранилище торрента в выбранном каталоге.
pub struct Storage {
    root: PathBuf,
    slots: Vec<Slot>,
    total_length: u64,
}

impl Storage {
    /// Создаёт каталоги и файлы, при необходимости расширяет их до нужного
    /// размера (`set_len`), чтобы последующие записи не фрагментировали диск.
    ///
    /// Существующие файлы не обрезаются, если они больше нужного размера:
    /// данные пользователя не удаляются.
    pub fn prepare(root: &Path, info: &Info) -> io::Result<Storage> {
        let mut slots = Vec::with_capacity(info.files.len());
        let mut offset = 0u64;
        for entry in &info.files {
            let path = join_safe(root, &entry.path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = open_read_write(&path)?;
            let current = file.metadata()?.len();
            if current < entry.length {
                file.set_len(entry.length)?;
            }
            slots.push(Slot {
                path,
                offset,
                length: entry.length,
                file,
                read_only: false,
            });
            offset += entry.length;
        }
        Ok(Storage {
            root: root.to_path_buf(),
            slots,
            total_length: offset,
        })
    }

    /// Готовит хранилище только для чтения (раздача): файлы должны существовать.
    pub fn open_existing(root: &Path, info: &Info) -> io::Result<Storage> {
        let mut slots = Vec::with_capacity(info.files.len());
        let mut offset = 0u64;
        for entry in &info.files {
            let path = join_safe(root, &entry.path)?;
            let file = open_read_write(&path)?;
            slots.push(Slot {
                path,
                offset,
                length: entry.length,
                file,
                read_only: false,
            });
            offset += entry.length;
        }
        Ok(Storage {
            root: root.to_path_buf(),
            slots,
            total_length: offset,
        })
    }

    /// Общий размер всех файлов торрента.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Корневой каталог, внутри которого лежат файлы.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Пути файлов торрента (для UI и для «открыть каталог»).
    pub fn paths(&self) -> Vec<PathBuf> {
        self.slots.iter().map(|s| s.path.clone()).collect()
    }

    /// Пишет данные по глобальному смещению торрента, разбивая запись по файлам.
    pub fn write_at(&self, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
        if offset + data.len() as u64 > self.total_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "запись за пределами торрента",
            ));
        }
        if self.slots.iter().any(|slot| slot.read_only) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "файлы торрента открыты только для чтения",
            ));
        }
        while !data.is_empty() {
            let slot = self.slot_at(offset).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "смещение вне торрента")
            })?;
            let within = offset - slot.offset;
            let available = (slot.length - within) as usize;
            let n = available.min(data.len());
            let mut file = &slot.file;
            file.seek(SeekFrom::Start(within))?;
            file.write_all(&data[..n])?;
            offset += n as u64;
            data = &data[n..];
        }
        Ok(())
    }

    /// Читает диапазон по глобальному смещению торрента.
    pub fn read_at(&self, mut offset: u64, mut len: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while len > 0 {
            let slot = match self.slot_at(offset) {
                Some(s) => s,
                None => break,
            };
            let within = offset - slot.offset;
            let available = (slot.length - within) as usize;
            let n = available.min(len);
            let mut file = &slot.file;
            file.seek(SeekFrom::Start(within))?;
            let mut buf = vec![0u8; n];
            file.read_exact(&mut buf)?;
            out.extend_from_slice(&buf);
            offset += n as u64;
            len -= n;
        }
        Ok(out)
    }

    /// Сбрасывает буферы ОС на диск (вызывается после завершения куска).
    ///
    /// Освобождённые файлы (только для чтения) пропускаются: синхронизировать
    /// нечего, а `sync_data` на дескрипторе без права записи на Windows падает.
    pub fn flush(&self) -> io::Result<()> {
        for slot in &self.slots {
            if slot.read_only {
                continue;
            }
            slot.file.sync_data()?;
        }
        Ok(())
    }

    /// Освобождает файлы: сбрасывает буферы на диск и переоткрывает их только
    /// для чтения.
    ///
    /// Нужно, когда торрент завершён, стоит в очереди или на паузе: Windows не
    /// даёт запустить скачанный `.exe` (а также переименовать или удалить файл),
    /// пока его держит дескриптор с правом записи. Чтение при этом продолжает
    /// работать — раздача и проверка кусков не страдают.
    ///
    /// Идемпотентен: повторный вызов ничего не делает.
    pub fn release_writes(&mut self) -> io::Result<()> {
        for slot in &mut self.slots {
            if slot.read_only {
                continue;
            }
            // Буферы ОС сохраняем: после переоткрытия дескриптор уже другой.
            slot.file.sync_data()?;
            slot.file = open_read_only(&slot.path)?;
            slot.read_only = true;
        }
        Ok(())
    }

    /// Снова открывает файлы для записи (возобновление загрузки).
    ///
    /// Идемпотентен: если запись уже разрешена, ничего не делает.
    pub fn acquire_writes(&mut self) -> io::Result<()> {
        for slot in &mut self.slots {
            if !slot.read_only {
                continue;
            }
            // Синхронизировать нечего: при освобождении буферы уже сброшены, а
            // `sync_data` на дескрипторе без права записи на Windows падает.
            slot.file = open_read_write(&slot.path)?;
            slot.read_only = false;
        }
        Ok(())
    }

    /// Все файлы открыты только для чтения (запись освобождена).
    pub fn writes_released(&self) -> bool {
        self.slots.iter().all(|slot| slot.read_only)
    }

    fn slot_at(&self, offset: u64) -> Option<&Slot> {
        // Первый слот, который заканчивается позже `offset`. Такой поиск
        // автоматически пропускает пустые файлы (их конец равен началу) и тем
        // самым не даёт чтению оборваться на границе нулевого файла.
        let idx = self
            .slots
            .partition_point(|slot| slot.offset + slot.length <= offset);
        let slot = self.slots.get(idx)?;
        if offset >= slot.offset && offset < slot.offset + slot.length {
            Some(slot)
        } else {
            None
        }
    }
}

/// Склеивает корень и относительный путь, отсекая попытки выйти за пределы
/// каталога (`..`) — защита от вредоносных торрентов.
fn join_safe(root: &Path, relative: &str) -> io::Result<PathBuf> {
    let mut out = root.to_path_buf();
    for part in relative.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "путь торрента пытается выйти за пределы каталога загрузки",
                ))
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::FileEntry;

    /// Один файл в 64 байта: два куска по 32.
    fn sample() -> Info {
        Info {
            name: "sample".to_string(),
            piece_length: 32,
            pieces: vec![crate::metainfo::InfoHash::new([0u8; 20]); 2],
            files: vec![FileEntry {
                path: "sample.bin".to_string(),
                length: 64,
                offset: 0,
            }],
            total_length: 64,
            private: false,
        }
    }

    #[test]
    fn released_storage_reads_but_does_not_write() {
        let dir = tempfile::tempdir().expect("временный каталог");
        let mut storage = Storage::prepare(dir.path(), &sample()).expect("хранилище готово");
        storage.write_at(0, b"hello").expect("запись работает");
        assert!(!storage.writes_released());

        storage.release_writes().expect("файлы освобождены");
        assert!(storage.writes_released());
        // Повторный вызов идемпотентен.
        storage.release_writes().expect("повторное освобождение");
        // Чтение работает и после освобождения (нужно раздаче и перепроверке).
        assert_eq!(storage.read_at(0, 5).expect("чтение работает"), b"hello");
        let err = storage
            .write_at(5, b"!")
            .expect_err("запись при освобождённых файлах запрещена");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        storage
            .acquire_writes()
            .expect("файлы снова доступны для записи");
        assert!(!storage.writes_released());
        storage.write_at(5, b"!").expect("запись снова работает");
        assert_eq!(storage.read_at(0, 6).expect("чтение работает"), b"hello!");
    }

    /// Windows не даёт открыть занятый файл с другим режимом общего доступа —
    /// так ведёт себя, например, загрузчик образов. Пока торрент держит файл
    /// для записи, открыть его «как чужая программа» нельзя; после
    /// `release_writes` — можно.
    #[cfg(windows)]
    #[test]
    fn released_files_are_readable_by_other_programs() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().expect("временный каталог");
        let mut storage = Storage::prepare(dir.path(), &sample()).expect("хранилище готово");
        storage.write_at(0, b"data").expect("запись работает");
        let path = dir.path().join("sample.bin");

        let open_foreign = || {
            OpenOptions::new()
                .access_mode(0x8000_0000) // GENERIC_READ
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                .open(&path)
        };
        assert!(
            open_foreign().is_err(),
            "занятый на запись файл не должен открываться чужой программой"
        );

        storage.release_writes().expect("файлы освобождены");
        let file = open_foreign().expect("освобождённый файл открывается на чтение");
        drop(file);
    }
}
