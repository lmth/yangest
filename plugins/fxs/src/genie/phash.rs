/// Implements `erlang:phash/2` (the old, deprecated hash used by the `dict` module).
///
/// # Implementation notes
///
/// `make_hash()` in OTP's `erl_term_hashing.c` uses `Eterm hash = 0` which is
/// a 64-bit unsigned integer on 64-bit platforms.  All arithmetic (including the
/// UINT32_HASH_STEP macro) therefore runs in **u64 wrapping** arithmetic.
///
/// The exception is `hash_binary_bytes()`: it receives and returns `Uint32`, so
/// binary hashing uses **u32 wrapping** arithmetic on a u32-truncated input.
///
/// The function signature is `Uint32 make_hash(Eterm)`, so the final result is
/// truncated to u32 before the modulo.
///
/// Reference: erts/emulator/beam/erl_term_hashing.c

use super::term::Term;

// Funny primes just above 2^28, from erl_term_hashing.c
const FN1:  u64 = 268_440_163;
const FN2:  u64 = 268_439_161;
const FN3:  u64 = 268_435_459;
const FN4:  u64 = 268_436_141;
const FN6:  u64 = 268_437_017;
const FN8:  u64 = 268_437_511;
const FN9:  u64 = 268_439_627;
const FN12: u64 = 268_440_581;

/// `erlang:phash(Key, Range)` → value in `1..=Range`.
pub fn phash(term: &Term, range: u32) -> u32 {
    let hash = make_hash(term);
    1 + (hash % range)
}

/// `make_hash()` — the hash underlying `erlang:phash/2`.
/// Returns a u32 (the Eterm hash truncated on return from the C function).
pub fn make_hash(term: &Term) -> u32 {
    let mut hash: u64 = 0;
    hash_term(term, &mut hash);
    hash as u32
}

// ── core recursive hasher (u64 accumulator) ──────────────────────────────────

fn hash_term(term: &Term, hash: &mut u64) {
    match term {
        Term::Nil => {
            // NIL_DEF: hash = hash * FN3 + 1
            *hash = hash.wrapping_mul(FN3).wrapping_add(1);
        }

        Term::Atom(bytes) => {
            // ATOM_DEF: hash = hash * FN1 + atom_hvalue(bytes)
            let hv = atom_hvalue(bytes) as u64;
            *hash = hash.wrapping_mul(FN1).wrapping_add(hv);
        }

        Term::SmallInt(n) => {
            // SMALL_DEF
            // y2 = abs(n) as u64
            let neg = *n < 0;
            let y2 = n.unsigned_abs(); // u64 magnitude
            uint32_hash_step(hash, y2 as u32, FN2);
            if y2 >> 32 != 0 {
                // 64-bit arch: second step for high word
                uint32_hash_step(hash, (y2 >> 32) as u32, FN2);
            }
            *hash = hash.wrapping_mul(if neg { FN4 } else { FN3 });
        }

        Term::BigInt(neg, bytes) => {
            // BIG_DEF: bytes = magnitude in little-endian, grouped into 64-bit digits
            hash_bigint(*neg, bytes, hash);
        }

        Term::Float(f) => {
            // FLOAT_DEF: hash = hash * FN6 + (fw[0] ^ fw[1])
            // Ensure positive zero (match OTP's erts_get_positive_zero_float).
            let f = if *f == 0.0 { 0.0f64 } else { *f };
            let bits = f.to_bits();
            let fw0 = (bits >> 32) as u32;
            let fw1 = (bits & 0xFFFF_FFFF) as u32;
            *hash = hash.wrapping_mul(FN6).wrapping_add((fw0 ^ fw1) as u64);
        }

        Term::Tuple(elems) => {
            // TUPLE_DEF: hash each element, then hash = hash * FN9 + arity
            for e in elems {
                hash_term(e, hash);
            }
            *hash = hash.wrapping_mul(FN9).wrapping_add(elems.len() as u64);
        }

        Term::List(elems) => {
            hash_list(elems, &Term::Nil, hash);
        }

        Term::ImproperList(elems, tail) => {
            hash_list(elems, tail, hash);
        }

        Term::Binary(bytes) => {
            // BITSTRING_DEF (full bytes, no trailing bits)
            // hash_binary_bytes uses u32 arithmetic on a u32-truncated input.
            let h32 = hash_binary_bytes(bytes, None, *hash as u32);
            *hash = h32 as u64;
        }

        Term::BitBinary(bytes, trailing_bits) => {
            let h32 = hash_binary_bytes(bytes, Some(*trailing_bits), *hash as u32);
            *hash = h32 as u64;
        }
    }
}

