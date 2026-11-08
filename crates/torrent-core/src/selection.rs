//! Выбор файлов торрента и вычисление «нужных» кусков (wanted).
//!
//! Нужный кусок — тот, что пересекается хотя бы с одним выбранным файлом. Для
//! торрента из одного файла и для полного выбора wanted совпадает с полным
//! набором кусков, поэтому поведение 1.0 сохраняется. Здесь же считается
//! действующий лимит скорости (глобальный + торрентный).

use crate::bitfield::Bitfield;
use crate::metainfo::Info;

/// Текущий выбор файлов торрента.
///
/// Хранится в `TorrentHandle` и копируется в рантайм (в пикер), чтобы снимок
/// состояния и планировщик видели один и тот же набор.
#[derive(Debug, Clone)]
pub(crate) struct Selection {
    /// Выбран ли файл; индекс совпадает с `Info::files`.
    files: Vec<bool>,
    /// Куски, которые нужно скачать.
    pieces: Bitfield,
    /// Суммарная длина выбранных файлов.
    bytes: u64,
}

impl Selection {
    /// Пустой выбор без файлов (метаданные magnet ещё не получены).
    pub(crate) fn empty() -> Selection {
        Selection {
            files: Vec::new(),
            pieces: Bitfield::new(0),
            bytes: 0,
        }
    }

    /// Полный выбор: все файлы.
    pub(crate) fn all(info: &Info) -> Selection {
        Selection::from_flags(info, vec![true; info.files.len()])
    }

    /// Выбор из списка индексов файлов (`None` — все файлы).
    ///
    /// `None` в результате означает «некорректный выбор»: пустой список или
    /// индекс за пределами торрента. Вызывающая сторона превращает это в
    /// [`crate::engine::EngineError::Selection`].
    pub(crate) fn from_files(info: &Info, files: Option<&[usize]>) -> Option<Selection> {
        let flags = match files {
            None => vec![true; info.files.len()],
            Some(list) => {
                if list.is_empty() {
                    return None;
                }
                let mut flags = vec![false; info.files.len()];
                for index in list {
                    if *index >= flags.len() {
                        return None;
                    }
                    flags[*index] = true;
                }
                flags
            }
        };
        Some(Selection::from_flags(info, flags))
    }

    fn from_flags(info: &Info, files: Vec<bool>) -> Selection {
        let pieces = wanted_pieces(info, &files);
        let bytes = info
            .files
            .iter()
            .zip(files.iter())
            .filter(|(_, selected)| **selected)
            .map(|(file, _)| file.length)
            .sum();
        Selection {
            files,
            pieces,
            bytes,
        }
    }

    /// Битовая карта нужных кусков.
    pub(crate) fn pieces(&self) -> &Bitfield {
        &self.pieces
    }

    /// Суммарная длина выбранных файлов.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Выбранные файлы (по индексу).
    pub(crate) fn files(&self) -> &[bool] {
        &self.files
    }

    /// Куски известны (метаданные получены) и есть хотя бы один нужный байт.
    pub(crate) fn is_empty(&self) -> bool {
        self.pieces.len() == 0
    }
}

/// Куски, пересекающиеся хотя бы с одним выбранным файлом.
pub(crate) fn wanted_pieces(info: &Info, files: &[bool]) -> Bitfield {
    let count = info.piece_count();
    let mut wanted = Bitfield::new(count);
    if files.is_empty() || info.files.is_empty() {
        return wanted;
    }
    // Файлы идут по возрастанию смещения, куски тоже: достаточно одного
    // указателя, чтобы не перебирать все файлы для каждого куска.
    let mut first = 0usize;
    for index in 0..count {
        let start = info.piece_offset(index);
        let end = start + info.piece_size(index) as u64;
        while first < info.files.len() {
            let file = &info.files[first];
            if file.offset + file.length > start {
                break;
            }
            first += 1;
        }
        let mut probe = first;
        while probe < info.files.len() {
            let file = &info.files[probe];
            if file.offset >= end {
                break;
            }
            if file.length > 0 && files.get(probe).copied().unwrap_or(false) {
                wanted.set(index);
                break;
            }
            probe += 1;
        }
    }
    wanted
}

