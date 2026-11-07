//! Битовая карта кусков (bitfield из peer wire protocol, BEP 3).
//!
//! Бит `i` (старший бит первого байта — кусок 0) означает наличие куска `i`.
//! Хвостовые биты последнего байта, выходящие за `len`, всегда остаются нулевыми.

/// Компактная карта кусков.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bitfield {
    bytes: Vec<u8>,
    len: usize,
}

impl Bitfield {
    /// Пустая карта на `len` кусков.
    pub fn new(len: usize) -> Bitfield {
        Bitfield {
            bytes: vec![0u8; len.div_ceil(8)],
            len,
        }
    }

    /// Карта со всеми установленными битами.
    pub fn all_set(len: usize) -> Bitfield {
        let mut bf = Bitfield::new(len);
        for i in 0..len {
            bf.set(i);
        }
        bf
    }

    /// Восстанавливает карту из байтов, пришедших от пира.
    pub fn from_bytes(bytes: Vec<u8>, len: usize) -> Bitfield {
        let needed = len.div_ceil(8);
        let mut bytes = bytes;
        if bytes.len() > needed {
            bytes.truncate(needed);
        } else if bytes.len() < needed {
            bytes.resize(needed, 0);
        }
        let mut bf = Bitfield { bytes, len };
        bf.clear_tail();
        bf
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Есть ли кусок.
    pub fn get(&self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        let byte = self.bytes[index / 8];
        byte & (0x80 >> (index % 8)) != 0
    }

    /// Устанавливает кусок. Возвращает `true`, если бит реально изменился.
    pub fn set(&mut self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        let byte = &mut self.bytes[index / 8];
        let mask = 0x80 >> (index % 8);
        if *byte & mask != 0 {
            return false;
        }
        *byte |= mask;
        true
    }

    /// Снимает кусок. Возвращает `true`, если бит реально изменился.
    pub fn unset(&mut self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        let byte = &mut self.bytes[index / 8];
        let mask = 0x80 >> (index % 8);
        if *byte & mask == 0 {
            return false;
        }
        *byte &= !mask;
        true
    }

    /// Сколько кусков установлено.
    pub fn count(&self) -> usize {
        let mut total = 0usize;
        for (i, byte) in self.bytes.iter().enumerate() {
            let remaining = self.len.saturating_sub(i * 8);
            let meaningful = remaining.min(8);
            let masked = if meaningful == 8 {
                *byte
            } else {
                *byte & !(0xFFu8 >> meaningful)
            };
            total += masked.count_ones() as usize;
        }
        total
    }

    /// Все куски установлены.
    pub fn is_complete(&self) -> bool {
        self.count() == self.len
    }

    /// Байты карты (как в peer wire `bitfield`).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Пересечение: сколько кусков есть у обоих.
    pub fn intersection_count(&self, other: &Bitfield) -> usize {
        let n = self.len.min(other.len);
        (0..n).filter(|i| self.get(*i) && other.get(*i)).count()
    }

    /// Сбрасывает все биты.
    pub fn clear(&mut self) {
        self.bytes.iter_mut().for_each(|b| *b = 0);
    }

    /// Обнуляет неиспользуемые хвостовые биты последнего байта.
    fn clear_tail(&mut self) {
        if self.len == 0 {
            return;
        }
        let tail_bits = self.len % 8;
        if tail_bits != 0 {
            let last = self.bytes.len() - 1;
            self.bytes[last] &= !(0xFFu8 >> tail_bits);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrip() {
        let mut bf = Bitfield::new(19);
        assert!(!bf.get(0));
        assert!(bf.set(0));
        assert!(!bf.set(0));
        bf.set(7);
        bf.set(8);
        bf.set(18);
        assert!(bf.get(0) && bf.get(7) && bf.get(8) && bf.get(18));
        assert!(!bf.get(1));
        assert_eq!(bf.count(), 4);
        assert!(!bf.is_complete());
    }

    #[test]
    fn byte_layout_is_msb_first() {
        let mut bf = Bitfield::new(16);
        bf.set(0);
        assert_eq!(bf.as_bytes()[0], 0b1000_0000);
        bf.set(1);
        assert_eq!(bf.as_bytes()[0], 0b1100_0000);
        bf.set(15);
        assert_eq!(bf.as_bytes()[1], 0b0000_0001);
    }

    #[test]
    fn tail_bits_are_ignored() {
        // 10 кусков = 2 байта, 6 хвостовых бит не должны считаться.
        let bf = Bitfield::from_bytes(vec![0xFF, 0xFF], 10);
        assert_eq!(bf.count(), 10);
        assert!(bf.is_complete());
        assert_eq!(bf.as_bytes()[1], 0b1100_0000);
    }

    #[test]
    fn from_bytes_normalizes_size() {
        let bf = Bitfield::from_bytes(vec![0xFF], 16);
        assert_eq!(bf.as_bytes().len(), 2);
        assert!(bf.get(7));
        assert!(!bf.get(8));
    }

    #[test]
    fn out_of_range_is_safe() {
        let bf = Bitfield::new(3);
        assert!(!bf.get(999));
    }

    #[test]
    fn unset_works() {
        let mut bf = Bitfield::all_set(9);
        assert!(bf.is_complete());
        assert!(bf.unset(4));
        assert!(!bf.unset(4));
        assert_eq!(bf.count(), 8);
    }
}