// ── list hashing — mirrors the LIST_DEF / MAKE_HASH_CDR_PRE/POST_OP logic ────

/// Hash a list given its elements and explicit tail.
fn hash_list(elems: &[Term], tail: &Term, hash: &mut u64) {
    let mut i = 0;

    // Byte optimisation loop: while CAR is a byte (0..=255 small int).
    while i < elems.len() {
        if let Some(b) = as_byte(&elems[i]) {
            // hash = hash * FN2 + byte
            *hash = hash.wrapping_mul(FN2).wrapping_add(b as u64);
            i += 1;
            // After hashing the byte, the CDR is either another cons cell or
            // a non-list (nil / improper tail).  If CDR is not a list we do
            // the CDR_POST path and return.
            if i == elems.len() {
                // CDR = tail (not a cons cell)
                hash_cdr_post(tail, hash);
                return;
            }
            // CDR = elems[i..] — still a list; continue the byte loop.
        } else {
            break; // non-byte element
        }
    }

    if i < elems.len() {
        // elems[i] is non-byte: hash it, then handle the rest as CDR_PRE.
        hash_term(&elems[i], hash);
        hash_cdr_pre(&elems[i + 1..], tail, hash);
    } else {
        // All elements consumed before finding a non-byte (only reachable
        // when elems is empty on initial call — tail is the whole "list").
        hash_cdr_post(tail, hash);
    }
}

/// MAKE_HASH_CDR_PRE_OP: the CDR is either a list (→ hash_list) or not (→ CDR_POST).
fn hash_cdr_pre(elems: &[Term], tail: &Term, hash: &mut u64) {
    if elems.is_empty() {
        // CDR is the tail — not a cons cell.
        hash_cdr_post(tail, hash);
    } else {
        // CDR is a non-empty list — fall through to list processing.
        hash_list(elems, tail, hash);
    }
}

/// MAKE_HASH_CDR_POST_OP: process `term` then multiply hash by FN8.
fn hash_cdr_post(term: &Term, hash: &mut u64) {
    hash_term(term, hash);          // NIL → hash*FN3+1; other terms as normal
    *hash = hash.wrapping_mul(FN8);
}

/// Returns `Some(byte)` if the term is a small integer in 0..=255.
fn as_byte(term: &Term) -> Option<u8> {
    if let Term::SmallInt(n) = term {
        if *n >= 0 && *n <= 255 {
            return Some(*n as u8);
        }
    }
    None
}

// ── atom hash (hashpjw from atom.c) ──────────────────────────────────────────

/// Computes the hash stored in OTP's atom table for an atom with the given
/// UTF-8 byte sequence.  This is the hashpjw algorithm with the "latin-1 clutch"
/// that converts 2-byte UTF-8 sequences for code points U+0080..U+00FF back to
/// their 1-byte Latin-1 values before hashing — ensuring backward compatibility
/// with pre-Unicode Erlang atom hashes.
pub fn atom_hvalue(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i] as u32;
        // Latin-1 clutch: 2-byte UTF-8 for U+0080..U+00FF (C2/C3 lead byte).
        let v: u32 = if i + 1 < bytes.len()
            && (b0 & 0xFE) == 0xC2
            && (bytes[i + 1] & 0xC0) == 0x80
        {
            // Combine into a single byte (truncated to u8, as in the C code).
            let combined = (b0 << 6) | (bytes[i + 1] & 0x3F) as u32;
            i += 2;
            combined & 0xFF // truncate to byte, matching C's `byte v` assignment
        } else {
            i += 1;
            b0
        };

        h = (h << 4).wrapping_add(v);
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
            h ^= g;
        }
    }
    h
}

// ── binary hashing — uses u32 arithmetic (hash_binary_bytes in OTP) ──────────

