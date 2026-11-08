//! Выбор файлов торрента и вычисление «нужных» кусков (wanted).
//!
//! Нужный кусок — тот, что пересекается хотя бы с одним выбранным файлом. Для
//! торрента из одного файла и для полного выбора wanted совпадает с полным
//! набором кусков, поэтому поведение 1.0 сохраняется. Здесь же считается
//! действующий лимит скорости (глобальный + торрентный).

use crate::bitfield::Bitfield;
use crate::engine::FilePriority;
use crate::metainfo::Info;

/// Текущий выбор файлов торрента.
///
/// Хранится в `TorrentHandle` и копируется в рантайм (в пикер), чтобы снимок
/// состояния и планировщик видели один и тот же набор.
#[derive(Debug, Clone)]
pub(crate) struct Selection {
    /// Выбран ли файл; индекс совпадает с `Info::files`.
    files: Vec<bool>,
    /// Приоритет каждого файла; индекс совпадает с `Info::files`.
    priorities: Vec<FilePriority>,
    /// Куски, которые нужно скачать.
    pieces: Bitfield,
    /// Уровень приоритета каждого куска: 0 — низкий, 1 — обычный, 2 — высокий.
    levels: Vec<u8>,
    /// Суммарная длина выбранных файлов.
    bytes: u64,
}

impl Selection {
    /// Пустой выбор без файлов (метаданные magnet ещё не получены).
    pub(crate) fn empty() -> Selection {
        Selection {
            files: Vec::new(),
            priorities: Vec::new(),
            pieces: Bitfield::new(0),
            levels: Vec::new(),
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
        let priorities = files
            .iter()
            .map(|selected| {
                if *selected {
                    FilePriority::Normal
                } else {
                    FilePriority::Skip
                }
            })
            .collect();
        Selection::build(info, priorities)
    }

    /// Выбор из приоритетов файлов; длина среза должна совпадать с числом
    /// файлов торрента, иначе выбор считается некорректным (`None`).
    pub(crate) fn from_priorities(info: &Info, priorities: &[FilePriority]) -> Option<Selection> {
        if priorities.is_empty() || priorities.len() != info.files.len() {
            return None;
        }
        Some(Selection::build(info, priorities.to_vec()))
    }

    /// Собирает выбор: нужны все файлы, кроме `Skip`, куски и их уровни
    /// приоритета считаются по файлам.
    fn build(info: &Info, priorities: Vec<FilePriority>) -> Selection {
        let files: Vec<bool> = priorities
            .iter()
            .map(|priority| *priority != FilePriority::Skip)
            .collect();
        let pieces = wanted_pieces(info, &files);
        let levels = piece_levels(info, &priorities);
        let bytes = info
            .files
            .iter()
            .zip(files.iter())
            .filter(|(_, selected)| **selected)
            .map(|(file, _)| file.length)
            .sum();
        Selection {
            files,
            priorities,
            pieces,
            levels,
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

    /// Приоритеты файлов (по индексу).
    pub(crate) fn priorities(&self) -> &[FilePriority] {
        &self.priorities
    }

    /// Уровни приоритета кусков: 0 — низкий, 1 — обычный, 2 — высокий.
    pub(crate) fn levels(&self) -> &[u8] {
        &self.levels
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

/// Уровень приоритета куска: максимум по файлам, которые его пересекают.
///
/// `Skip`-файлы не участвуют в выборе кусков, поэтому кусок, нужный только им,
/// получает низкий уровень и никогда не запрашивается (его нет в wanted).
pub(crate) fn piece_levels(info: &Info, priorities: &[FilePriority]) -> Vec<u8> {
    let count = info.piece_count();
    let mut levels = vec![0u8; count];
    if priorities.len() != info.files.len() || info.files.is_empty() {
        return levels;
    }
    // Файлы идут по возрастанию смещения — одного указателя достаточно, как и
    // в `wanted_pieces`.
    let mut first = 0usize;
    for (index, slot) in levels.iter_mut().enumerate() {
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
        let mut level = 0u8;
        while probe < info.files.len() {
            let file = &info.files[probe];
            if file.offset >= end {
                break;
            }
            if file.length > 0 {
                level = level.max(level_of(priorities[probe]));
            }
            probe += 1;
        }
        *slot = level;
    }
    levels
}

/// Числовой уровень приоритета (0 — низкий, 1 — обычный, 2 — высокий).
fn level_of(priority: FilePriority) -> u8 {
    match priority {
        FilePriority::Skip | FilePriority::Low => 0,
        FilePriority::Normal => 1,
        FilePriority::High => 2,
    }
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
    fn priorities_mark_wanted_files_and_piece_levels() {
        let info = sample();
        let priorities = [FilePriority::Low, FilePriority::High, FilePriority::Skip];
        let selection = Selection::from_priorities(&info, &priorities).expect("выбор корректен");
        assert_eq!(selection.files(), &[true, true, false]);
        assert_eq!(selection.bytes(), 8);
        assert_eq!(selection.priorities(), &priorities[..]);
        // Куски файлов a (0,1) — низкий, b (2,3) — высокий, c (4,5) — не нужны.
        assert_eq!(selection.levels(), &[0, 0, 2, 2, 0, 0]);
        assert!(selection.pieces().get(3));
        assert!(!selection.pieces().get(4));
        assert!(!selection.pieces().get(5));
    }

    #[test]
    fn priorities_reject_wrong_length() {
        let info = sample();
        assert!(Selection::from_priorities(&info, &[FilePriority::Normal; 2]).is_none());
        assert!(Selection::from_priorities(&info, &[FilePriority::Normal; 4]).is_none());
        assert!(Selection::from_priorities(&info, &[]).is_none());
        // Полный набор обычных приоритетов — полный выбор, как `Selection::all`.
        let full =
            Selection::from_priorities(&info, &[FilePriority::Normal; 3]).expect("корректно");
        assert_eq!(full.bytes(), info.total_length);
        assert_eq!(full.levels(), &[1; 6]);
    }

    #[test]
    fn shared_piece_takes_highest_priority() {
        // Куски по 3 байта: [0..3) [3..6) [6..9) [9..12).
        let mut info = sample();
        info.piece_length = 3;
        info.pieces = vec![InfoHash::new([0u8; 20]); 4];
        // Файл b — [4..8): пересекает куски 1 и 2. Куски 1 общий для a и b,
        // поэтому получает высокий приоритет b, а не низкий a.
        let priorities = [FilePriority::Low, FilePriority::High, FilePriority::Skip];
        let selection = Selection::from_priorities(&info, &priorities).expect("выбор корректен");
        assert_eq!(selection.levels(), &[0, 2, 2, 0]);
        assert_eq!(selection.files(), &[true, true, false]);
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
