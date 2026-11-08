//! Выбор кусков для загрузки.
//!
//! Алгоритм: rarest-first (сначала самые редкие куски — они быстрее исчезнут из
//! сети), с ограничением числа одновременно загружаемых кусков и защитой от
//! «залипания» на медленном пире (таймаут возвращает кусок в пул).

use std::collections::HashSet;

use crate::bitfield::Bitfield;

/// Состояние одного куска.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceStatus {
    /// Куска нет, он свободен.
    Missing,
    /// Кусок уже скачан и проверен.
    Have,
    /// Кусок загружается (заблокирован одним из пиров).
    InProgress,
}

/// Планировщик кусков для одного торрента.
pub struct PiecePicker {
    piece_count: usize,
    have: Bitfield,
    status: Vec<PieceStatus>,
    /// Сколько пиров имеют кусок — для rarest-first.
    availability: Vec<u32>,
    /// Порядок кусков для «почти завершённых» торрентов (конец загрузки).
    in_progress_count: usize,
    /// Максимум кусков в работе одновременно.
    max_in_progress: usize,
    /// Нужные куски (пересекаются с выбранными файлами): только их и отдаём.
    wanted: Bitfield,
}

impl PiecePicker {
    /// Создаёт планировщик на `piece_count` кусков (все отсутствуют).
    pub fn new(piece_count: usize) -> PiecePicker {
        PiecePicker {
            piece_count,
            have: Bitfield::new(piece_count),
            status: vec![PieceStatus::Missing; piece_count],
            availability: vec![0; piece_count],
            in_progress_count: 0,
            max_in_progress: (piece_count / 40).clamp(4, 64),
            wanted: Bitfield::all_set(piece_count),
        }
    }

    /// Создаёт планировщик для раздачи, где все куски уже есть.
    pub fn new_complete(piece_count: usize) -> PiecePicker {
        let mut picker = PiecePicker::new(piece_count);
        for i in 0..piece_count {
            picker.have.set(i);
            picker.status[i] = PieceStatus::Have;
        }
        picker
    }

    pub fn piece_count(&self) -> usize {
        self.piece_count
    }

    /// Битовая карта имеющихся кусков.
    pub fn have(&self) -> &Bitfield {
        &self.have
    }

    pub fn have_mut(&mut self) -> &mut Bitfield {
        &mut self.have
    }

    /// Сколько кусков уже есть.
    pub fn have_count(&self) -> usize {
        self.have.count()
    }

    /// Все нужные куски получены.
    ///
    /// Ненужные куски (в невыбранных файлах) на «завершённость» не влияют:
    /// после частичного выбора торрент считается скачанным, как только на диске
    /// лежат все куски выбранных файлов.
    pub fn is_complete(&self) -> bool {
        (0..self.piece_count).all(|index| !self.wanted.get(index) || self.have.get(index))
    }

    /// Битовая карта нужных кусков.
    pub fn wanted(&self) -> &Bitfield {
        &self.wanted
    }

    /// Куски, которые надо скачать.
    pub fn wanted_count(&self) -> usize {
        (0..self.piece_count)
            .filter(|index| self.wanted.get(*index))
            .count()
    }

    /// Сколько нужных кусков уже есть.
    pub fn wanted_have_count(&self) -> usize {
        (0..self.piece_count)
            .filter(|index| self.wanted.get(*index) && self.have.get(*index))
            .count()
    }

    /// Заменяет набор нужных кусков (например, после смены выбора файлов).
    pub fn set_wanted(&mut self, wanted: &Bitfield) {
        let mut next = Bitfield::new(self.piece_count);
        for index in 0..self.piece_count {
            if wanted.get(index) {
                next.set(index);
            }
        }
        self.wanted = next;
    }

    /// Нужен ли кусок.
    pub fn is_wanted(&self, index: usize) -> bool {
        self.wanted.get(index)
    }

    pub fn status(&self, index: usize) -> PieceStatus {
        self.status
            .get(index)
            .copied()
            .unwrap_or(PieceStatus::Missing)
    }

