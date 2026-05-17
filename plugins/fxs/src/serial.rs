//! FXS binary file format serializer.
//!
//! Implements the exact binary layout expected by ConfD/NSO's FXS loader:
//!
//! ```text
//! [4 bytes]  FXS_NEW_MAGIC = 0x04 0x07 0x06 0x08
//! [Sz:32][ETF uncompressed]  {"8.8_1", #fxs_header{}}  -- header (patched later)
//! -- data section (each chunk = [Sz:32][ETF zlib-compressed]) --
//!   fxs_write_list(ExsTypes)    -- empty = no bytes
//!   fxs_write_list(LoadTypes)   -- usually empty
//!   fxs_write_list(AugL)        -- augmentations
//!   fxs_write_list(CsCdbL)      -- CDB-specific cs subset
//!   fxs_write_list(Identities)  -- identity tuples   <-- CDB checksum up to here
//!   fxs_write_list(CsL)         -- main cs records (reversed, chunks of 256)
//!   fxs_write_list(Misc2)       -- callpoints, docs, etc.
//!   fxs_write_dict(HashDict)    -- #hash{} records
//!   fxs_write_list([#callpoint_info{}])  <-- full checksum up to here
//! [0:32]  end marker
//! [optional YANG section]
//! [optional descr section]
//! [0:32]  end section marker
//! ```
//!
//! The MD5 checksums are computed over the **compressed** ETF bytes (without
//! the 4-byte size prefix).  The CDB checksum covers sections up through
//! Identities; the full checksum covers everything through callpoint_info.

use std::io::{Cursor, Write};

use eetf::Term;
use flate2::{Compression, write::ZlibEncoder};
use md5::{Digest, Md5};

const FXS_NEW_MAGIC: [u8; 4] = [0x04, 0x07, 0x06, 0x08];

/// In-memory FXS file builder.
///
/// Call methods in order, then `finish()` to get the final byte vector with
/// the header patched with correct checksums.
pub struct FxsWriter {
    buf: Cursor<Vec<u8>>,
    header_pos: usize,
    md5_cdb: Md5,
    md5_full: Md5,
    cdb_done: bool,
}

impl FxsWriter {
    pub fn new() -> Self {
        FxsWriter {
            buf: Cursor::new(Vec::new()),
            header_pos: 0,
            md5_cdb: Md5::new(),
            md5_full: Md5::new(),
            cdb_done: false,
        }
    }

    /// Write the 4-byte magic header.
    pub fn write_magic(&mut self) {
        self.buf.write_all(&FXS_NEW_MAGIC).unwrap();
    }

    /// Write a placeholder header term (uncompressed).  Returns the byte
    /// offset just before this term so it can be patched later.
    pub fn write_header(&mut self, term: &Term) -> usize {
        let pos = self.buf.position() as usize;
        self.header_pos = pos;
        write_term_uncompressed(&mut self.buf, term);
        pos
    }

    /// Write a list in the FXS format: groups of ≤256 items, each group
    /// encoded as a reversed Erlang list and written as a compressed term.
    ///
    /// Empty input → nothing written (no bytes, no MD5 update).
    pub fn write_list(&mut self, items: &[Term]) {
        for chunk in mk_list_chunks(items) {
            let compressed = compress_term(&Term::from(chunk));
            self.update_md5(&compressed);
            write_compressed_bytes(&mut self.buf, &compressed);
        }
    }

    /// Write a hash dict in FXS format, mirroring `fxs_write_dict` in
    /// `confd_rt_tools.erl`.
    ///
    /// `fxs_write_dict` uses `dict:fold` + accumulate-and-flush: full chunks
    /// (256 items) are written **as they are filled** (in fold order), and the
    /// remaining partial chunk is written **last**.  This is the OPPOSITE of
    /// `fxs_write_list`/`fxs_mk_list_chunks` which puts the partial chunk first.
    ///
    /// Within each chunk items are reversed (the fold accumulator uses prepend),
    /// same as `fxs_mk_list_chunks`.
    pub fn write_dict(&mut self, items: &[Term]) {
        for chunk in mk_dict_chunks(items) {
            let compressed = compress_term(&Term::from(chunk));
            self.update_md5(&compressed);
            write_compressed_bytes(&mut self.buf, &compressed);
        }
    }

