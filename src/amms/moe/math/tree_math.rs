use alloy_primitives::U256;
use std::collections::HashMap;

use super::bit_math;

#[derive(Default)]
pub struct TreeUint24 {
    level0: U256,
    level1: HashMap<u16, U256>,
    level2: HashMap<u32, U256>,
}

impl TreeUint24 {
    pub fn contains(&self, id: u32) -> bool {
        let key2 = id >> 8;
        if let Some(leaves) = self.level2.get(&key2) {
            let mask = U256::from(1_u128) << (id & 0xff);
            (*leaves & mask) != U256::ZERO
        } else {
            false
        }
    }

    pub fn add(&mut self, id: u32) -> bool {
        let key2 = id >> 8;
        let bit = U256::from(1_u128) << (id & 0xff);
        let leaves = self.level2.entry(key2).or_insert(U256::ZERO);
        if (*leaves & bit).is_zero() {
            *leaves |= bit;
            let key1 = (key2 >> 8) as u16;
            let bit1 = U256::from(1_u128) << (key2 & 0xff);
            let level1 = self.level1.entry(key1).or_insert(U256::ZERO);
            if (*level1 & bit1).is_zero() {
                *level1 |= bit1;
                self.level0 |= U256::from(1_u128) << (key1 & 0xff);
            }
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, id: u32) -> bool {
        let key2 = id >> 8;
        let bit = U256::from(1_u128) << (id & 0xff);
        if let Some(leaves) = self.level2.get_mut(&key2) {
            if (*leaves & bit).is_zero() {
                return false;
            }
            *leaves &= !bit;
            if leaves.is_zero() {
                self.level2.remove(&key2);
                let key1 = (key2 >> 8) as u16;
                let bit1 = U256::from(1_u128) << (key2 & 0xff);
                if let Some(level1) = self.level1.get_mut(&key1) {
                    *level1 &= !bit1;
                    if level1.is_zero() {
                        self.level1.remove(&key1);
                        self.level0 &= !(U256::from(1_u128) << (key1 & 0xff));
                    }
                }
            }
            true
        } else {
            false
        }
    }

    pub fn find_first_right(&self, id: u32) -> Option<u32> {
        let mut key2 = id >> 8;
        let mut bit = (id & 0xff) as u8;

        if bit != 0 {
            if let Some(leaves) = self.level2.get(&key2) {
                if let Ok(next_bit) = bit_math::closest_bit_right(*leaves, bit) {
                    if next_bit != u32::MAX {
                        return Some((key2 << 8) | next_bit);
                    }
                }
            }
        }

        let mut key1 = key2 >> 8;
        bit = (key2 & 0xff) as u8;
        if bit != 0 {
            if let Some(level1) = self.level1.get(&(key1 as u16)) {
                if let Ok(next_bit) = bit_math::closest_bit_right(*level1, bit) {
                    if next_bit != u32::MAX {
                        key2 = (key1 << 8) | next_bit;
                        if let Some(leaves) = self.level2.get(&key2) {
                            if let Ok(msb) = bit_math::most_significant_bit(*leaves) {
                                return Some((key2 << 8) | msb as u32);
                            }
                        }
                    }
                }
            }
        }

        bit = (key1 & 0xff) as u8;
        if bit != 0 {
            if let Ok(next_bit) = bit_math::closest_bit_right(self.level0, bit) {
                if next_bit != u32::MAX {
                    key1 = next_bit as u32;
                    if let Some(level1) = self.level1.get(&(key1 as u16)) {
                        if let Ok(msb) = bit_math::most_significant_bit(*level1) {
                            key2 = (key1 << 8) | msb as u32;
                            if let Some(leaves) = self.level2.get(&key2) {
                                if let Ok(msb2) = bit_math::most_significant_bit(*leaves) {
                                    return Some((key2 << 8) | msb2 as u32);
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }

    pub fn find_first_left(&self, id: u32) -> Option<u32> {
        let mut key2 = id >> 8;
        let mut bit = (id & 0xff) as u8;

        if bit != u8::MAX {
            if let Some(leaves) = self.level2.get(&key2) {
                if let Ok(next_bit) = bit_math::closest_bit_left(*leaves, bit) {
                    if next_bit != u32::MAX {
                        return Some((key2 << 8) | next_bit);
                    }
                }
            }
        }

        let mut key1 = key2 >> 8;
        bit = (key2 & 0xff) as u8;
        if bit != u8::MAX {
            if let Some(level1) = self.level1.get(&(key1 as u16)) {
                if let Ok(next_bit) = bit_math::closest_bit_left(*level1, bit) {
                    if next_bit != u32::MAX {
                        key2 = (key1 << 8) | next_bit;
                        if let Some(leaves) = self.level2.get(&key2) {
                            if let Ok(lsb) = bit_math::least_significant_bit(*leaves) {
                                return Some((key2 << 8) | lsb as u32);
                            }
                        }
                    }
                }
            }
        }

        bit = (key1 & 0xff) as u8;
        if bit != u8::MAX {
            if let Ok(next_bit) = bit_math::closest_bit_left(self.level0, bit) {
                if next_bit != u32::MAX {
                    key1 = next_bit as u32;
                    if let Some(level1) = self.level1.get(&(key1 as u16)) {
                        if let Ok(lsb) = bit_math::least_significant_bit(*level1) {
                            key2 = (key1 << 8) | lsb as u32;
                            if let Some(leaves) = self.level2.get(&key2) {
                                if let Ok(lsb2) = bit_math::least_significant_bit(*leaves) {
                                    return Some((key2 << 8) | lsb2 as u32);
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_contains_remove() {
        let mut tree = TreeUint24::default();
        assert!(!tree.contains(100));
        assert!(tree.add(100));
        assert!(tree.contains(100));
        assert!(!tree.add(100));
        assert!(tree.remove(100));
        assert!(!tree.contains(100));
    }

    #[test]
    fn test_find_first_right() {
        let mut tree = TreeUint24::default();
        for id in [50, 100, 150] {
            tree.add(id);
        }
        assert_eq!(tree.find_first_right(50), Some(50));
        assert_eq!(tree.find_first_right(60), Some(100));
        assert_eq!(tree.find_first_right(200), None);
    }

    #[test]
    fn test_find_first_left() {
        let mut tree = TreeUint24::default();
        for id in [50, 100, 150] {
            tree.add(id);
        }
        assert_eq!(tree.find_first_left(150), Some(150));
        assert_eq!(tree.find_first_left(140), Some(100));
        assert_eq!(tree.find_first_left(10), None);
    }
}
