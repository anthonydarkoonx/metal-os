extern crate std;
use super::*;
use std::vec::Vec;

fn hex(b: &[u8]) -> std::string::String {
    b.iter().map(|x| std::format!("{x:02x}")).collect()
}

#[test]
fn rfc8439_block_function_vector() {
    // RFC 8439 section 2.3.2: key 00 01 .. 1f, nonce 00 00 00 09 00 00 00 4a 00 00 00 00, counter 1
    let key = words(&(0..32u8).collect::<Vec<_>>());
    let nonce = [u32::from_le_bytes([0, 0, 0, 9]), u32::from_le_bytes([0, 0, 0, 0x4a]), 0];
    assert_eq!(
        hex(&chacha20_block(&key, 1, &nonce)),
        "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4ed2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
    );
}

#[test]
fn zero_key_block_vector() {
    // RFC 7539 appendix A.1, test vector #1
    assert_eq!(
        hex(&chacha20_block(&[0; 8], 0, &[0; 3])),
        "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586"
    );
}

#[test]
fn same_seed_same_stream_different_seed_different_stream() {
    let (mut a, mut b, mut c) = (Rng::from_seed([7; 32]), Rng::from_seed([7; 32]), Rng::from_seed([8; 32]));
    let (va, vb, vc): (Vec<u64>, Vec<u64>, Vec<u64>) = ((0..64).map(|_| a.next_u64()).collect(), (0..64).map(|_| b.next_u64()).collect(), (0..64).map(|_| c.next_u64()).collect());
    assert_eq!(va, vb);
    assert_ne!(va, vc);
}

#[test]
fn no_value_repeats_and_streams_cross_refill_boundaries() {
    let mut r = Rng::from_seed([1; 32]);
    let v: Vec<u64> = (0..10_000).map(|_| r.next_u64()).collect();
    let mut s = v.clone();
    s.sort();
    s.dedup();
    assert_eq!(s.len(), v.len());
    // byte-wise reading yields the same stream as word-wise reading
    let (mut a, mut b) = (Rng::from_seed([2; 32]), Rng::from_seed([2; 32]));
    let mut bytes = [0u8; 100];
    a.fill(&mut bytes);
    let mut words = Vec::new();
    for _ in 0..13 {
        words.extend_from_slice(&b.next_u64().to_le_bytes());
    }
    assert_eq!(&bytes[..], &words[..100]);
}

#[test]
fn an_output_does_not_reveal_the_next_one() {
    // SplitMix64 is a bijection of a counter: from one output the next can be computed. Here the state is a key that was
    // replaced by cipher output, so predicting the next word from earlier words by any simple rule must fail.
    let mut r = Rng::from_seed([3; 32]);
    let v: Vec<u64> = (0..2000).map(|_| r.next_u64()).collect();
    let linear = v.windows(3).filter(|w| w[2].wrapping_sub(w[1]) == w[1].wrapping_sub(w[0])).count();
    assert_eq!(linear, 0);
    // every bit position is set about half of the time
    for bit in 0..64 {
        let ones = v.iter().filter(|x| *x >> bit & 1 == 1).count();
        assert!((800..1200).contains(&ones), "bit {bit} set {ones} times of 2000");
    }
}

#[test]
fn mixing_changes_the_stream_and_is_deterministic() {
    let (mut a, mut b, mut c) = (Rng::from_seed([4; 32]), Rng::from_seed([4; 32]), Rng::from_seed([4; 32]));
    a.mix(b"interrupt at 12345");
    b.mix(b"interrupt at 12345");
    c.mix(b"interrupt at 12346");
    let (x, y, z) = (a.next_u64(), b.next_u64(), c.next_u64());
    assert_eq!(x, y);
    assert_ne!(x, z);
    assert_ne!(x, Rng::from_seed([4; 32]).next_u64());
    // long input (several chunks) and the empty input both work
    let mut d = Rng::from_seed([4; 32]);
    d.mix(&[9u8; 100]);
    d.mix(&[]);
    let _ = d.next_u64();
}

#[test]
fn below_is_in_range_and_uniform() {
    let mut r = Rng::from_seed([5; 32]);
    for n in [1u64, 2, 3, 7, 4096, 3000, u64::MAX / 3 * 2 + 1] {
        for _ in 0..200 {
            assert!(r.below(n) < n.max(1));
        }
    }
    let mut counts = [0usize; 10];
    for _ in 0..100_000 {
        counts[r.below(10) as usize] += 1;
    }
    for (i, &c) in counts.iter().enumerate() {
        assert!((9_300..10_700).contains(&c), "value {i} drawn {c} times of 100000");
    }
    // a range that does not divide 2^64 shows no bias towards the low end (rejection sampling)
    let big = u64::MAX / 3 * 2 + 1;
    let low = (0..20_000).filter(|_| r.below(big) < big / 2).count();
    assert!((9_500..10_500).contains(&low), "{low}");
}
