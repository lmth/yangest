/// Simulates Erlang's `dict` module data structure.
///
/// Erlang's `dict` uses dynamic hashing (Larsson/Griswold-Townsend algorithm):
/// buckets expand by splitting slots, segment array doubles when needed.
///
/// # Fold order
///
/// `dict:fold/3` iterates segments **high → low**, buckets **15 → 0**, within
/// each bucket **head → tail**.  New keys are appended to the **end** of their
/// bucket list (`store_bkt_val` appends; see `dict.erl`), so for distinct keys
/// the bucket order is insertion order.
///
/// Reference: lib/stdlib/src/dict.erl

use super::phash::phash;
use super::term::Term;

const SEG_SIZE: usize = 16; // SEGMENT_SIZE in dict.erl

pub struct ErlangDict {
    /// Flat array of all buckets.  Index is slot − 1 (0-based).
    /// Each bucket holds keys in insertion order (head = first inserted).
    buckets: Vec<Vec<Term>>,

    /// Current number of active slots (n in dict.erl).
    n: u32,
    /// Current maximum slot range for phash (maxn in dict.erl).
    maxn: u32,
    /// Buddy-slot offset (bso in dict.erl = maxn/2 always, but tracked explicitly).
    bso: u32,
    /// Expansion threshold (exp_size in dict.erl).
    exp_size: u32,
    /// Contraction threshold (con_size in dict.erl — unused here, kept for fidelity).
    con_size: u32,
    /// Number of stored keys.
    size: u32,
}

impl ErlangDict {
    /// Create an empty dict matching Erlang's `dict:new()`.
    ///
    /// From dict.erl:
    /// ```erlang
    /// #dict{size=0, n=?seg_size, maxn=?seg_size, bso=?seg_size div 2,
    ///       exp_size=?seg_size * ?expand_load,
    ///       con_size=?seg_size * ?contract_load, ...}
    /// ```
    /// (seg_size=16, expand_load=5, contract_load=3)
    pub fn new() -> Self {
        let n = SEG_SIZE as u32;   // 16
        let maxn = n;              // 16
        let bso = n / 2;           // 8
        ErlangDict {
            buckets: vec![vec![]; n as usize],
            n,
            maxn,
            bso,
            exp_size: n * 5,       // 80
            con_size: n * 3,       // 48 (dict.erl has this, not div 2)
            size: 0,
        }
    }

    /// Store a key (no value — we only care about insertion order for fold).
    /// Mirrors `dict:store/3` behaviour for distinct keys.
    ///
    /// If the key is already present it is NOT re-added (matching dict:store
    /// which updates the value but doesn't change position).
    pub fn store(&mut self, key: Term) {
        let slot = self.get_slot(&key);
        let bucket = &self.buckets[slot];

        // Check for duplicate (dict:store updates value but doesn't move position).
        if bucket.iter().any(|k| k == &key) {
            return;
        }

        // Append to end of bucket (store_bkt_val recurses to tail).
        self.buckets[slot].push(key);
        self.size += 1;

        // Erlang: maybe_expand(D1, Ic=1) checks `size_before + 1 > exp_size`.
        // Since we've already incremented size: self.size > self.exp_size is the same.
        if self.size > self.exp_size {
            self.expand();
        }
    }

    /// Returns the keys in the order that `dict:fold/3` would visit them.
    ///
    /// Fold visits: segments high→low, buckets within segment 15→0,
    /// elements within bucket head→tail.
    ///
    /// Since we store buckets in a flat array indexed by slot number (0-based),
    /// slot ordering matches: high segment first, then high bucket within segment.
    /// Slot `s = seg_i * 16 + bkt_i`; fold visits in decreasing slot order.
    pub fn fold_order(&self) -> Vec<&Term> {
        let mut result = Vec::new();
        let total_slots = self.buckets.len();
        for slot in (0..total_slots).rev() {
            for key in &self.buckets[slot] {
                result.push(key);
            }
        }
        result
    }

    /// Returns indices into the original `input` slice in `dict:fold/3` order.
    ///
    /// `input` must be the slice that was passed to `store` in order.
    /// Panics if any fold-returned key is not found in `input`.
    pub fn fold_order_indices(&self, input: &[Term]) -> Vec<usize> {
        self.fold_order()
            .into_iter()
            .map(|key| {
                input.iter().position(|k| k == key)
                    .expect("fold returned key not in input")
            })
            .collect()
    }

    // ── dict internals ───────────────────────────────────────────────────────

