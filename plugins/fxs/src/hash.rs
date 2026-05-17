//! Erlang `phash2` implementation matching `confd_rt_tools:mk_hash_value/1`.
//!
//! `phash2_atom(name)` computes `erlang:phash2(<<name/binary>>, 0x7fffffff)`,
//! which is the hash algorithm used by yanger_fxs for all node/namespace hashes.

/// Compute `erlang:phash2(Binary, 0xffffffff)`.
///
/// Used for t<hash> anonymous type names in yanger_fxs.
/// The `data` argument should include the ETF version byte (131) and the encoded term.
pub fn phash2_bytes(data: &[u8]) -> u32 {
    const HCONST_13: u32 = 0x08d12e65;
    let h = block_hash(data, HCONST_13);
    h % 0xffff_ffff
}

/// Compute `erlang:phash2(<<name>>, 0x7fffffff)`.
///
/// Includes the same collision overrides as `confd_rt_tools:mk_hash_value/1`.
pub fn phash2_atom(name: &str) -> u32 {
    // Override table from confd_rt_tools.erl to avoid collisions
    match name {
        "community-id" => return 2738413126,
        "lag_id" => return 2945934752,
        "fabric-interfaces" => return 4211803770,
        "ejpol-items" => return 3314060139,
        "isis-context-information" => return 3390256607,
        "si-nat-icmpv6-errors-local-nptv6" => return 2933833315,
        "tBgpInstanceParamsExtTblLstCh" => return 3511111249,
        "java-uninitialized" => return 2688234466,
        "lldp-remote-chassis-id-subtype" => return 2901139796,
        "graceful-restart-restart-time" => return 4149982121,
        "bgp-lsp" => return 3575648194,
        _ => {}
    }
    const HCONST_13: u32 = 0x08d12e65;
    let h = block_hash(name.as_bytes(), HCONST_13);
    h % 0x7fff_ffff
}

#[inline(always)]
fn mix(a: u32, b: u32, c: u32) -> (u32, u32, u32) {
    let a = a.wrapping_sub(b).wrapping_sub(c) ^ (c >> 13);
    let b = b.wrapping_sub(c).wrapping_sub(a) ^ (a << 8);
    let c = c.wrapping_sub(a).wrapping_sub(b) ^ (b >> 13);
    let a = a.wrapping_sub(b).wrapping_sub(c) ^ (c >> 12);
    let b = b.wrapping_sub(c).wrapping_sub(a) ^ (a << 16);
    let c = c.wrapping_sub(a).wrapping_sub(b) ^ (b >> 5);
    let a = a.wrapping_sub(b).wrapping_sub(c) ^ (c >> 3);
    let b = b.wrapping_sub(c).wrapping_sub(a) ^ (a << 10);
    let c = c.wrapping_sub(a).wrapping_sub(b) ^ (b >> 15);
    (a, b, c)
}

fn block_hash(data: &[u8], initval: u32) -> u32 {
    const HCONST: u32 = 0x9e3779b9;
    let mut a: u32 = HCONST;
    let mut b: u32 = HCONST;
    let mut c: u32 = initval;
    let n = data.len();
    let mut i = 0;
    while i + 12 <= n {
        a = a.wrapping_add(u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]));
        b = b.wrapping_add(u32::from_le_bytes([
            data[i + 4],
            data[i + 5],
            data[i + 6],
            data[i + 7],
        ]));
        c = c.wrapping_add(u32::from_le_bytes([
            data[i + 8],
            data[i + 9],
            data[i + 10],
            data[i + 11],
        ]));
        (a, b, c) = mix(a, b, c);
        i += 12;
    }
    let tail = &data[i..];
    c = c.wrapping_add(n as u32);
    if tail.len() >= 11 {
        c = c.wrapping_add((tail[10] as u32) << 24);
    }
    if tail.len() >= 10 {
        c = c.wrapping_add((tail[9] as u32) << 16);
    }
    if tail.len() >= 9 {
        c = c.wrapping_add((tail[8] as u32) << 8);
    }
    if tail.len() >= 8 {
        b = b.wrapping_add((tail[7] as u32) << 24);
    }
    if tail.len() >= 7 {
        b = b.wrapping_add((tail[6] as u32) << 16);
    }
    if tail.len() >= 6 {
        b = b.wrapping_add((tail[5] as u32) << 8);
    }
    if tail.len() >= 5 {
        b = b.wrapping_add(tail[4] as u32);
    }
    if tail.len() >= 4 {
        a = a.wrapping_add((tail[3] as u32) << 24);
    }
    if tail.len() >= 3 {
        a = a.wrapping_add((tail[2] as u32) << 16);
    }
    if tail.len() >= 2 {
        a = a.wrapping_add((tail[1] as u32) << 8);
    }
    if tail.len() >= 1 {
        a = a.wrapping_add(tail[0] as u32);
    }
    let (_, _, c) = mix(a, b, c);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_hashes() {
        // Verified against Erlang: erlang:phash2(<<"top">>, 16#7fffffff)
        assert_eq!(phash2_atom("top"), 703743881);
        // erlang:phash2(<<"name">>, 16#7fffffff)
        assert_eq!(phash2_atom("name"), 1998270519);
        // erlang:phash2(<<"http://example.com/test">>, 16#7fffffff)
        assert_eq!(phash2_atom("http://example.com/test"), 1089280427);
    }

    #[test]
    fn test_override_table() {
        // Override: should return fixed value, not computed hash
        assert_eq!(phash2_atom("community-id"), 2738413126);
        assert_eq!(phash2_atom("lag_id"), 2945934752);
    }
}
