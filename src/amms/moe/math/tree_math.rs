use alloy_primitives::U256;
use std::collections::HashMap;

use super::bit_math;

/// Bin-id bitmap tree matching Moe/Trader Joe `TreeMath.TreeUint24`.
///
/// Reference: `data/contracts/moe/libraries/math/TreeMath.sol`
/// - `find_first_right(id)` → largest stored id **strictly less than** `id`
/// - `find_first_left(id)` → smallest stored id **strictly greater than** `id`
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

    /// First stored id strictly lower than `id` (Moe `findFirstRight`).
    pub fn find_first_right(&self, id: u32) -> Option<u32> {
        let mut key2 = id >> 8;
        let mut bit = (id & 0xff) as u8;

        // Leaf: closest set bit strictly below `bit` → closestBitRight(bit - 1).
        if bit != 0 {
            if let Some(leaves) = self.level2.get(&key2) {
                if let Some(next_bit) = closest_bit_right_exclusive(*leaves, bit) {
                    return Some((key2 << 8) | next_bit);
                }
            }
        }

        let mut key1 = key2 >> 8;
        bit = (key2 & 0xff) as u8;
        if bit != 0 {
            if let Some(level1) = self.level1.get(&(key1 as u16)) {
                if let Some(next_bit) = closest_bit_right_exclusive(*level1, bit) {
                    key2 = (key1 << 8) | next_bit;
                    if let Some(leaves) = self.level2.get(&key2) {
                        if let Ok(msb) = bit_math::most_significant_bit(*leaves) {
                            return Some((key2 << 8) | msb as u32);
                        }
                    }
                }
            }
        }

        bit = (key1 & 0xff) as u8;
        if bit != 0 {
            if let Some(next_bit) = closest_bit_right_exclusive(self.level0, bit) {
                key1 = next_bit;
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

        None
    }

    /// First stored id strictly higher than `id` (Moe `findFirstLeft`).
    pub fn find_first_left(&self, id: u32) -> Option<u32> {
        let mut key2 = id >> 8;
        let mut bit = (id & 0xff) as u8;

        // Leaf: closest set bit strictly above `bit` → closestBitLeft(bit + 1).
        if bit != u8::MAX {
            if let Some(leaves) = self.level2.get(&key2) {
                if let Some(next_bit) = closest_bit_left_exclusive(*leaves, bit) {
                    return Some((key2 << 8) | next_bit);
                }
            }
        }

        let mut key1 = key2 >> 8;
        bit = (key2 & 0xff) as u8;
        if bit != u8::MAX {
            if let Some(level1) = self.level1.get(&(key1 as u16)) {
                if let Some(next_bit) = closest_bit_left_exclusive(*level1, bit) {
                    key2 = (key1 << 8) | next_bit;
                    if let Some(leaves) = self.level2.get(&key2) {
                        if let Ok(lsb) = bit_math::least_significant_bit(*leaves) {
                            return Some((key2 << 8) | lsb as u32);
                        }
                    }
                }
            }
        }

        bit = (key1 & 0xff) as u8;
        if bit != u8::MAX {
            if let Some(next_bit) = closest_bit_left_exclusive(self.level0, bit) {
                key1 = next_bit;
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

        None
    }
}

/// Moe `TreeMath._closestBitRight`: first set bit strictly lower than `bit`.
fn closest_bit_right_exclusive(leaves: U256, bit: u8) -> Option<u32> {
    if bit == 0 {
        return None;
    }
    match bit_math::closest_bit_right(leaves, bit - 1) {
        Ok(v) if v != u32::MAX => Some(v),
        _ => None,
    }
}

/// Moe `TreeMath._closestBitLeft`: first set bit strictly higher than `bit`.
fn closest_bit_left_exclusive(leaves: U256, bit: u8) -> Option<u32> {
    if bit == u8::MAX {
        return None;
    }
    match bit_math::closest_bit_left(leaves, bit + 1) {
        Ok(v) if v != u32::MAX => Some(v),
        _ => None,
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

    /// Mirrors `TreeMath.t.sol::test_FindFirst` / fuzz strict inequalities.
    #[test]
    fn test_find_first_matches_moe_reference_semantics() {
        let mut tree = TreeUint24::default();
        for id in [0u32, 1, 2] {
            tree.add(id);
        }

        // findFirstRight: strictly lower
        assert_eq!(tree.find_first_right(2), Some(1));
        assert_eq!(tree.find_first_right(1), Some(0));
        assert_eq!(tree.find_first_right(0), None);

        // findFirstLeft: strictly higher
        assert_eq!(tree.find_first_left(0), Some(1));
        assert_eq!(tree.find_first_left(1), Some(2));
        assert_eq!(tree.find_first_left(2), None);
    }

    #[test]
    fn test_find_first_right() {
        let mut tree = TreeUint24::default();
        for id in [50, 100, 150] {
            tree.add(id);
        }
        // Exclusive of the probe id (Moe reference).
        assert_eq!(tree.find_first_right(50), None);
        assert_eq!(tree.find_first_right(60), Some(50));
        assert_eq!(tree.find_first_right(100), Some(50));
        assert_eq!(tree.find_first_right(200), Some(150));
        assert_eq!(tree.find_first_right(40), None);
    }

    #[test]
    fn test_find_first_left() {
        let mut tree = TreeUint24::default();
        for id in [50, 100, 150] {
            tree.add(id);
        }
        assert_eq!(tree.find_first_left(150), None);
        assert_eq!(tree.find_first_left(140), Some(150));
        assert_eq!(tree.find_first_left(100), Some(150));
        assert_eq!(tree.find_first_left(10), Some(50));
        assert_eq!(tree.find_first_left(200), None);
    }

    #[test]
    fn test_find_first_far() {
        let mut tree = TreeUint24::default();
        tree.add(0);
        tree.add(u32::from(u16::MAX)); // stays within u24 range used by bins
        assert_eq!(tree.find_first_right(u32::from(u16::MAX)), Some(0));
        assert_eq!(tree.find_first_left(0), Some(u32::from(u16::MAX)));
    }
}