/// `hash_binary_bytes()` from OTP.  Takes and returns a `u32`; all internal
/// arithmetic is **u32 wrapping** (not u64).
///
/// `trailing_bits`: `None` for a full binary; `Some(n)` for a bit string where
/// only the high `n` bits of the last byte are significant.
fn hash_binary_bytes(bytes: &[u8], trailing_bits: Option<u8>, init: u32) -> u32 {
    let mut hash = init;

    let (full_bytes, last_byte) = match trailing_bits {
        None | Some(0) => (bytes, None),
        Some(bits) => {
            if bytes.is_empty() {
                (bytes, None)
            } else {
                (&bytes[..bytes.len() - 1], Some((bytes[bytes.len() - 1], bits)))
            }
        }
    };

    // Full bytes
    for &b in full_bytes {
        hash = (hash as u64).wrapping_mul(FN1).wrapping_add(b as u64) as u32;
    }

    // Trailing partial byte (bit string)
    if let Some((last, bits)) = last_byte {
        let b = last >> (8 - bits);
        hash = ((hash as u64).wrapping_mul(FN1).wrapping_add(b as u64) as u32)
            .wrapping_mul(FN12 as u32)
            .wrapping_add(bits as u32);
    }

    // Final: hash * FN4 + bytesize
    let bytesize = full_bytes.len() as u32;
    (hash as u64).wrapping_mul(FN4).wrapping_add(bytesize as u64) as u32
}

// ── integer hash helpers ──────────────────────────────────────────────────────

/// UINT32_HASH_STEP macro: hashes the 4 bytes of `x` (LSB first) into `hash`.
/// Uses u64 arithmetic (hash is Eterm = u64 on 64-bit OTP).
fn uint32_hash_step(hash: &mut u64, x: u32, prime: u64) {
    let h = hash.wrapping_mul(prime).wrapping_add((x & 0xFF) as u64);
    let h = h.wrapping_mul(prime).wrapping_add(((x >> 8) & 0xFF) as u64);
    let h = h.wrapping_mul(prime).wrapping_add(((x >> 16) & 0xFF) as u64);
    *hash = h.wrapping_mul(prime).wrapping_add((x >> 24) as u64);
}

/// BIG_DEF case: hash a bignum stored as little-endian magnitude bytes.
///
/// OTP stores bignums as an array of 64-bit "digits" (little-endian).  All
/// digits except the last are hashed byte-by-byte (8 bytes each).  For the last
/// digit, only 4 bytes are hashed if the high 32 bits are zero.
fn hash_bigint(negative: bool, bytes: &[u8], hash: &mut u64) {
    // Strip trailing zero bytes (most-significant zeros in the magnitude).
    let end = bytes.iter().rposition(|&b| b != 0).map(|i| i + 1).unwrap_or(0);
    let bytes = &bytes[..end];

    if bytes.is_empty() {
        // Magnitude zero → treated as small 0 elsewhere; shouldn't land here.
        *hash = hash.wrapping_mul(FN3);
        return;
    }

    // Group bytes into 64-bit digits (OTP's ErtsDigit = u64 on 64-bit).
    let n_digits = bytes.len().div_ceil(8);
    let k = n_digits - 1; // index of last digit

    // All digits except the last: hash all 8 bytes.
    for i in 0..k {
        let d = read_digit64(bytes, i);
        for j in 0..8u32 {
            let b = ((d >> (j * 8)) & 0xFF) as u32;
            *hash = hash.wrapping_mul(FN2).wrapping_add(b as u64);
        }
    }

    // Last digit: hash 4 bytes if high 32 bits are zero, else 8 bytes.
    let d = read_digit64(bytes, k);
    let n_bytes = if (d >> 32) == 0 { 4usize } else { 8usize };
    for j in 0..n_bytes as u32 {
        let b = ((d >> (j * 8)) & 0xFF) as u32;
        *hash = hash.wrapping_mul(FN2).wrapping_add(b as u64);
    }

    *hash = hash.wrapping_mul(if negative { FN4 } else { FN3 });
}

