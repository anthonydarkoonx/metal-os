extern crate std;
use super::*;
use std::vec::Vec;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

fn h32(s: &str) -> [u8; 32] {
    hex(s).try_into().unwrap()
}

fn h64(s: &str) -> [u8; 64] {
    hex(s).try_into().unwrap()
}

#[test]
fn sha512_known_answers() {
    assert_eq!(
        sha512(b"abc").to_vec(),
        hex("ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f")
    );
    assert_eq!(
        sha512(b"").to_vec(),
        hex("cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e")
    );
    // across the block-boundary cases of the padding
    for n in [0usize, 1, 55, 56, 111, 112, 113, 127, 128, 129, 255, 256, 1000] {
        let data: Vec<u8> = (0..n).map(|i| (i * 7 + 1) as u8).collect();
        let mut s = Sha512::new();
        for chunk in data.chunks(13) {
            s.update(chunk);
        }
        assert_eq!(s.finish(), sha512(&data), "length {n}");
    }
}

// RFC 8032 section 7.1 (vectors 1 and 2), then signatures made by another implementation (the Python `cryptography` package)
const V: [(&str, &str, &str, &str); 5] = [
    ("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a", "", "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b", ""),
    ("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c", "72", "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00", ""),
    ("47bef8411b3cc828024c9bfb84e39e928b5086915e4992245b8def5947c48920", "4d6574616c204f5320617070207061636b616765", "c889159ccef6747e33e81984a4fa7450c3dba18338faa3551d3347050729a696e5f3671c845dbc5b3d366b674c24549994fc1f86b75e8c4b3a5a31f36a553105", ""),
    ("3ebeb1272e9b1c937a125c79c677f0fe90f9fcf38db349bd8e50f07a25fc320b", "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff", "f890847b706537e00043463a98891f249acadd4200504443e2e0f493037e2aca24df6d4b160c6735cb30d094f2874a61ec1ab78f484a683643d3fb9e95bb0006", ""),
    ("f9b90fc54f7f0ee94609d790907bfce894f7f71d275b7d640eb423b429dce6fe", "787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878", "46d2f4745e8b14711892867d138c10770e8a386569cca7bae2054861c0a4ab40174b3f560a91bf02016bd474a2b7102efd52958465b60fedf1827f5a0d53c403", ""),
];

#[test]
fn rfc8032_vectors() {
    for (i, (pk, msg, sig, _)) in V.iter().enumerate().take(if cfg!(miri) { 2 } else { usize::MAX }) {
        let (pk, msg, sig) = (h32(pk), hex(msg), h64(sig));
        assert!(verify(&pk, &msg, &sig), "vector {}", i + 1);
        // any change is caught: the message, each byte of the signature, the key
        let mut m2 = msg.clone();
        m2.push(0);
        assert!(!verify(&pk, &m2, &sig));
        for b in (0..64).step_by(if cfg!(miri) { 9 } else { 1 }) {
            let mut s2 = sig;
            s2[b] ^= 1;
            assert!(!verify(&pk, &msg, &s2), "vector {} signature byte {b}", i + 1);
        }
        let mut pk2 = pk;
        pk2[5] ^= 0x10;
        assert!(!verify(&pk2, &msg, &sig));
    }
}

#[test]
fn group_order_and_malleability() {
    // S + L is another valid-looking scalar for the same signature: it must be refused
    let (pk, sig) = (h32(V[0].0), h64(V[0].2));
    let mut s = limbs(sig[32..].try_into().unwrap());
    let l = limbs(&L);
    let mut carry = 0u64;
    for i in 0..4 {
        let (a, c1) = s[i].overflowing_add(l[i]);
        let (b, c2) = a.overflowing_add(carry);
        s[i] = b;
        carry = (c1 | c2) as u64;
    }
    let mut bad = sig;
    for i in 0..4 {
        bad[32 + 8 * i..40 + 8 * i].copy_from_slice(&s[i].to_le_bytes());
    }
    if bad[63] & 0xE0 == 0 {
        assert!(!verify(&pk, b"", &bad));
    }
    // the group order itself: [L]B is the identity
    let c = Curve::new();
    let p = c.mul(&c.base(), &L);
    assert!(p.x.eq(Fe::ZERO) && p.y.eq(p.z));
    // the base point encodes as in the RFC
    assert_eq!(c.encode(&c.base()).to_vec(), hex("5866666666666666666666666666666666666666666666666666666666666666"));
}

#[test]
fn field_arithmetic() {
    let a = Fe::from_bytes(&h32("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f00"));
    assert!(a.mul(a.invert()).eq(Fe::ONE));
    assert!(a.sub(a).eq(Fe::ZERO));
    assert!(a.add(a.neg()).eq(Fe::ZERO));
    // p itself reduces to zero; p - 1 is -1
    let mut p = [0xFFu8; 32];
    p[0] = 0xED;
    p[31] = 0x7F;
    assert_eq!(Fe::from_bytes(&p).to_bytes(), [0u8; 32]);
    assert!(sqrt_m1().square().eq(Fe::ONE.neg()));
}

#[test]
fn reject_garbage_keys_and_noise() {
    let sig = h64(V[0].2);
    // a y that is not on the curve, and y >= p
    assert!(!verify(&[0xFF; 32], b"", &sig));
    let mut s = 0x1234_5678u32;
    for _ in 0..200 {
        let mut pk = [0u8; 32];
        let mut sg = [0u8; 64];
        for b in pk.iter_mut().chain(sg.iter_mut()) {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (s >> 24) as u8;
        }
        assert!(!verify(&pk, b"hello", &sg));
    }
}
