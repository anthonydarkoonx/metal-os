//! Cryptographic random numbers from the kernel (`GETRANDOM`): 40 bytes a call, from the generator that is seeded with the CPU's RNDR, the harness's seed
//! file and the virtio-rng device. Where the kernel has no real entropy it says so (`E_NOSYS`) and this module reports an error: a program that needs
//! secure random numbers (TLS) must then refuse to go on, not fall back to something weaker.
use core::sync::atomic::{AtomicU8, Ordering};

pub const GETRANDOM: u64 = 43;

/// The kernel's error for "no secure random numbers".
pub const E_NOSYS: i64 = -7;

/// Fill `buf` with random bytes; the kernel's error code if it has no secure generator.
pub fn fill(buf: &mut [u8]) -> Result<(), i64> {
    for chunk in buf.chunks_mut(40) {
        let r = crate::svc(GETRANDOM, 0, 0, 0, 0, 0);
        if r[0] as i64 != 0 {
            return Err(r[0] as i64);
        }
        let mut bytes = [0u8; 40];
        for i in 0..5 {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&r[i + 1].to_le_bytes());
        }
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    Ok(())
}

static AVAILABLE: AtomicU8 = AtomicU8::new(0); // 0 not asked, 1 yes, 2 no

/// Does the kernel have a secure random generator? (Asked once.)
pub fn available() -> bool {
    match AVAILABLE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let ok = fill(&mut [0u8; 1]).is_ok();
            AVAILABLE.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
            ok
        }
    }
}