/// Read one 64-bit little-endian digit from a byte slice (zero-padded).
fn read_digit64(bytes: &[u8], digit_idx: usize) -> u64 {
    let start = digit_idx * 8;
    let mut d: u64 = 0;
    for j in 0..8usize {
        d |= (bytes.get(start + j).copied().unwrap_or(0) as u64) << (j * 8);
    }
    d
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genie::term::Term;

    fn atom(s: &str) -> Term { Term::Atom(s.as_bytes().to_vec()) }
    fn int(n: i64) -> Term  { Term::SmallInt(n) }

    // Each tuple: (term, range, expected_phash)
    // Generated by:  ~/otp/bin/escript /tmp/gen_phash_vectors.escript
    fn vectors() -> Vec<(Term, u32, u32)> {
        vec![
            // atoms
            (atom("foo"),         16, 16),
            (atom("bar"),         16,  3),
            (atom(""),            16,  1),
            (atom("hello_world"), 16,  5),
            (atom("CamelCase"),   16,  6),
            (atom("foo"),         32, 32),
            (atom("bar"),         32,  3),
            (atom("a"),           16,  2),
            (atom("b"),           16,  3),

            // integers
            (int(0),         16,  1),
            (int(1),         16, 12),
            (int(-1),        16,  6),
            (int(255),       16,  6),
            (int(256),       16,  4),
            (int(-256),      16, 14),
            (int(65535),     16,  3),
            (int(-65535),    16, 15),
            (int(1_000_000), 16, 12),
            (int(-1_000_000),16,  6),
            (int(134_217_727),  16, 13),
            (int(-134_217_728), 16,  9),
            (int(2_147_483_647),  16, 5),
            (int(-2_147_483_648), 16, 1),
            (int(1),  32, 28),
            (int(2),  32, 23),
            (int(15), 32, 22),
            (int(16), 32, 17),
            (int(17), 32, 12),

            // floats
            (Term::Float(0.0),   16, 1),
            (Term::Float(1.0),   16, 1),
            (Term::Float(-1.0),  16, 1),
            (Term::Float(3.14159265358979), 16, 11),

            // nil
            (Term::Nil, 16, 2),
            (Term::Nil, 16, 2),

            // proper lists (byte elements)
            (Term::List(vec![int(1), int(2), int(3)]),  16, 6),
            (Term::List(vec![int(1), int(2), int(3)]),  16, 6),

            // proper lists (non-byte atoms)
            (Term::List(vec![atom("a"), atom("b"), atom("c")]), 16, 2),

            // mixed: byte then atom then byte
            (Term::List(vec![int(1), atom("a"), int(2)]), 16, 6),

            // "hello" as list of bytes [104,101,108,108,111]
            (Term::List(vec![int(104),int(101),int(108),int(108),int(111)]), 16, 4),

            // improper lists
            (Term::ImproperList(vec![int(1)], Box::new(int(2))),    16, 16),
            (Term::ImproperList(vec![atom("a")], Box::new(atom("b"))), 16,  4),

            // binaries
            (Term::Binary(vec![]),            16,  1),
            (Term::Binary(vec![1, 2, 3]),     16, 14),
            (Term::Binary(b"hello".to_vec()), 16, 12),
            (Term::Binary(vec![1,2,3]),       16, 14),

            // tuples
            (Term::Tuple(vec![]),                             16,  1),
            (Term::Tuple(vec![atom("a")]),                    16, 13),
            (Term::Tuple(vec![atom("a"), atom("b")]),         16, 10),
            (Term::Tuple(vec![int(1), int(2), int(3)]),       16,  6),
            (Term::Tuple(vec![atom("a"),int(1),atom("b"),int(2)]), 16, 3),

            // nested
            (Term::Tuple(vec![atom("a"), Term::List(vec![int(1),int(2),int(3)])]), 16, 9),
            (Term::List(vec![
                Term::List(vec![atom("a"), atom("b")]),
                Term::List(vec![atom("c"), atom("d")]),
            ]), 16, 16),
        ]
    }

    #[test]
    fn test_atom_hvalue() {
        // atom "" → hvalue 0 → phash(16) = 1
        assert_eq!(atom_hvalue(b""), 0);
        // atom "a" (0x61): h = 97
        assert_eq!(atom_hvalue(b"a"), 97);
        // atom "b" (0x62): h = 98
        assert_eq!(atom_hvalue(b"b"), 98);
        // atom "foo" = [102, 111, 111]
        // h=0: h=(0<<4)+102=102; h=(102<<4)+111=1743; h=(1743<<4)+111=27999
        assert_eq!(atom_hvalue(b"foo"), 27999);
    }

    #[test]
    fn test_phash_vectors() {
        for (term, range, expected) in vectors() {
            let got = phash(&term, range);
            assert_eq!(
                got, expected,
                "phash({:?}, {}) = {} expected {}",
                term, range, got, expected
            );
        }
    }
}
