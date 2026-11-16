//! Audio: the protocol with the audio service (role 20, `audiod.rs`: a machine with an audio device supplies the driver) and the client side the shell's
//! `play` uses: WAV parsing, conversion to 48 kHz stereo 16 bit, and the ring writer. Samples reach the service through one shared ring (the kernel's
//! `OBJ_AUDIO_RING`): page 0 is a control block, pages 1.. hold `RING_FRAMES` frames of stereo 16 bit samples as the controller
//! wants them. The client writes frames and advances the write index; the service empties the ring into the FIFO whenever the
//! controller's "FIFO nearly empty" interrupt fires and advances the read index. A client that finds the service idle sends it
//! one message (a kick) to start; while it plays, no message is needed. All indices count frames and wrap at 2^32.
use crate::*;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

/// Handle of the ring mapping (both tasks) and of the register window (the service).
pub const RING_VA: usize = 0xB000_0000;
/// Capability slots: the service's own, and the shell's.
pub const SHELL_EP: u64 = 10;
pub const SHELL_RING: u64 = 11;

pub const RATE: u32 = 48_000;
pub const RING_FRAMES: u32 = 64 * 4096 / 4;
pub(crate) const DATA_OFF: usize = 4096;
// control block (u32 words)
pub(crate) const C_WRITE: usize = 0; // frames written by the client
pub(crate) const C_READ: usize = 1; // frames consumed by the service
pub(crate) const C_STATE: usize = 2; // 1 while the service is playing, 0 when idle (kick it)
pub(crate) const C_FLUSH: usize = 3; // the client sets 1 to throw away what is queued
pub(crate) const C_UNDERRUNS: usize = 4;
pub(crate) const OP_KICK: u64 = 1;

pub(crate) fn ctl(ring: usize, word: usize) -> *mut u32 {
    (ring + word * 4) as *mut u32
}
pub(crate) fn cget(ring: usize, word: usize) -> u32 {
    unsafe { read_volatile(ctl(ring, word)) }
}
pub(crate) fn cset(ring: usize, word: usize, v: u32) {
    unsafe { write_volatile(ctl(ring, word), v) }
}
pub(crate) fn slot_ptr(ring: usize, idx: u32) -> *mut u32 {
    (ring + DATA_OFF + (idx % RING_FRAMES) as usize * 4) as *mut u32
}

// ---------------------------------------------------------------------------------------------------------------------
// Client side (the shell)
// ---------------------------------------------------------------------------------------------------------------------

/// A client's view of the ring.
pub struct Player {
    ring: usize,
    pub cancelled: bool,
}

impl Player {
    pub fn open() -> Result<Player, &'static str> {
        if mem_map(SHELL_RING, RING_VA, true) != 0 {
            return Err("no audio on this machine");
        }
        Ok(Player { ring: va(RING_VA), cancelled: false })
    }

    pub fn underruns(&self) -> u32 {
        cget(self.ring, C_UNDERRUNS)
    }

    /// Queue frames (left | right << 16). `poll` runs while the ring is full; it returns true to cancel.
    pub fn push(&mut self, frames: &[u32], mut poll: impl FnMut() -> bool) {
        let mut i = 0;
        while i < frames.len() && !self.cancelled {
            let (w, r) = (cget(self.ring, C_WRITE), cget(self.ring, C_READ));
            let free = RING_FRAMES - w.wrapping_sub(r);
            if free == 0 {
                self.kick();
                if poll() {
                    self.cancel();
                    return;
                }
                sleep_us(5_000);
                continue;
            }
            let n = (free as usize).min(frames.len() - i);
            for k in 0..n {
                unsafe { write_volatile(slot_ptr(self.ring, w.wrapping_add(k as u32)), frames[i + k]) };
            }
            fence(Ordering::SeqCst);
            cset(self.ring, C_WRITE, w.wrapping_add(n as u32));
            i += n;
            self.kick();
        }
    }

    /// Start the service if it is idle (it re-checks the ring before it sleeps, so one look at the state is enough).
    fn kick(&self) {
        fence(Ordering::SeqCst);
        if cget(self.ring, C_STATE) == 0 {
            let _ = call(SHELL_EP, [OP_KICK, 0, 0]);
        }
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
        cset(self.ring, C_FLUSH, 1);
    }

    /// Wait until everything queued has been played (or `poll` says cancel).
    pub fn drain(&mut self, mut poll: impl FnMut() -> bool) {
        loop {
            let pending = cget(self.ring, C_WRITE) != cget(self.ring, C_READ);
            let playing = cget(self.ring, C_STATE) != 0;
            if !pending && !playing {
                return;
            }
            if self.cancelled {
                return;
            }
            if pending {
                self.kick();
            }
            if poll() {
                self.cancel();
                return;
            }
            sleep_us(10_000);
        }
    }
}

/// What a WAV file says about its samples.
#[derive(Clone, Copy)]
pub struct Format {
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
}

