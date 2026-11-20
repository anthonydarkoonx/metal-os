//! What an encrypted data volume needs, `no_std`, no heap, no unsafe.
//!
//! * `hmac_sha512` and `pbkdf2_hmac_sha512` (a passphrase's key-derivation function), checked against RFC 4231 and Python's `hashlib`.
//! * The **volume header** (one 4 KiB block at the start of the volume): a random 32-byte *volume key* sealed under up to four *key slots*. A slot is
//!   a passphrase (the key-encrypting key comes from PBKDF2) or a device secret (HMAC of a per-device secret). Any slot opens the volume; slots can be
//!   added and removed while the volume is open (change the passphrase, add a recovery slot) without touching the data. Everything about a slot
//!   (kind, cost, salt, position) is bound into the seal, so a changed header opens nothing.
//! * **Block sealing**: a data block is encrypted with ChaCha20-Poly1305 under the volume key with a fresh random nonce; the 28 bytes it leaves
//!   behind (nonce and tag) go in the block's metadata. The block's position and the volume's id are authenticated, so a block cannot be moved to
//!   another place or another volume unnoticed. (An old copy of the *same* block put back is not noticed: no rollback protection.)
#![forbid(unsafe_code)]
#![no_std]

use metallium_aead::{key_from_bytes, open, seal, TAG_LEN};
use metallium_ed25519::Sha512;

pub const KEY_LEN: usize = 32;
pub const HEADER_LEN: usize = 4096;
pub const SLOTS: usize = 4;
/// Bytes a sealed block leaves in its metadata: the nonce (12) and the tag (16).
pub const META_LEN: usize = 28;

// ---- HMAC-SHA512 and PBKDF2 ---------------------------------------------------------------------------------------------------------

const BLOCK: usize = 128;

#[derive(Clone)]
pub struct Hmac {
    inner: Sha512,
    outer: Sha512,
}