    /// Помечает кусок как скачанный/потерянный (после проверки SHA-1).
    pub fn mark_have(&mut self, index: usize) {
        if index >= self.piece_count {
            return;
        }
        if self.status[index] == PieceStatus::InProgress {
            self.in_progress_count = self.in_progress_count.saturating_sub(1);
        }
        self.status[index] = PieceStatus::Have;
        self.have.set(index);
    }

    /// Возвращает кусок в пул (проверка не прошла или пир отключился).
    pub fn release(&mut self, index: usize) {
        if index >= self.piece_count {
            return;
        }
        if self.status[index] == PieceStatus::InProgress {
            self.in_progress_count = self.in_progress_count.saturating_sub(1);
        }
        if self.status[index] != PieceStatus::Have {
            self.status[index] = PieceStatus::Missing;
        }
    }

    /// Учитывает битовую карту нового пира (для rarest-first).
    pub fn add_peer(&mut self, peer_have: &Bitfield) {
        for i in 0..self.piece_count {
            if peer_have.get(i) {
                self.availability[i] = self.availability[i].saturating_add(1);
            }
        }
    }

    /// Убирает информацию о пире при отключении.
    pub fn remove_peer(&mut self, peer_have: &Bitfield) {
        for i in 0..self.piece_count {
            if peer_have.get(i) {
                self.availability[i] = self.availability[i].saturating_sub(1);
            }
        }
    }

    /// Учитывает появление куска у одного пира (для rarest-first).
    pub fn add_have(&mut self, index: usize) {
        if let Some(availability) = self.availability.get_mut(index) {
            *availability = availability.saturating_add(1);
        }
    }

    /// Учитывает пропажу куска у одного пира.
    pub fn remove_have(&mut self, index: usize) {
        if let Some(availability) = self.availability.get_mut(index) {
            *availability = availability.saturating_sub(1);
        }
    }

    /// Есть ли кусок у пира (кусок отсутствует локально).
    fn peer_has(&self, index: usize) -> bool {
        self.status.get(index) == Some(&PieceStatus::Missing)
    }

    /// Выбирает следующий кусок для загрузки у данного пира.
    ///
    /// * `peer_have` — что есть у пира;
    /// * `exclude` — куски, которые уже запрошены у других пиров (избегаем дублей).
    ///
    /// Куски вне набора нужных не выбираются никогда.
    pub fn pick(&mut self, peer_have: &Bitfield, exclude: &HashSet<usize>) -> Option<usize> {
        if self.is_complete() || self.in_progress_count >= self.max_in_progress {
            return None;
        }
        let index = self.best(peer_have, exclude, &self.wanted)?;
        self.claim(index)
    }

    /// То же, но набор нужных кусков задаётся снаружи (без изменения состояния).
    pub fn pick_wanted(
        &mut self,
        peer_have: &Bitfield,
        exclude: &HashSet<usize>,
        wanted: &Bitfield,
    ) -> Option<usize> {
        if self.in_progress_count >= self.max_in_progress {
            return None;
        }
        let index = self.best(peer_have, exclude, wanted)?;
        self.claim(index)
    }

    /// Лучший кандидат: rarest-first, на старте — последовательно с начала.
    fn best(
        &self,
        peer_have: &Bitfield,
        exclude: &HashSet<usize>,
        wanted: &Bitfield,
    ) -> Option<usize> {
        // Две трети — rarest-first, оставшаяся треть — последовательно с начала,
        // чтобы уже скачанные данные были полезны сразу (можно смотреть/раздавать).
        let sequential_bias = self.in_progress_count == 0;
        let mut best: Option<(usize, u32)> = None;
        for i in 0..self.piece_count {
            if !wanted.get(i) || !self.peer_has(i) || exclude.contains(&i) || !peer_have.get(i) {
                continue;
            }
            let avail = self.availability[i];
            best = match best {
                None => Some((i, avail)),
                Some((bi, ba)) => {
                    let better = if sequential_bias {
                        i < bi
                    } else {
                        avail < ba
                    };
                    if better {
                        Some((i, avail))
                    } else {
                        Some((bi, ba))
                    }
                }
            };
        }
        best.map(|(index, _)| index)
    }

