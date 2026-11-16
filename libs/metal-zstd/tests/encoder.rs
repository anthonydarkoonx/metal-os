//! The compressor against three judges: our own decoders, and the real `zstd` command-line tool
//! (a black-box oracle: it is run, never linked or copied).
use metal_zstd::encode::{PageWorkspace, Workspace};
use metal_zstd::{decompress, lite};
use std::io::Write;
use std::process::{Command, Stdio};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

type Big = Workspace<16384, 4097>;

/// A mix of shapes: zeros, one repeated byte, text-like, structured records, noise, mixtures.
fn samples() -> Vec<(String, Vec<u8>)> {
    let mut r = Rng(7);
    let mut v: Vec<(String, Vec<u8>)> = Vec::new();
    v.push(("empty".into(), vec![]));
    v.push(("one byte".into(), vec![42]));
    v.push(("two equal".into(), vec![9, 9]));
    for n in [3usize, 31, 32, 33, 255, 256, 257, 4095, 4096, 4097, 20000, 65791, 65792, 70000, 300_000] {
        v.push((format!("zeros {n}"), vec![0; n]));
        v.push((format!("noise {n}"), (0..n).map(|_| r.next() as u8).collect()));
        let text: Vec<u8> = (0..).flat_map(|i: usize| format!("line {i}: the quick brown fox jumps over the lazy dog\n").into_bytes()).take(n).collect();
        v.push((format!("text {n}"), text));
        let rec: Vec<u8> = (0..n).map(|i| ((i / 16) as u8).wrapping_mul(7) ^ (i % 16) as u8).collect();
        v.push((format!("records {n}"), rec));
        let mut mix = Vec::new();
        while mix.len() < n {
            let len = 1 + r.next() as usize % 300;
            match r.next() % 3 {
                0 => mix.extend((0..len).map(|_| r.next() as u8)),
                1 => mix.extend(std::iter::repeat((r.next() % 256) as u8).take(len)),
                _ => {
                    if mix.len() > 8 {
                        let from = r.next() as usize % (mix.len() - 4);
                        let l = len.min(mix.len() - from);
                        let copy: Vec<u8> = mix[from..from + l].to_vec();
                        mix.extend(copy);
                    }
                }
            }
        }
        mix.truncate(n);
        v.push((format!("mix {n}"), mix));
    }
    v
}

fn have_zstd() -> bool {
    Command::new("zstd").arg("--version").output().is_ok()
}

