/// Erlang term types sufficient to cover common dict keys.
/// Decoded from the External Term Format (EETF).
#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// `[]`
    Nil,
    /// Atom — raw UTF-8 bytes (same as stored in OTP's atom table).
    Atom(Vec<u8>),
    /// Small integer: fits in OTP's SMALL range (-2^59 .. 2^59-1 on 64-bit).
    SmallInt(i64),
    /// Big integer: (negative, magnitude_bytes_little_endian).
    BigInt(bool, Vec<u8>),
    /// IEEE 754 double.
    Float(f64),
    /// Tuple.
    Tuple(Vec<Term>),
    /// Proper list (tail is implicitly Nil).
    List(Vec<Term>),
    /// Improper list `[h1, h2, ... | tail]` where tail is not Nil.
    ImproperList(Vec<Term>, Box<Term>),
    /// Binary (byte string).
    Binary(Vec<u8>),
    /// Bit string: `bytes` holds the data, `trailing_bits` is the number of
    /// significant bits in the last byte (1-7; 0 means all 8 bits used).
    BitBinary(Vec<u8>, u8),
}

impl Term {
    /// Is this term a cons cell / list? (NIL is not, in OTP's C sense.)
    pub fn is_cons(&self) -> bool {
        matches!(self, Term::List(_) | Term::ImproperList(_, _))
    }
}
