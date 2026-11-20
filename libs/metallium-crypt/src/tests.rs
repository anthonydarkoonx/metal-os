extern crate std;
use std::vec::Vec;
use super::*;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

// (expected values: RFC 4231 for HMAC, and Python's hashlib / hmac for the rest)
#[test]
fn hmac_matches_the_rfc_and_python() {
    assert_eq!(hmac_sha512(&[0x0b; 20], b"Hi There").to_vec(), hex("87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"));
    // a key longer than the block is hashed first
    assert_eq!(
        hmac_sha512(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First").to_vec(),
        hex("80b24263c7c1a3ebb71493c1dd7be8b49b46d1f41b4aeec1121b013783f8f3526b56d037e05f2598bd0fd2215d6a1e5295e64f73f63f0aec8b915a985d786598")
    );
}

#[test]
fn pbkdf2_matches_python() {
    let mut o = [0u8; 64];
    pbkdf2_hmac_sha512(b"password", b"salt", 1, &mut o);
    assert_eq!(o.to_vec(), hex("867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce"));
    pbkdf2_hmac_sha512(b"password", b"salt", 2, &mut o);
    assert_eq!(o.to_vec(), hex("e1d9c16aa681708a45f5c7c4e215ceb66e011a2e9f0040713f18aefdb866d53cf76cab2868a39b9f7840edce4fef5a82be67335c77a6068e04112754f27ccf4e"));
    // more than one output block, and many iterations
    let mut long = [0u8; 100];
    pbkdf2_hmac_sha512(b"password", b"salt", 4096, &mut long);
    assert_eq!(long.to_vec(), hex("d197b1b33db0143e018b12f3d1d1479e6cdebdcc97c5c0f87f6902e072f457b5143f30602641b3d55cd335988cb36b84376060ecd532e039b742a239434af2d5d6883f0be4c24d363b638f4c2f8d917533cd4158937d0b490697a64adadb07f180c32308"));
    // a password longer than the block
    let mut k = [0u8; 32];
    pbkdf2_hmac_sha512(&[b'k'; 200], b"NaClNaClNaCl", 10, &mut k);
    assert_eq!(k.to_vec(), hex("223de4c55ca8a97cef66469888166cb39276ab75808f2a82ca29f8b611dce874"));
}

fn salt(n: u8) -> [u8; 32] {
    [n; 32]
}
fn nonce(n: u8) -> [u8; 12] {
    [n; 12]
}

#[test]
fn a_passphrase_slot_opens_with_the_passphrase_only() {
    let vk = [7u8; 32];
    let mut h = new_header([9; 16]);
    set_slot(&mut h, 0, Kind::Passphrase, b"correct horse", 50, salt(1), nonce(1), &vk);
    assert!(is_header(&h));
    assert_eq!(unlock(&h, Kind::Passphrase, b"correct horse"), Some(vk));
    assert_eq!(unlock(&h, Kind::Passphrase, b"correct horsf"), None);
    assert_eq!(unlock(&h, Kind::Passphrase, b""), None);
    assert_eq!(unlock(&h, Kind::Device, b"correct horse"), None, "a slot of another kind is not tried");
}

#[test]
fn slots_are_independent_and_can_be_changed() {
    let vk = [3u8; 32];
    let mut h = new_header([1; 16]);
    set_slot(&mut h, 0, Kind::Passphrase, b"first", 20, salt(1), nonce(1), &vk);
    set_slot(&mut h, 2, Kind::Device, &[0x55; 32], 0, salt(2), nonce(2), &vk);
    assert_eq!(unlock(&h, Kind::Passphrase, b"first"), Some(vk));
    assert_eq!(unlock(&h, Kind::Device, &[0x55; 32]), Some(vk));
    assert_eq!(unlock(&h, Kind::Device, &[0x56; 32]), None);
    // change the passphrase: the old one stops working, the device slot is untouched
    set_slot(&mut h, 0, Kind::Passphrase, b"second", 20, salt(3), nonce(3), &vk);
    assert_eq!(unlock(&h, Kind::Passphrase, b"first"), None);
    assert_eq!(unlock(&h, Kind::Passphrase, b"second"), Some(vk));
    assert_eq!(unlock(&h, Kind::Device, &[0x55; 32]), Some(vk));
    clear_slot(&mut h, 2);
    assert_eq!(unlock(&h, Kind::Device, &[0x55; 32]), None);
    assert_eq!(slot_kind(&h, 2), None);
}

#[test]
fn a_changed_header_opens_nothing() {
    let vk = [5u8; 32];
    let mut h = new_header([4; 16]);
    set_slot(&mut h, 1, Kind::Passphrase, b"pw", 20, salt(9), nonce(9), &vk);
    assert_eq!(unlock(&h, Kind::Passphrase, b"pw"), Some(vk));
    // every byte of the slot, and of the volume id, is bound into the seal (or is the sealed key itself)
    for at in (16..32).chain(64 + 128..64 + 256) {
        if at == 64 + 128 + 1 || at == 64 + 128 + 2 || at == 64 + 128 + 3 || at >= 64 + 128 + 100 {
            continue; // (padding that nothing reads)
        }
        let mut t = h;
        t[at] ^= 1;
        assert_eq!(unlock(&t, Kind::Passphrase, b"pw"), None, "byte {at}");
    }
    // moving a slot to another place breaks it too
    let mut moved = new_header([4; 16]);
    moved.copy_within(0..0, 0);
    let slot: Vec<u8> = h[64 + 128..64 + 256].to_vec();
    moved[64..64 + 128].copy_from_slice(&slot);
    assert_eq!(unlock(&moved, Kind::Passphrase, b"pw"), None);
}

#[test]
fn a_block_opens_only_where_and_as_it_was_sealed() {
    let key = [0x42u8; 32];
    let id = [0x11u8; 16];
    let plain: Vec<u8> = (0..4096).map(|i| (i * 7) as u8).collect();
    let mut data = plain.clone();
    let meta = seal_block(&key, &id, 77, nonce(5), &mut data);
    assert_ne!(data, plain);
    // another block number, another volume, another key, a changed byte, a changed tag: all refused, the data untouched
    for (k, i, b) in [(key, id, 78u64), (key, [0x12; 16], 77), ([0x43; 32], id, 77)] {
        let mut t = data.clone();
        assert!(!open_block(&k, &i, b, &meta, &mut t));
        assert_eq!(t, data);
    }
    let mut t = data.clone();
    t[1000] ^= 1;
    assert!(!open_block(&key, &id, 77, &meta, &mut t));
    let mut bad = meta;
    bad[20] ^= 1;
    let mut t = data.clone();
    assert!(!open_block(&key, &id, 77, &bad, &mut t));
    // the right way
    let mut t = data.clone();
    assert!(open_block(&key, &id, 77, &meta, &mut t));
    assert_eq!(t, plain);
}

#[test]
fn a_recovery_slot_opens_only_with_its_own_key_and_kind() {
    let vk = [8u8; 32];
    let mut h = new_header([2; 16]);
    set_slot(&mut h, 0, Kind::Passphrase, b"pw", 20, salt(1), nonce(1), &vk);
    set_slot(&mut h, 1, Kind::Recovery, &[0x77; 16], 0, salt(2), nonce(2), &vk);
    assert_eq!(unlock(&h, Kind::Recovery, &[0x77; 16]), Some(vk));
    assert_eq!(unlock(&h, Kind::Recovery, &[0x78; 16]), None);
    assert_eq!(unlock(&h, Kind::Passphrase, &[0x77; 16]), None, "a recovery key is not a passphrase");
    assert_eq!(unlock(&h, Kind::Device, &[0x77; 16]), None, "and not a device secret (the derivations are separated)");
    assert_eq!(slot_kind(&h, 1), Some(Kind::Recovery));
    // the passphrase can be replaced while the recovery slot stays
    set_slot(&mut h, 0, Kind::Passphrase, b"new", 20, salt(3), nonce(3), &vk);
    assert_eq!(unlock(&h, Kind::Passphrase, b"pw"), None);
    assert_eq!(unlock(&h, Kind::Passphrase, b"new"), Some(vk));
    assert_eq!(unlock(&h, Kind::Recovery, &[0x77; 16]), Some(vk));
}