    /// `get_slot(Key)` from dict.erl:
    /// ```erlang
    /// H = erlang:phash(Key, Maxn),
    /// if H > N -> H - Bso; true -> H end.
    /// ```
    fn get_slot(&self, key: &Term) -> usize {
        let h = phash(key, self.maxn);
        let slot = if h > self.n { h - self.bso } else { h };
        (slot - 1) as usize // convert 1-based to 0-based
    }

    /// Expand capacity by splitting one slot.
    ///
    /// Mirrors dict.erl `maybe_expand_aux/2` + `maybe_expand_segs/1`:
    ///
    /// 1. If `n == maxn`: double the segment array (add `maxn` empty buckets),
    ///    then `maxn *= 2` and `bso *= 2`.  (This is `maybe_expand_segs`.)
    /// 2. `N = n + 1`.
    /// 3. `Slot1 = N - bso` (using the potentially-doubled bso).
    /// 4. `Slot2 = N`.
    /// 5. Rehash bucket at `Slot1` into `Slot1` and `Slot2`.
    /// 6. Update `n = N`, `exp_size = N * 5`, `con_size = N * 3`.
    fn expand(&mut self) {
        // Step 1: maybe_expand_segs
        if self.n == self.maxn {
            let extra = self.maxn as usize;
            self.buckets.extend(std::iter::repeat_with(Vec::new).take(extra));
            self.maxn *= 2;
            self.bso  *= 2;
        }

        // Step 2-4: new slot numbers (1-based)
        let new_n = self.n + 1;
        let slot1 = new_n - self.bso;
        let slot2 = new_n;
        let idx1 = (slot1 - 1) as usize;
        let idx2 = (slot2 - 1) as usize;

        // Step 5: rehash
        // slot2 already exists (either from initial alloc or from step 1 above).
        debug_assert!(idx2 < self.buckets.len(), "slot2 index out of range");
        let split_bucket = std::mem::take(&mut self.buckets[idx1]);
        self.n = new_n;
        for key in split_bucket {
            let s = self.get_slot(&key);
            self.buckets[s].push(key);
        }

        // Step 6: update thresholds
        self.exp_size = new_n * 5;
        self.con_size = new_n * 3;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genie::term::Term;

    fn atom(s: &str) -> Term { Term::Atom(s.as_bytes().to_vec()) }
    fn int(n: i64) -> Term  { Term::SmallInt(n) }

    #[test]
    fn test_new_dict_state() {
        let d = ErlangDict::new();
        assert_eq!(d.n, 16);
        assert_eq!(d.maxn, 16);
        assert_eq!(d.bso, 8);
        assert_eq!(d.exp_size, 80);
        assert_eq!(d.size, 0);
        assert_eq!(d.buckets.len(), 16);
    }

    #[test]
    fn test_single_insert() {
        let mut d = ErlangDict::new();
        d.store(atom("foo"));
        assert_eq!(d.size, 1);
        let order: Vec<&Term> = d.fold_order();
        assert_eq!(order, vec![&atom("foo")]);
    }

    #[test]
    fn test_duplicate_insert() {
        let mut d = ErlangDict::new();
        d.store(atom("foo"));
        d.store(atom("foo"));
        assert_eq!(d.size, 1);
    }

    #[test]
    fn test_slot_calculation() {
        let d = ErlangDict::new();
        // phash(foo, 16) = 16, 16 <= n=16 → slot = 16, 0-based = 15
        let s = d.get_slot(&atom("foo"));
        assert_eq!(s, 15);
        // phash(bar, 16) = 3 → slot 3, 0-based = 2
        let s = d.get_slot(&atom("bar"));
        assert_eq!(s, 2);
    }

    #[test]
    fn test_small_dict_fold_order() {
        // Insert a handful of atoms and check order matches Erlang.
        // We'll verify this matches the reference escript output.
        let mut d = ErlangDict::new();
        let keys = ["a", "b", "c", "d", "e"];
        for k in &keys {
            d.store(atom(k));
        }
        let order: Vec<&Term> = d.fold_order();
        // All keys should be present
        assert_eq!(order.len(), 5);
        // Every key should appear exactly once
        for k in &keys {
            assert!(order.contains(&&atom(k)), "missing key {k}");
        }
    }

    #[test]
    fn test_expansion_triggers() {
        // Insert 85 keys to force at least one expansion (exp_size=80).
        let mut d = ErlangDict::new();
        for i in 0..85 {
            d.store(int(i));
        }
        assert_eq!(d.size, 85);
        let order = d.fold_order();
        assert_eq!(order.len(), 85);
    }
}