/// Сколько байт выбранных файлов уже на диске.
pub(crate) fn wanted_downloaded(info: &Info, have: &Bitfield, selection: &Selection) -> u64 {
    let count = info.piece_count();
    let mut total = 0u64;
    for index in 0..count {
        if !selection.pieces.get(index) || !have.get(index) {
            continue;
        }
        let start = info.piece_offset(index);
        let end = start + info.piece_size(index) as u64;
        let mut probe = info
            .files
            .partition_point(|file| file.offset + file.length <= start);
        while probe < info.files.len() {
            let file = &info.files[probe];
            if file.offset >= end {
                break;
            }
            if selection.files.get(probe).copied().unwrap_or(false) {
                let from = file.offset.max(start);
                let to = (file.offset + file.length).min(end);
                if to > from {
                    total += to - from;
                }
            }
            probe += 1;
        }
    }
    total
}

/// Действующий лимит скорости: минимум из глобального и торрентного,
/// где 0 означает «без ограничения».
pub(crate) fn effective_limit(global: u32, torrent: u32) -> u32 {
    match (global, torrent) {
        (0, torrent) => torrent,
        (global, 0) => global,
        (global, torrent) => global.min(torrent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::{FileEntry, InfoHash};

    /// Торрент из трёх файлов по 4 байта, куски по 2 байта: кусков шесть.
    fn sample() -> Info {
        let files = vec![
            FileEntry {
                path: "a.bin".to_string(),
                length: 4,
                offset: 0,
            },
            FileEntry {
                path: "b.bin".to_string(),
                length: 4,
                offset: 4,
            },
            FileEntry {
                path: "c.bin".to_string(),
                length: 4,
                offset: 8,
            },
        ];
        Info {
            name: "sample".to_string(),
            piece_length: 2,
            pieces: vec![InfoHash::new([0u8; 20]); 6],
            files,
            total_length: 12,
            private: false,
        }
    }

    #[test]
    fn selection_all_covers_every_piece() {
        let info = sample();
        let selection = Selection::all(&info);
        assert_eq!(selection.pieces().count(), info.piece_count());
        assert_eq!(selection.bytes(), info.total_length);
    }

    #[test]
    fn selection_of_middle_file_marks_its_pieces_only() {
        let info = sample();
        let selection = Selection::from_files(&info, Some(&[1])).expect("выбор корректен");
        let wanted: Vec<usize> = (0..info.piece_count())
            .filter(|index| selection.pieces().get(*index))
            .collect();
        assert_eq!(wanted, vec![2, 3]);
        assert_eq!(selection.bytes(), 4);
        assert_eq!(selection.files(), &[false, true, false]);
    }

    #[test]
    fn selection_rejects_empty_and_out_of_range() {
        let info = sample();
        assert!(Selection::from_files(&info, Some(&[])).is_none());
        assert!(Selection::from_files(&info, Some(&[3])).is_none());
        assert!(Selection::from_files(&info, None).is_some());
    }

    #[test]
    fn selection_handles_unaligned_pieces() {
        // Куски по 3 байта: [0..3) [3..6) [6..9) [9..12).
        let mut info = sample();
        info.piece_length = 3;
        info.pieces = vec![InfoHash::new([0u8; 20]); 4];
        info.total_length = 12;
        // Файл b — [4..8): пересекает куски 1 и 2.
        let selection = Selection::from_files(&info, Some(&[1])).expect("выбор корректен");
        let wanted: Vec<usize> = (0..4)
            .filter(|index| selection.pieces().get(*index))
            .collect();
        assert_eq!(wanted, vec![1, 2]);
    }

    #[test]
    fn wanted_downloaded_counts_only_selected_files() {
        let info = sample();
        let selection = Selection::from_files(&info, Some(&[1])).expect("выбор корректен");
        // Есть куски 0..4 (в том числе чужие файлы) — в зачёт идут только
        // байты выбранного файла.
        let mut have = Bitfield::new(info.piece_count());
        for index in 0..4 {
            have.set(index);
        }
        assert_eq!(wanted_downloaded(&info, &have, &selection), 4);
    }

    #[test]
    fn effective_limit_ignores_zero() {
        assert_eq!(effective_limit(0, 0), 0);
        assert_eq!(effective_limit(500, 0), 500);
        assert_eq!(effective_limit(0, 200), 200);
        assert_eq!(effective_limit(500, 200), 200);
        assert_eq!(effective_limit(200, 500), 200);
    }
}