    /// Mark the point after which the CDB-only checksum is final.
    /// Call this after write_list(Identities).
    pub fn mark_cdb_done(&mut self) {
        debug_assert!(!self.cdb_done, "mark_cdb_done called twice");
        self.cdb_done = true;
    }

    /// Write the 4-byte end-of-data-section marker (0x00000000).
    pub fn write_end_marker(&mut self) {
        self.buf.write_all(&[0u8; 4]).unwrap();
    }

    /// Returns the current byte position in the buffer.
    pub fn current_pos(&self) -> u32 {
        self.buf.position() as u32
    }

    /// Write a YANG source section entry (one YANG file) in the FXS format:
    ///
    /// ```text
    /// [Sz:32][ETF: {yang_module, NameAtom, RevisionBin|undefined}]  (uncompressed)
    /// [ChunkSz:32][zlib-deflated YANG source]
    /// [0:32]  end-of-file marker
    /// ```
    ///
    /// NOT included in the MD5 checksum region.
    pub fn write_yang_file(&mut self, name: &str, revision: Option<&str>, source: &[u8]) {
        // Build yang_module record: {yang_module, NameAtom, Revision}
        // Revision: None → <<>>, Some(r) → <<r>>   (mirrors yanger behaviour)
        let rev_term: Term = match revision {
            Some(r) => Term::from(eetf::Binary { bytes: r.as_bytes().to_vec() }),
            None => Term::from(eetf::Binary { bytes: vec![] }),
        };
        let yang_mod = Term::from(eetf::Tuple {
            elements: vec![
                Term::from(eetf::Atom { name: "yang_module".to_string() }),
                Term::from(eetf::Atom { name: name.to_string() }),
                rev_term,
            ],
        });
        write_term_uncompressed(&mut self.buf, &yang_mod);

        // Compress the entire YANG source with zlib and write as one chunk
        if !source.is_empty() {
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(source).unwrap();
            let deflated = enc.finish().unwrap();
            let sz = deflated.len() as u32;
            self.buf.write_all(&sz.to_be_bytes()).unwrap();
            self.buf.write_all(&deflated).unwrap();
        }

        // End marker for this file
        self.buf.write_all(&[0u8; 4]).unwrap();
    }

    /// Patch the header at the recorded position with the final checksums
    /// and section positions, then return the finished byte vector.
    pub fn finish(mut self, build_header: impl FnOnce([u8; 16], [u8; 16]) -> Term) -> Vec<u8> {
        let cdb_checksum: [u8; 16] = self.md5_cdb.finalize().into();
        let full_checksum: [u8; 16] = self.md5_full.finalize().into();
        let header_term = build_header(cdb_checksum, full_checksum);

        // Patch: overwrite the old header term in-place with the new one.
        // We can do this because the compressed terms all come after the header
        // and we sized the placeholder correctly.
        let header_bytes = term_to_uncompressed_bytes(&header_term);
        let pos = self.header_pos;
        let buf = self.buf.get_mut();
        let needed = 4 + header_bytes.len();
        let available = buf.len() - pos;
        debug_assert!(
            available >= needed,
            "header grew unexpectedly: needed {needed} but only {available} bytes available"
        );
        let dest = &mut buf[pos..pos + needed];
        dest[..4].copy_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        dest[4..].copy_from_slice(&header_bytes);

        self.buf.into_inner()
    }

    fn update_md5(&mut self, compressed_bytes: &[u8]) {
        if !self.cdb_done {
            self.md5_cdb.update(compressed_bytes);
        }
        self.md5_full.update(compressed_bytes);
    }
}

// ---------------------------------------------------------------------------
// Low-level write helpers
// ---------------------------------------------------------------------------

/// Encode `term` with Erlang's `term_to_binary/1` (uncompressed ETF) and
/// write `[Sz:32][bytes]` to `w`.
pub fn write_term_uncompressed<W: Write>(w: &mut W, term: &Term) {
    let bytes = term_to_uncompressed_bytes(term);
    let sz = bytes.len() as u32;
    w.write_all(&sz.to_be_bytes()).unwrap();
    w.write_all(&bytes).unwrap();
}

/// Produce uncompressed `term_to_binary` bytes (with ETF tag byte 0x83).
pub fn term_to_uncompressed_bytes(term: &Term) -> Vec<u8> {
    let mut buf = Vec::new();
    term.encode(&mut buf).expect("eetf encode");
    buf
}