impl Hmac {
    pub fn new(key: &[u8]) -> Hmac {
        let mut k = [0u8; BLOCK];
        if key.len() > BLOCK {
            let mut h = Sha512::new();
            h.update(key);
            k[..64].copy_from_slice(&h.finish());
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let (mut ipad, mut opad) = ([0x36u8; BLOCK], [0x5cu8; BLOCK]);
        for i in 0..BLOCK {
            ipad[i] ^= k[i];
            opad[i] ^= k[i];
        }
        let (mut inner, mut outer) = (Sha512::new(), Sha512::new());
        inner.update(&ipad);
        outer.update(&opad);
        Hmac { inner, outer }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    pub fn finish(self) -> [u8; 64] {
        let mut outer = self.outer;
        outer.update(&self.inner.finish());
        outer.finish()
    }
}

pub fn hmac_sha512(key: &[u8], data: &[u8]) -> [u8; 64] {
    let mut h = Hmac::new(key);
    h.update(data);
    h.finish()
}

/// PBKDF2 (RFC 8018) with HMAC-SHA512 as the pseudo-random function: fills `out` (any length) from `password` and `salt`.
pub fn pbkdf2_hmac_sha512(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    let base = Hmac::new(password);
    for (i, chunk) in out.chunks_mut(64).enumerate() {
        let mut h = base.clone();
        h.update(salt);
        h.update(&(i as u32 + 1).to_be_bytes());
        let mut u = h.finish();
        let mut t = u;
        for _ in 1..iterations.max(1) {
            let mut h = base.clone();
            h.update(&u);
            u = h.finish();
            for (a, b) in t.iter_mut().zip(u.iter()) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

// ---- the volume header --------------------------------------------------------------------------------------------------------------

const MAGIC: &[u8; 8] = b"MTLMVOL1";
const VERSION: u32 = 1;
const O_ID: usize = 16;
const O_SLOTS: usize = 64;
const SLOT_LEN: usize = 128;
// within a slot
const S_KIND: usize = 0;
const S_ITER: usize = 4;
const S_SALT: usize = 8;
const S_NONCE: usize = 40;
const S_WRAPPED: usize = 52;
const S_TAG: usize = 84;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A passphrase the user types.
    Passphrase = 1,
    /// A secret that belongs to the device (a chip's unique value, say).
    Device = 2,
    /// A random recovery key written down once (high entropy, so no slow derivation).
    Recovery = 3,
}

/// Does this block start a volume header?
pub fn is_header(b: &[u8]) -> bool {
    b.len() >= HEADER_LEN && &b[..8] == MAGIC && u32::from_le_bytes([b[8], b[9], b[10], b[11]]) == VERSION
}

/// A header with no slots for a volume with the given random `id`.
pub fn new_header(id: [u8; 16]) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..8].copy_from_slice(MAGIC);
    h[8..12].copy_from_slice(&VERSION.to_le_bytes());
    h[O_ID..O_ID + 16].copy_from_slice(&id);
    h
}

pub fn volume_id(h: &[u8; HEADER_LEN]) -> [u8; 16] {
    let mut id = [0u8; 16];
    id.copy_from_slice(&h[O_ID..O_ID + 16]);
    id
}

/// Which kind each slot holds (`None`: empty).
pub fn slot_kind(h: &[u8; HEADER_LEN], i: usize) -> Option<Kind> {
    match h[O_SLOTS + i * SLOT_LEN + S_KIND] {
        1 => Some(Kind::Passphrase),
        2 => Some(Kind::Device),
        3 => Some(Kind::Recovery),
        _ => None,
    }
}

/// The key-encrypting key a slot's secret gives.
fn kek(kind: Kind, secret: &[u8], salt: &[u8], iterations: u32) -> [u8; KEY_LEN] {
    let mut k = [0u8; KEY_LEN];
    match kind {
        Kind::Passphrase => pbkdf2_hmac_sha512(secret, salt, iterations, &mut k),
        Kind::Device | Kind::Recovery => {
            let mut h = Hmac::new(secret);
            h.update(if kind == Kind::Recovery { b"metallium-recovery-kek".as_slice() } else { b"metallium-volume-kek".as_slice() });
            h.update(salt);
            k.copy_from_slice(&h.finish()[..KEY_LEN]);
        }
    }
    k
}

/// What the seal of slot `i` authenticates: the volume, the slot's place and its kind, cost and salt.
fn slot_aad(h: &[u8; HEADER_LEN], i: usize, out: &mut [u8; 16 + 1 + 1 + 4 + 32]) {
    let s = &h[O_SLOTS + i * SLOT_LEN..O_SLOTS + (i + 1) * SLOT_LEN];
    out[..16].copy_from_slice(&h[O_ID..O_ID + 16]);
    out[16] = i as u8;
    out[17] = s[S_KIND];
    out[18..22].copy_from_slice(&s[S_ITER..S_ITER + 4]);
    out[22..54].copy_from_slice(&s[S_SALT..S_SALT + 32]);
}

/// Put the volume key into slot `i`, openable with `secret`. `iterations` is the PBKDF2 cost (ignored for a device secret); `salt` and `nonce`
/// must be random. Replaces whatever the slot held.
pub fn set_slot(h: &mut [u8; HEADER_LEN], i: usize, kind: Kind, secret: &[u8], iterations: u32, salt: [u8; 32], nonce: [u8; 12], volume_key: &[u8; KEY_LEN]) {
    assert!(i < SLOTS);
    let iterations = if kind == Kind::Passphrase { iterations.max(1) } else { 1 };
    let base = O_SLOTS + i * SLOT_LEN;
    h[base..base + SLOT_LEN].fill(0);
    h[base + S_KIND] = kind as u8;
    h[base + S_ITER..base + S_ITER + 4].copy_from_slice(&iterations.to_le_bytes());
    h[base + S_SALT..base + S_SALT + 32].copy_from_slice(&salt);
    h[base + S_NONCE..base + S_NONCE + 12].copy_from_slice(&nonce);
    let mut aad = [0u8; 54];
    slot_aad(h, i, &mut aad);
    let key = key_from_bytes(&kek(kind, secret, &salt, iterations));
    let mut wrapped = *volume_key;
    let tag = seal(&key, &nonce, &aad, &mut wrapped);
    h[base + S_WRAPPED..base + S_WRAPPED + KEY_LEN].copy_from_slice(&wrapped);
    h[base + S_TAG..base + S_TAG + TAG_LEN].copy_from_slice(&tag);
}

pub fn clear_slot(h: &mut [u8; HEADER_LEN], i: usize) {
    h[O_SLOTS + i * SLOT_LEN..O_SLOTS + (i + 1) * SLOT_LEN].fill(0);
}

/// Try every slot of `kind` with `secret`; the volume key of the first that opens.
pub fn unlock(h: &[u8; HEADER_LEN], kind: Kind, secret: &[u8]) -> Option<[u8; KEY_LEN]> {
    if !is_header(h) {
        return None;
    }
    for i in 0..SLOTS {
        if slot_kind(h, i) != Some(kind) {
            continue;
        }
        let base = O_SLOTS + i * SLOT_LEN;
        let iterations = u32::from_le_bytes([h[base + S_ITER], h[base + S_ITER + 1], h[base + S_ITER + 2], h[base + S_ITER + 3]]);
        if iterations == 0 || iterations > 10_000_000 {
            continue;
        }
        let key = key_from_bytes(&kek(kind, secret, &h[base + S_SALT..base + S_SALT + 32], iterations));
        let mut aad = [0u8; 54];
        slot_aad(h, i, &mut aad);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&h[base + S_NONCE..base + S_NONCE + 12]);
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&h[base + S_TAG..base + S_TAG + TAG_LEN]);
        let mut vk = [0u8; KEY_LEN];
        vk.copy_from_slice(&h[base + S_WRAPPED..base + S_WRAPPED + KEY_LEN]);
        if open(&key, &nonce, &aad, &mut vk, &tag) {
            return Some(vk);
        }
    }
    None
}

// ---- block sealing ------------------------------------------------------------------------------------------------------------------

fn block_aad(id: &[u8; 16], blk: u64) -> [u8; 24] {
    let mut a = [0u8; 24];
    a[..16].copy_from_slice(id);
    a[16..].copy_from_slice(&blk.to_le_bytes());
    a
}

/// Encrypt `data` in place as block `blk` of the volume `id`, with a fresh random `nonce` (never one used before under this key); the metadata to
/// store beside it.
pub fn seal_block(volume_key: &[u8; KEY_LEN], id: &[u8; 16], blk: u64, nonce: [u8; 12], data: &mut [u8]) -> [u8; META_LEN] {
    let tag = seal(&key_from_bytes(volume_key), &nonce, &block_aad(id, blk), data);
    let mut meta = [0u8; META_LEN];
    meta[..12].copy_from_slice(&nonce);
    meta[12..].copy_from_slice(&tag);
    meta
}

/// Decrypt `data` in place; false (and `data` untouched) if it was changed, or is not this block of this volume.
pub fn open_block(volume_key: &[u8; KEY_LEN], id: &[u8; 16], blk: u64, meta: &[u8; META_LEN], data: &mut [u8]) -> bool {
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&meta[..12]);
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&meta[12..]);
    open(&key_from_bytes(volume_key), &nonce, &block_aad(id, blk), data, &tag)
}

#[cfg(test)]
mod tests;
