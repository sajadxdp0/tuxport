//! Reproducible pseudo-random numbers (SplitMix64). The same seed always generates the same
//! sequence of instructions and register values.

/// Pseudo-random number generator.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Initializes with a seed.
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// The next 64-bit random number.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A random number in `[0, n)`.
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % n as u64) as u32
    }

    /// A random `n`-bit field.
    pub fn bits(&mut self, n: u32) -> u32 {
        (self.next_u64() & ((1u64 << n) - 1)) as u32
    }

    /// A random bit.
    pub fn bit(&mut self) -> u32 {
        self.bits(1)
    }

    /// Picks a random item from the list.
    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u32) as usize]
    }
}