fn zstd_decode(frame: &[u8]) -> Result<Vec<u8>, String> {
    let mut c = Command::new("zstd").args(["-d", "-q", "-c"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdin = c.stdin.take().unwrap();
    let f = frame.to_vec();
    let t = std::thread::spawn(move || {
        let _ = stdin.write_all(&f);
    });
    let out = c.wait_with_output().unwrap();
    t.join().unwrap();
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

#[test]
fn round_trips_through_both_of_our_decoders() {
    let mut ws = Box::new(Big::new());
    for (name, data) in samples() {
        for checksum in [false, true] {
            let mut frame = vec![0u8; Big::max_compressed_len(data.len())];
            let n = ws.compress(&data, &mut frame, checksum).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            frame.truncate(n);
            let mut out = vec![0u8; data.len()];
            assert_eq!(lite::decompress(&frame, &mut out), Ok(data.len()), "{name} (lite, checksum {checksum})");
            assert_eq!(out, data, "{name} (lite)");
            let mut out2 = vec![0u8; data.len()];
            assert_eq!(decompress(&frame, &mut out2), Ok(data.len()), "{name} (full, checksum {checksum})");
            assert_eq!(out2, data, "{name} (full)");
        }
    }
}

#[test]
fn the_real_zstd_decodes_our_frames() {
    if !have_zstd() {
        eprintln!("zstd not installed: skipping the interoperability check");
        return;
    }
    let mut ws = Box::new(Big::new());
    let mut checked = 0;
    for (name, data) in samples() {
        for checksum in [false, true] {
            let mut frame = vec![0u8; Big::max_compressed_len(data.len())];
            let n = ws.compress(&data, &mut frame, checksum).unwrap();
            frame.truncate(n);
            let got = zstd_decode(&frame).unwrap_or_else(|e| panic!("{name} (checksum {checksum}): zstd rejected our frame: {e}"));
            assert!(got == data, "{name} (checksum {checksum}): zstd decoded different bytes");
            checked += 1;
        }
    }
    assert!(checked > 100);
}

#[test]
fn random_structured_data_many_times_against_the_real_zstd() {
    if !have_zstd() {
        return;
    }
    let mut r = Rng(99);
    let mut ws = Box::new(PageWorkspace::new());
    for round in 0..400 {
        // a page of a random mixture of copies, runs and noise at random scales
        let mut page = Vec::new();
        while page.len() < 4096 {
            let len = 1 + (r.next() as usize % (1 + (r.next() as usize % 400)));
            match r.next() % 4 {
                0 => page.extend((0..len).map(|_| r.next() as u8)),
                1 => page.extend(std::iter::repeat((r.next() % 4) as u8).take(len)),
                2 => page.extend((0..len).map(|i| b"abcdefgh"[i % 8])),
                _ => {
                    if page.len() > 16 {
                        let from = r.next() as usize % (page.len() - 8);
                        let l = len.min(page.len() - from);
                        let copy: Vec<u8> = page[from..from + l].to_vec();
                        page.extend(copy);
                    }
                }
            }
        }
        page.truncate(4096);
        let mut frame = vec![0u8; PageWorkspace::max_compressed_len(4096)];
        let n = ws.compress(&page, &mut frame, round % 2 == 0).unwrap();
        frame.truncate(n);
        assert!(n <= 4096 + 16);
        let mut out = vec![0u8; 4096];
        assert_eq!(lite::decompress(&frame, &mut out), Ok(4096), "round {round}");
        assert_eq!(out, page, "round {round}");
        if round % 8 == 0 {
            assert_eq!(zstd_decode(&frame).unwrap(), page, "round {round}: the real zstd disagrees");
        }
    }
}

#[test]
fn compression_actually_compresses() {
    let mut ws = Box::new(PageWorkspace::new());
    let mut size = |data: &[u8]| {
        let mut frame = vec![0u8; PageWorkspace::max_compressed_len(data.len())];
        ws.compress(data, &mut frame, false).unwrap()
    };
    assert!(size(&[0u8; 4096]) <= 12, "a zero page is a handful of bytes");
    let text: Vec<u8> = (0..).flat_map(|i: usize| format!("cold anonymous page {}\n", i % 7).into_bytes()).take(4096).collect();
    assert!(size(&text) < 400, "repetitive text shrinks a lot: {}", size(&text));
    let mut r = Rng(1);
    let noise: Vec<u8> = (0..4096).map(|_| r.next() as u8).collect();
    assert!(size(&noise) <= 4096 + 12, "incompressible data costs only the frame overhead (10 bytes)");
}

#[test]
fn output_too_small_is_an_error_not_a_panic() {
    let mut ws = Box::new(PageWorkspace::new());
    let data: Vec<u8> = (0..4096).map(|i| (i * 7 % 251) as u8).collect();
    for cap in 0..64 {
        let mut tiny = vec![0u8; cap];
        assert!(ws.compress(&data, &mut tiny, true).is_err());
    }
}

#[test]
fn corrupted_frames_never_panic_the_small_decoder() {
    let mut r = Rng(5);
    let mut ws = Box::new(PageWorkspace::new());
    let mut survivors = 0;
    for case in 0..30 {
        let page: Vec<u8> = (0..4096).map(|i| if case % 2 == 0 { b"abcabcabd xyz"[i % 13] } else { (i * (case + 3)) as u8 }).collect();
        let mut frame = vec![0u8; PageWorkspace::max_compressed_len(4096)];
        let n = ws.compress(&page, &mut frame, case % 3 == 0).unwrap();
        frame.truncate(n);
        for _ in 0..1000 {
            let mut f = frame.clone();
            for _ in 0..1 + r.next() % 3 {
                let at = r.next() as usize % f.len();
                f[at] ^= 1 << (r.next() % 8);
            }
            if r.next() % 5 == 0 {
                f.truncate(r.next() as usize % f.len());
            }
            let mut out = vec![0u8; 4096];
            if lite::decompress(&f, &mut out).is_ok() {
                survivors += 1; // a flip can land in literal bytes: legal, just different data
            }
        }
    }
    assert!(survivors < 30 * 1000, "most corruptions must be noticed");
}

// ---- the entropy coders: Huffman literals and transmitted FSE tables ----

/// Bytes with a skewed distribution over the whole 0..=255 range (so the Huffman description needs FSE-coded
/// weights, not 4-bit ones): `spread` is the scale of the exponential.
fn skewed(r: &mut Rng, n: usize, spread: f64) -> Vec<u8> {
    (0..n)
        .map(|_| {
            let u = (r.next() as f64 + 1.0) / 4294967297.0;
            ((-u.ln() * spread) as usize).min(255) as u8
        })
        .collect()
}

/// (literals block type, Huffman header byte class, sequence modes) of the first block of a one-block frame
/// with a known-size header; `None` for frames that are not that shape.
fn first_block(frame: &[u8]) -> Option<(u8, u8)> {
    let fhd = frame[4];
    let fcs = match fhd >> 6 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut at = 5 + fcs;
    let h = frame[at] as u32 | (frame[at + 1] as u32) << 8 | (frame[at + 2] as u32) << 16;
    at += 3;
    if h >> 1 & 3 != 2 {
        return None;
    }
    let b0 = frame[at];
    let ty = b0 & 3;
    let fmt = b0 >> 2 & 3;
    let lit_len = match ty {
        0 | 1 => {
            let (hdr, size) = match fmt {
                0 | 2 => (1, (b0 >> 3) as usize),
                1 => (2, (b0 >> 4) as usize | (frame[at + 1] as usize) << 4),
                _ => (3, (b0 >> 4) as usize | (frame[at + 1] as usize) << 4 | (frame[at + 2] as usize) << 12),
            };
            hdr + if ty == 0 { size } else { 1 }
        }
        _ => {
            let (hbytes, bits) = [(3usize, 10u32), (3, 10), (4, 14), (5, 18)][fmt as usize];
            let mut v = 0u64;
            for i in 0..hbytes {
                v |= (frame[at + i] as u64) << (8 * i);
            }
            hbytes + ((v >> 4 >> bits) & ((1 << bits) - 1)) as usize
        }
    };
    at += lit_len;
    let n0 = frame[at];
    at += 1 + match n0 {
        0..=127 => 0,
        128..=254 => 1,
        _ => 2,
    };
    if n0 == 0 {
        return Some((ty, 0xff));
    }
    Some((ty, frame[at]))
}

fn page_frame(data: &[u8]) -> Vec<u8> {
    let mut ws = Box::new(PageWorkspace::new());
    let mut frame = vec![0u8; PageWorkspace::max_compressed_len(data.len())];
    let n = ws.compress(data, &mut frame, false).unwrap();
    frame.truncate(n);
    frame
}

#[test]
fn text_pages_use_huffman_literals_and_fitted_tables() {
    let text: Vec<u8> = include_str!("../src/fse.rs").bytes().take(4096).collect();
    let frame = page_frame(&text);
    let (lit_ty, modes) = first_block(&frame).expect("a compressed block");
    assert_eq!(lit_ty, 2, "the literals of source text are Huffman-coded");
    assert!(modes != 0xff && modes & 0xfc != 0, "at least one sequence table is not the predefined one (modes byte {modes:#x})");
    let mut out = vec![0u8; 4096];
    assert_eq!(lite::decompress(&frame, &mut out), Ok(4096));
    assert!(out == text, "first difference at {:?}", out.iter().zip(&text).position(|(a, b)| a != b));
    assert_eq!(decompress(&frame, &mut vec![0u8; 4096]), Ok(4096));
}

#[test]
fn huffman_with_fse_coded_weights_and_every_literal_size() {
    // many distinct byte values: the tree description must be an FSE stream; sizes straddle the 1-stream /
    // 4-stream (1024) and the 3/4/5-byte header (16384) boundaries
    let mut r = Rng(31);
    type W = Workspace<65536, 16385>;
    let mut ws = Box::new(W::new());
    let mut used_fse_weights = 0;
    for &n in &[40usize, 100, 1000, 1023, 1024, 1025, 4000, 16383, 16384, 16385, 40000, 65536] {
        for spread in [8.0, 25.0, 60.0] {
            let data = skewed(&mut r, n, spread);
            let mut frame = vec![0u8; W::max_compressed_len(n)];
            let len = ws.compress(&data, &mut frame, true).unwrap();
            frame.truncate(len);
            let mut out = vec![0u8; n];
            assert_eq!(lite::decompress_with(&frame, &mut out, &mut vec![0u8; 65536]), Ok(n), "n={n} spread={spread}");
            assert_eq!(out, data, "n={n} spread={spread} (lite)");
            assert_eq!(decompress(&frame, &mut vec![0u8; n]), Ok(n), "n={n} spread={spread}");
            if have_zstd() {
                assert!(zstd_decode(&frame).unwrap() == data, "n={n} spread={spread}: zstd disagrees");
            }
            if let Some((2, _)) = first_block(&frame) {
                // header byte of the tree is at a known place only for 1-block frames of the page sizes; count by size
                if len < n * 9 / 10 && spread >= 25.0 {
                    used_fse_weights += 1;
                }
            }
        }
    }
    assert!(used_fse_weights > 3, "the wide-alphabet cases must actually compress with Huffman ({used_fse_weights})");
}

#[test]
fn fitted_tables_and_rle_streams_match_the_real_zstd() {
    if !have_zstd() {
        return;
    }
    // pages that make the three streams behave differently: constant offsets, constant lengths, few symbols
    let mut r = Rng(77);
    let mut ws = Box::new(PageWorkspace::new());
    let mut modes_seen = [false; 3];
    for round in 0..300 {
        let mut page = Vec::new();
        let (period, run) = (2 + r.next() as usize % 40, 4 + r.next() as usize % 30);
        while page.len() < 4096 {
            match round % 3 {
                0 => {
                    // equal-length matches at one distance: offset and match-length streams are RLE-ish
                    let start = page.len();
                    page.extend((0..period).map(|_| r.next() as u8));
                    for k in 0..run {
                        let b = page[start + k % period];
                        page.push(b);
                    }
                }
                1 => {
                    let len = 50 + r.next() as usize % 200;
                    page.extend(skewed(&mut r, len, 12.0))
                }
                _ => {
                    let v = r.next() as usize % 50;
                    page.extend(format!("key{v}=value{}; ", v * 7 % 13).into_bytes());
                }
            }
        }
        page.truncate(4096);
        let mut frame = vec![0u8; PageWorkspace::max_compressed_len(4096)];
        let n = ws.compress(&page, &mut frame, round % 2 == 0).unwrap();
        frame.truncate(n);
        if let Some((_, m)) = first_block(&frame) {
            if m != 0xff {
                for (i, seen) in modes_seen.iter_mut().enumerate() {
                    *seen |= (m >> (6 - 2 * i)) & 3 != 0;
                }
            }
        }
        let mut out = vec![0u8; 4096];
        assert_eq!(lite::decompress(&frame, &mut out), Ok(4096), "round {round}");
        assert_eq!(out, page, "round {round}");
        assert_eq!(zstd_decode(&frame).unwrap(), page, "round {round}: the real zstd disagrees");
    }
    assert!(modes_seen.iter().all(|&s| s), "every stream used a non-predefined table somewhere: {modes_seen:?}");
}

#[test]
fn corrupted_entropy_coded_frames_never_panic() {
    let mut r = Rng(1234);
    let text: Vec<u8> = include_str!("../src/encode.rs").bytes().take(4096).collect();
    let frames = [page_frame(&text), page_frame(&skewed(&mut r, 4096, 20.0)), page_frame(&skewed(&mut r, 4096, 70.0))];
    for frame in &frames {
        for _ in 0..4000 {
            let mut f = frame.clone();
            for _ in 0..1 + r.next() % 4 {
                let at = r.next() as usize % f.len();
                f[at] ^= 1 << (r.next() % 8);
            }
            if r.next() % 6 == 0 {
                f.truncate(r.next() as usize % f.len());
            }
            let mut out = vec![0u8; 4096];
            let _ = lite::decompress(&f, &mut out);
            let _ = decompress(&f, &mut out);
        }
    }
}

/// `cargo test -p metal-zstd --test encoder ratio_report -- --ignored --nocapture`: our compressed size per 4 KiB
/// page against the real `zstd` (levels 1 and 3, one frame per page) on real files.
#[test]
#[ignore]
fn ratio_report() {
    if !have_zstd() {
        return;
    }
    let files = std::env::var("RATIO_FILES").unwrap_or_default();
    let mut ws = Box::new(PageWorkspace::new());
    for path in files.split(':').filter(|p| !p.is_empty()) {
        let Ok(data) = std::fs::read(path) else { continue };
        let (mut ours, mut z1, mut z3, mut raw) = (0usize, 0usize, 0usize, 0usize);
        for page in data.chunks_exact(4096).step_by(7).take(150) {
            let mut frame = vec![0u8; PageWorkspace::max_compressed_len(4096)];
            let n = ws.compress(page, &mut frame, false).unwrap();
            ours += n.min(4096);
            raw += 4096;
            for (level, total) in [("-1", &mut z1), ("-3", &mut z3)] {
                let mut c = Command::new("zstd").args([level, "-q", "-c", "--no-check"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
                let mut stdin = c.stdin.take().unwrap();
                let p = page.to_vec();
                let t = std::thread::spawn(move || {
                    let _ = stdin.write_all(&p);
                });
                *total += c.wait_with_output().unwrap().stdout.len().min(4096);
                t.join().unwrap();
            }
        }
        if raw > 0 {
            println!("{path}: raw {raw}  ours {ours} ({}%)  zstd-1 {z1} ({}%)  zstd-3 {z3} ({}%)", ours * 100 / raw, z1 * 100 / raw, z3 * 100 / raw);
        }
    }
}

/// `CORPUS_FILES=a:b cargo test --release -p metal-zstd --test encoder corpus_round_trip -- --ignored --nocapture`:
/// every 4 KiB page of real files must survive compress + lite decode (and the full decoder).
#[test]
#[ignore]
fn corpus_round_trip() {
    let files = std::env::var("CORPUS_FILES").unwrap_or_default();
    let mut ws = Box::new(PageWorkspace::new());
    let (mut pages, mut huff, mut fitted) = (0usize, 0usize, 0usize);
    for path in files.split(':').filter(|p| !p.is_empty()) {
        let Ok(data) = std::fs::read(path) else { continue };
        for (i, page) in data.chunks(4096).enumerate() {
            let mut page4 = [0u8; 4096];
            page4[..page.len()].copy_from_slice(page);
            let mut frame = vec![0u8; PageWorkspace::max_compressed_len(4096)];
            let n = ws.compress(&page4, &mut frame, false).unwrap();
            frame.truncate(n);
            let mut out = vec![0u8; 4096];
            assert_eq!(lite::decompress(&frame, &mut out), Ok(4096), "{path} page {i}");
            assert!(out == page4, "{path} page {i}: lite decoded different bytes");
            let mut out2 = vec![0u8; 4096];
            assert_eq!(decompress(&frame, &mut out2), Ok(4096), "{path} page {i} (full)");
            assert!(out2 == page4, "{path} page {i}: full decoder decoded different bytes");
            if let Some((ty, m)) = first_block(&frame) {
                huff += (ty == 2) as usize;
                fitted += (m != 0xff && m & 0xfc != 0) as usize;
            }
            pages += 1;
        }
    }
    println!("{pages} pages round-tripped ({huff} with Huffman literals, {fitted} with fitted tables)");
}