/// Produce compressed ETF bytes matching Erlang's `term_to_binary(T, [{compressed, N}])`:
/// - If zlib compression reduces size: `[131, 80, u32be(uncompressed_len), zlib_deflate(rest)]`
/// - Otherwise (compression doesn't help): plain uncompressed ETF `[131, ...]`
///
/// Erlang uses COMPRESSED_TERM only when the compressed form is strictly smaller.
pub fn compress_term(term: &Term) -> Vec<u8> {
    let uncompressed = term_to_uncompressed_bytes(term);
    let body = &uncompressed[1..]; // strip the 0x83 tag
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(body).unwrap();
    let deflated = enc.finish().unwrap();

    // Use compressed form when it doesn't increase total size, matching
    // Erlang's behaviour: term_to_binary(T, [compressed]) uses compressed form
    // when compressed_total <= uncompressed body length (i.e., equal size is also OK).
    let compressed_total = 1 + 4 + deflated.len(); // tag(1) + sz(4) + data
    if compressed_total <= body.len() {
        let uncomp_sz = body.len() as u32;
        let mut out = Vec::with_capacity(1 + 1 + 4 + deflated.len());
        out.push(131u8); // ETF tag
        out.push(80u8);  // COMPRESSED_TERM tag
        out.extend_from_slice(&uncomp_sz.to_be_bytes());
        out.extend_from_slice(&deflated);
        out
    } else {
        uncompressed
    }
}

/// Write compressed bytes with a 4-byte size prefix.
fn write_compressed_bytes<W: Write>(w: &mut W, compressed: &[u8]) {
    let sz = compressed.len() as u32;
    w.write_all(&sz.to_be_bytes()).unwrap();
    w.write_all(compressed).unwrap();
}

// ---------------------------------------------------------------------------
// List chunking (mirrors fxs_mk_list_chunks in confd_rt_tools.erl)
// ---------------------------------------------------------------------------

/// Split `items` into chunks of ≤256 elements and return them in the same
/// order that Erlang's `fxs_mk_list_chunks` produces.
///
/// `fxs_mk_list_chunks` works by prepending items to an `Acc` buffer and
/// flushing full chunks (256 items) to `CAcc` via prepend.  This means the
/// **last (partial) chunk ends up at the front of CAcc** and is written first.
/// Within each chunk the items are in reversed order (because of the per-item
/// prepend to Acc).
///
/// Concretely, for `items = [A, B, ..., Z]` with more than 256 items:
///   chunk_0  = items[0..256] reversed = [items[255], ..., items[0]]   (full)
///   chunk_1  = items[256..]  reversed = [items[last], ..., items[256]] (partial)
///   write order = [chunk_1, chunk_0]   (partial chunk written first)
///
/// fxs-print undoes the per-chunk reversal, so it sees:
///   chunk_1 forward = items[256..] in forward order
///   chunk_0 forward = items[0..256] in forward order
fn mk_list_chunks(items: &[Term]) -> Vec<eetf::List> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut chunks: Vec<eetf::List> = items
        .chunks(256)
        .map(|chunk| {
            let mut elems: Vec<Term> = chunk.to_vec();
            elems.reverse();
            eetf::List { elements: elems }
        })
        .collect();
    // Mirror fxs_mk_list_chunks: partial (last) chunk is prepended to CAcc,
    // so it appears first in the written output.
    chunks.reverse();
    chunks
}

/// Split `items` into chunks for the HashDict section, mirroring
/// `fxs_write_dict` in `confd_rt_tools.erl`.
///
/// `fxs_write_dict` uses a fold with N starting at 0: items are prepended to
/// Acc while `N < 256`, then when `N == 256` the *current* item is prepended
/// (making 257 total) and the chunk is flushed.  So each full chunk has **257**
/// items, and the remaining partial chunk is written last.
///
/// So for `items = [V1, ..., V275]` (275 items):
///   chunk_0 = items[0..257] reversed  (257 items, written first)
///   chunk_1 = items[257..275] reversed (18 items, written last)
fn mk_dict_chunks(items: &[Term]) -> Vec<eetf::List> {
    items
        .chunks(257)
        .map(|chunk| {
            let mut elems: Vec<Term> = chunk.to_vec();
            elems.reverse();
            eetf::List { elements: elems }
        })
        .collect()
}
