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
use std::path::{Path, PathBuf};

use crate::metainfo::Info;

/// Один файл торрента, открытый для записи.
struct Slot {
    /// Абсолютный путь (root + относительный путь торрента).
    path: PathBuf,
    /// Глобальное смещение начала файла в потоке торрента.
    offset: u64,
    length: u64,
    file: File,
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
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            let current = file.metadata()?.len();
            if current < entry.length {
                file.set_len(entry.length)?;
            }
            slots.push(Slot {
                path,
                offset,
                length: entry.length,
                file,
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
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            slots.push(Slot {
                path,
                offset,
                length: entry.length,
                file,
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
    pub fn flush(&self) -> io::Result<()> {
        for slot in &self.slots {
            slot.file.sync_data()?;
        }
        Ok(())
    }

    fn slot_at(&self, offset: u64) -> Option<&Slot> {
        // Бинарный поиск по смещениям; файлов обычно десятки — линейный тоже ок,
        // но binary_search гарантирует O(log n) на торрентах с тысячами файлов.
        let idx = match self.slots.binary_search_by(|s| s.offset.cmp(&offset)) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let slot = &self.slots[idx];
        if offset < slot.offset + slot.length {
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
