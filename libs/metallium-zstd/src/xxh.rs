//! XXH64 with seed 0 (the Zstandard frame content checksum keeps its low 32 bits). No allocation.


pub fn xxh64(data: &[u8]) -> u64 {
    const P1: u64 = 11400714785074694791;
    const P2: u64 = 14029467366897019727;
    const P3: u64 = 1609587929392839161;
    const P4: u64 = 9650029242287828579;
    const P5: u64 = 2870177450012600261;
    let rd64 = |b: &[u8]| u64::from_le_bytes(b[..8].try_into().unwrap());
    let rd32 = |b: &[u8]| u32::from_le_bytes(b[..4].try_into().unwrap()) as u64;
    let round = |acc: u64, x: u64| acc.wrapping_add(x.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1);
    let merge = |h: u64, v: u64| (h ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4);
    let mut p = data;
    let mut h;
    if data.len() >= 32 {
        let (mut v1, mut v2, mut v3, mut v4) = (P1.wrapping_add(P2), P2, 0u64, 0u64.wrapping_sub(P1));
        while p.len() >= 32 {
            v1 = round(v1, rd64(&p[0..]));
            v2 = round(v2, rd64(&p[8..]));
            v3 = round(v3, rd64(&p[16..]));
            v4 = round(v4, rd64(&p[24..]));
            p = &p[32..];
        }
        h = v1.rotate_left(1).wrapping_add(v2.rotate_left(7)).wrapping_add(v3.rotate_left(12)).wrapping_add(v4.rotate_left(18));
        h = merge(h, v1);
        h = merge(h, v2);
        h = merge(h, v3);
        h = merge(h, v4);
    } else {
        h = P5;
    }
    h = h.wrapping_add(data.len() as u64);
    while p.len() >= 8 {
        h = (h ^ round(0, rd64(p))).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        p = &p[8..];
    }
    if p.len() >= 4 {
        h = (h ^ rd32(p).wrapping_mul(P1)).rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        p = &p[4..];
    }
    for &b in p {
        h = (h ^ (b as u64).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}
