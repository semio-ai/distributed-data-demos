/// Monotonic sequence number generator.
///
/// Produces values starting from 1, incrementing by 1 on each call to `next_seq()`.
pub struct SeqGenerator {
    counter: u64,
}

impl SeqGenerator {
    /// Create a new sequence generator starting from 0 (first `next_seq()` returns 1).
    pub fn new() -> Self {
        Self { counter: 0 }
    }

    /// Return the next sequence number.
    pub fn next_seq(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }
}

impl Default for SeqGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_starts_at_one() {
        let mut gen = SeqGenerator::new();
        assert_eq!(gen.next_seq(), 1);
    }

    #[test]
    fn test_monotonic_increment() {
        let mut gen = SeqGenerator::new();
        let values: Vec<u64> = (0..5).map(|_| gen.next_seq()).collect();
        assert_eq!(values, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_default() {
        let mut gen = SeqGenerator::default();
        assert_eq!(gen.next_seq(), 1);
        assert_eq!(gen.next_seq(), 2);
    }
}
