//! A byte-bounded cache with CLOCK eviction (second chance), the policy
//! `hal::blockcache` uses: every hit sets a slot's reference bit; to make
//! room the hand sweeps the slots, clearing set bits and evicting the first
//! slot found clear. No knowledge of glyphs — `render` keys it by glyph and
//! stores coverage masks — so the policy is testable on its own.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

struct Slot<K, V> {
    key: K,
    val: V,
    size: usize,
    referenced: bool,
}

/// Counters for a log line or a test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    /// Sum of the sizes of what is cached now.
    pub bytes: usize,
    pub entries: usize,
}

pub struct ClockCache<K, V> {
    index: BTreeMap<K, usize>,
    slots: Vec<Option<Slot<K, V>>>,
    free: Vec<usize>,
    hand: usize,
    cap: usize,
    stats: Stats,
}

impl<K: Ord + Copy, V> ClockCache<K, V> {
    /// A cache holding at most `cap` bytes, by the sizes given to `insert`.
    pub fn new(cap: usize) -> Self {
        ClockCache { index: BTreeMap::new(), slots: Vec::new(), free: Vec::new(), hand: 0, cap, stats: Stats::default() }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The slot holding `key`, marking it referenced and counting a hit or
    /// a miss. A slot number rather than a reference so that a miss can be
    /// followed by `insert` without fighting the borrow checker.
    pub fn lookup(&mut self, key: &K) -> Option<usize> {
        match self.index.get(key) {
            Some(&i) => {
                self.stats.hits += 1;
                if let Some(s) = self.slots[i].as_mut() {
                    s.referenced = true;
                }
                Some(i)
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    /// The value in slot `i`, as returned by `lookup` or `insert`.
    pub fn at(&self, i: usize) -> &V {
        &self.slots[i].as_ref().expect("empty cache slot").val
    }

    /// Caches `val` under `key` (which must not be cached already),
    /// evicting until it fits. A value larger than the whole cache is not
    /// kept: it is handed back, for the caller to use once.
    pub fn insert(&mut self, key: K, val: V, size: usize) -> Result<usize, V> {
        if size > self.cap {
            return Err(val);
        }
        while self.stats.bytes + size > self.cap {
            self.evict_one();
        }
        let slot = Slot { key, val, size, referenced: false };
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i] = Some(slot);
                i
            }
            None => {
                self.slots.push(Some(slot));
                self.slots.len() - 1
            }
        };
        self.index.insert(key, i);
        self.stats.bytes += size;
        self.stats.entries += 1;
        Ok(i)
    }

    /// Only called while something is cached (bytes > 0 with size <= cap
    /// means at least one slot is full), so the sweep ends within two turns.
    fn evict_one(&mut self) {
        loop {
            if self.hand >= self.slots.len() {
                self.hand = 0;
            }
            let i = self.hand;
            self.hand += 1;
            match self.slots[i].as_mut() {
                None => continue,
                Some(s) if s.referenced => s.referenced = false,
                Some(_) => {
                    let s = self.slots[i].take().unwrap();
                    self.index.remove(&s.key);
                    self.free.push(i);
                    self.stats.bytes -= s.size;
                    self.stats.entries -= 1;
                    self.stats.evictions += 1;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ClockCache;

    #[test]
    fn a_hit_returns_what_was_inserted() {
        let mut c = ClockCache::new(100);
        assert_eq!(c.lookup(&1), None);
        let i = c.insert(1, "one", 10).unwrap();
        assert_eq!(*c.at(i), "one");
        let j = c.lookup(&1).unwrap();
        assert_eq!(*c.at(j), "one");
        let s = c.stats();
        assert_eq!((s.hits, s.misses, s.bytes, s.entries), (1, 1, 10, 1));
    }

    #[test]
    fn never_holds_more_than_its_cap() {
        let mut c = ClockCache::new(100);
        for k in 0..1000u32 {
            if c.lookup(&k).is_none() {
                c.insert(k, k, 7 + (k as usize % 13)).unwrap();
            }
            assert!(c.stats().bytes <= 100);
        }
        assert!(c.stats().evictions > 0);
    }

    #[test]
    fn a_referenced_entry_gets_a_second_chance() {
        let mut c = ClockCache::new(30);
        c.insert(1, (), 10).unwrap();
        c.insert(2, (), 10).unwrap();
        c.insert(3, (), 10).unwrap();
        c.lookup(&1);
        // Full: making room must skip 1 (referenced) and take 2.
        c.insert(4, (), 10).unwrap();
        assert!(c.lookup(&1).is_some());
        assert!(c.lookup(&2).is_none());
        assert!(c.lookup(&3).is_some());
        assert!(c.lookup(&4).is_some());
    }

    #[test]
    fn a_value_bigger_than_the_cache_is_handed_back() {
        let mut c = ClockCache::new(10);
        c.insert(1, 1, 5).unwrap();
        assert_eq!(c.insert(2, 2, 11), Err(2));
        // And nothing was evicted for it.
        assert!(c.lookup(&1).is_some());
    }

    #[test]
    fn freed_slots_are_reused() {
        let mut c = ClockCache::new(10);
        for k in 0..100u32 {
            c.insert(k, k, 5).unwrap();
        }
        assert_eq!(c.stats().entries, 2);
        assert!(c.slots.len() <= 3);
    }
}