    fn claim(&mut self, index: usize) -> Option<usize> {
        self.status[index] = PieceStatus::InProgress;
        self.in_progress_count += 1;
        Some(index)
    }

    /// Возвращает список отсутствующих кусков (для отладки и тестов).
    pub fn missing(&self) -> Vec<usize> {
        (0..self.piece_count)
            .filter(|i| self.status[*i] == PieceStatus::Missing)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_torrent_picks_nothing() {
        let mut picker = PiecePicker::new_complete(8);
        let all = Bitfield::new(8);
        assert!(picker.is_complete());
        assert_eq!(picker.pick(&all, &HashSet::new()), None);
    }

    #[test]
    fn picks_rarest_first_when_no_sequential_bias() {
        let mut picker = PiecePicker::new(4);
        // Пир №1 имеет куски 0,1,2; пир №2 — только 1 → кусок 1 самый редкий.
        let mut p1 = Bitfield::new(4);
        p1.set(0);
        p1.set(1);
        p1.set(2);
        let mut p2 = Bitfield::new(4);
        p2.set(1);
        picker.add_peer(&p1);
        picker.add_peer(&p2);
        // Сначала в работе ничего нет → последовательный режим: берём кусок 0.
        assert_eq!(picker.pick(&p1, &HashSet::new()), Some(0));
        // Теперь кусок 0 в работе → включается rarest-first. Куски 0 и 2 имеют
        // доступность 1 (кусок 0 пропускается как «в работе»), кусок 1 — 2.
        assert_eq!(picker.pick(&p1, &HashSet::new()), Some(2));
    }

    #[test]
    fn respects_exclude_and_peer_bitfield() {
        let mut picker = PiecePicker::new(3);
        let mut peer = Bitfield::new(3);
        peer.set(2);
        let mut exclude = HashSet::new();
        exclude.insert(2);
        assert_eq!(picker.pick(&peer, &exclude), None);
        exclude.clear();
        assert_eq!(picker.pick(&peer, &exclude), Some(2));
        // Повторно тот же кусок не выдаётся — он в работе.
        assert_eq!(picker.pick(&peer, &exclude), None);
        picker.release(2);
        assert_eq!(picker.pick(&peer, &exclude), Some(2));
    }

    #[test]
    fn in_progress_window_is_bounded() {
        let mut picker = PiecePicker::new(1000);
        let mut peer = Bitfield::new(1000);
        for i in 0..1000 {
            peer.set(i);
        }
        let mut picked = 0;
        while picker.pick(&peer, &HashSet::new()).is_some() {
            picked += 1;
            assert!(picked <= 64, "окно одновременно загружаемых кусков должно быть ограничено");
        }
        assert!(picked > 0);
    }

    #[test]
    fn never_picks_pieces_outside_wanted() {
        let mut picker = PiecePicker::new(4);
        let mut wanted = Bitfield::new(4);
        wanted.set(2);
        picker.set_wanted(&wanted);
        let mut peer = Bitfield::new(4);
        for i in 0..4 {
            peer.set(i);
        }
        assert!(!picker.is_wanted(1));
        assert_eq!(picker.pick(&peer, &HashSet::new()), Some(2));
        assert_eq!(picker.pick(&peer, &HashSet::new()), None);
    }

    #[test]
    fn completion_counts_wanted_pieces_only() {
        let mut picker = PiecePicker::new(4);
        let mut wanted = Bitfield::new(4);
        wanted.set(1);
        wanted.set(3);
        picker.set_wanted(&wanted);
        assert!(!picker.is_complete());
        // Наличие «чужих» кусков завершённости не даёт.
        picker.mark_have(0);
        picker.mark_have(2);
        assert!(!picker.is_complete());
        picker.mark_have(1);
        picker.mark_have(3);
        assert!(picker.is_complete());
        assert_eq!(picker.wanted_have_count(), 2);
        assert_eq!(picker.wanted_count(), 2);
        // В полном выборе завершённость по-прежнему требует все куски.
        let mut full = PiecePicker::new(4);
        full.mark_have(0);
        assert!(!full.is_complete());
        for index in 1..4 {
            full.mark_have(index);
        }
        assert!(full.is_complete());
    }
}