/// Parse a RIFF/WAVE header from the start of a file: the format and where the sample data starts and how long it is
/// (`None` for the length when the chunk header says nothing sensible, i.e. play to the end of the file).
pub fn parse_wav(b: &[u8]) -> Result<(Format, usize, Option<usize>), &'static str> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file");
    }
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let mut pos = 12;
    let mut fmt: Option<Format> = None;
    while pos + 8 <= b.len() {
        let size = u32_at(pos + 4) as usize;
        let body = pos + 8;
        match &b[pos..pos + 4] {
            b"fmt " => {
                if size < 16 || body + 16 > b.len() {
                    return Err("short fmt chunk");
                }
                let mut tag = u16_at(body);
                if tag == 0xFFFE && size >= 26 && body + 26 <= b.len() {
                    tag = u16_at(body + 24); // WAVE_FORMAT_EXTENSIBLE: the sub-format's first word
                }
                if tag != 1 {
                    return Err("only uncompressed PCM is supported (no float, ADPCM, MP3 ...)");
                }
                fmt = Some(Format { channels: u16_at(body + 2), rate: u32_at(body + 4), bits: u16_at(body + 14) });
            }
            b"data" => {
                let f = fmt.ok_or("data chunk before the fmt chunk")?;
                if f.channels == 0 || !matches!(f.bits, 8 | 16 | 24 | 32) || f.rate < 4000 || f.rate > 192_000 {
                    return Err("unsupported sample format (need 8/16/24/32 bit PCM, 4-192 kHz)");
                }
                let len = if size == 0 || size == 0xFFFF_FFFF { None } else { Some(size) };
                return Ok((f, body, len));
            }
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    Err("no data chunk in the first part of the file")
}

/// Turns the bytes of a PCM stream into 48 kHz stereo 16 bit frames (linear interpolation when the rates differ).
pub struct Converter {
    fmt: Format,
    step: u32, // source frames per output frame, Q16
    pos: u32,  // position between `prev` and the next source frame, Q16
    prev: Option<(i32, i32)>,
    volume: i32, // percent
    carry: Vec<u8>,
}

impl Converter {
    pub fn new(fmt: Format, volume: u32) -> Converter {
        Converter { fmt, step: (((fmt.rate as u64) << 16) / RATE as u64) as u32, pos: 0, prev: None, volume: volume.min(200) as i32, carry: Vec::new() }
    }

    fn sample(&self, b: &[u8]) -> i32 {
        match self.fmt.bits {
            8 => (b[0] as i32 - 128) << 8,
            16 => i16::from_le_bytes([b[0], b[1]]) as i32,
            24 => i16::from_le_bytes([b[1], b[2]]) as i32,
            _ => i16::from_le_bytes([b[2], b[3]]) as i32,
        }
    }

    /// Convert `data` (any length: a partial frame is kept for the next call) and append the frames to `out`.
    pub fn convert(&mut self, data: &[u8], out: &mut Vec<u32>) {
        let bytes = (self.fmt.bits / 8) as usize;
        let frame = bytes * self.fmt.channels as usize;
        let mut buf;
        let src: &[u8] = if self.carry.is_empty() {
            data
        } else {
            buf = core::mem::take(&mut self.carry);
            buf.extend_from_slice(data);
            &buf
        };
        let whole = src.len() / frame * frame;
        for f in src[..whole].chunks_exact(frame) {
            let l = self.sample(&f[..bytes]);
            let r = if self.fmt.channels > 1 { self.sample(&f[bytes..2 * bytes]) } else { l };
            self.feed(l, r, out);
        }
        self.carry = src[whole..].to_vec();
    }

    fn feed(&mut self, l: i32, r: i32, out: &mut Vec<u32>) {
        let Some((pl, pr)) = self.prev else {
            self.prev = Some((l, r));
            return;
        };
        while self.pos < 65536 {
            let f = self.pos as i64;
            let mix = |a: i32, b: i32| (a as i64 + ((b as i64 - a as i64) * f >> 16)) as i32;
            out.push(self.pack(mix(pl, l), mix(pr, r)));
            self.pos += self.step;
        }
        self.pos -= 65536;
        self.prev = Some((l, r));
    }

    fn pack(&self, l: i32, r: i32) -> u32 {
        let g = |s: i32| (s * self.volume / 100).clamp(-32768, 32767) as i16 as u16 as u32;
        g(l) | g(r) << 16
    }

    /// A tone as ready-made frames (a parabola-approximated sine).
    pub fn tone(hz: u32, frames: usize, volume: u32, out: &mut Vec<u32>) {
        let c = Converter::new(Format { channels: 2, rate: RATE, bits: 16 }, volume);
        let period = (RATE / hz.max(1)).max(2) as i64;
        for n in 0..frames as i64 {
            let p = (n * 2) % (2 * period);
            let (neg, q) = if p >= period { (true, p - period) } else { (false, p) };
            let x = (q << 15) / period;
            let xx = (x * (32768 - x)) >> 15;
            let s = ((16 * xx * 32768) / (5 * 32768 - 4 * xx) * 12000 / 32768) as i32;
            let v = if neg { -s } else { s };
            out.push(c.pack(v, v));
        }
    }
}
