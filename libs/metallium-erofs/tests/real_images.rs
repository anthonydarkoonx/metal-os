//! Cross-check against images produced by the reference `mkfs.erofs`.
//! Skipped (with a note) when the tool is not installed.
use metallium_erofs::{Erofs, Error};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn noise(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn tool(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok()
}

fn make_tree(root: &Path) {
    let mut r = Rng(1);
    let w = |p: &str, d: &[u8]| {
        let p = root.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, d).unwrap();
    };
    w("zeros.bin", &vec![0u8; 300_000]);
    w("noise.bin", &r.noise(100_000));
    w("text.txt", "the quick brown fox jumps over the lazy dog\n".repeat(5000).as_bytes());
    // alternate compressible / incompressible 4 KiB blocks, plus irregular run lengths
    let mut mixed = Vec::new();
    for i in 0..100 {
        match i % 4 {
            0 => mixed.extend(r.noise(4096)),
            1 => mixed.extend(vec![i as u8; 4096]),
            2 => mixed.extend("abcdefgh".repeat(512).into_bytes()),
            _ => {
                mixed.extend(r.noise(700));
                mixed.extend(vec![7u8; 3396]);
            }
        }
    }
    w("deep/mixed/mixed.bin", &mixed);
    for n in [0usize, 1, 100, 4095, 4096, 4097, 8191, 8192, 12345] {
        let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        w(&format!("sizes/s{n}"), &data);
        w(&format!("sizes/n{n}"), &Rng(n as u64).noise(n));
    }
    for i in 0..150 {
        w(&format!("many/f{i:03}"), format!("file number {i}\n").repeat(i % 40 + 1).as_bytes());
    }
}

/// Everything must either match the source byte for byte, or fail with `Unsupported`.
fn compare(fs_: &Erofs<&[u8]>, nid_path: &str, src: &Path, strict: bool, stats: &mut (usize, usize)) {
    let ino = fs_.lookup(nid_path).unwrap_or_else(|e| panic!("lookup {nid_path}: {e:?}"));
    let meta = fs::metadata(src).unwrap();
    if meta.is_dir() {
        assert!(ino.is_dir(), "{nid_path}");
        let mut got = Vec::new();
        fs_.read_dir(&ino, |n, _, _| {
            if n != b"." && n != b".." {
                got.push(String::from_utf8(n.to_vec()).unwrap());
            }
            true
        })
        .unwrap();
        got.sort();
        let mut want: Vec<String> = fs::read_dir(src).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        want.sort();
        assert_eq!(got, want, "directory {nid_path}");
        for n in want {
            let child = format!("{}/{}", nid_path.trim_end_matches('/'), n);
            compare(fs_, &child, &src.join(&n), strict, stats);
        }
        return;
    }
    let want = fs::read(src).unwrap();
    assert_eq!(ino.size as usize, want.len(), "{nid_path}: size");
    let mut got = vec![0u8; want.len()];
    match fs_.read(&ino, 0, &mut got) {
        Ok(n) => {
            assert_eq!(n, want.len(), "{nid_path}: short read");
            assert!(got == want, "{nid_path}: content differs");
            // random-access reads exercise the extent cache and mid-extent starts
            let mut r = Rng(want.len() as u64 + 9);
            for _ in 0..20 {
                if want.is_empty() {
                    break;
                }
                let off = r.next() as usize % want.len();
                let len = (r.next() as usize % 9000).min(want.len() - off);
                let mut b = vec![0u8; len];
                let got = fs_.read(&ino, off as u64, &mut b).unwrap_or_else(|e| panic!("{nid_path}@{off}+{len} (size {}): {e:?}", want.len()));
                assert_eq!(got, len, "{nid_path}@{off}");
                assert!(b == want[off..off + len], "{nid_path}: random read @{off}+{len}");
            }
            stats.0 += 1;
        }
        Err(Error::Unsupported) if !strict => stats.1 += 1,
        Err(e) => panic!("{nid_path}: {e:?}"),
    }
}

fn run_mode(name: &str, args: &[&str], strict: bool) {
    run_mode_with(name, args, strict, None);
}

/// Algorithm-slot test image: rewrite every compressed (legacy full-index) file of a ZSTD image so
/// its z-map header says "HEAD1 = LZ4, HEAD2 = ZSTD" and all HEAD1 index entries become HEAD2.
/// (The reference mkfs.erofs here crashes whenever --compress-hints is used, so a real mixed
/// image cannot be produced; this follows the on-disk format description instead.)
fn move_zstd_to_head2(img: &mut Vec<u8>) {
    let (files, meta, bs) = {
        let efs = Erofs::new(img.as_slice()).unwrap();
        let mut stack = vec![efs.root().unwrap()];
        let mut files = Vec::new();
        while let Some(d) = stack.pop() {
            let mut kids = Vec::new();
            efs.read_dir(&d, |n, nid, _| {
                if n != b"." && n != b".." {
                    kids.push(nid);
                }
                true
            })
            .unwrap();
            for nid in kids {
                let i = efs.inode(nid).unwrap();
                if i.is_dir() { stack.push(i) } else { files.push(nid) }
            }
        }
        (files, u32::from_le_bytes(img[1024 + 40..1024 + 44].try_into().unwrap()) as usize, 1usize << img[1024 + 12])
    };
    let mut patched = 0;
    for nid in files {
        let off = meta * bs + nid as usize * 32;
        let fmt = u16::from_le_bytes([img[off], img[off + 1]]);
        if (fmt >> 1) & 7 != 1 {
            continue; // not a legacy-index compressed file
        }
        let xic = u16::from_le_bytes([img[off + 2], img[off + 3]]) as usize;
        let (isz, size) = if fmt & 1 == 1 {
            (64, u64::from_le_bytes(img[off + 8..off + 16].try_into().unwrap()) as usize)
        } else {
            (32, u32::from_le_bytes(img[off + 8..off + 12].try_into().unwrap()) as usize)
        };
        let z = (off + isz + if xic > 0 { 12 + 4 * (xic - 1) } else { 0 } + 7) & !7;
        assert_eq!(img[z + 6] & 15, 3, "expected a ZSTD head-1 algorithm");
        img[z + 6] = 0x30; // HEAD1 = LZ4 (0), HEAD2 = ZSTD (3)
        for i in 0..size.div_ceil(bs) {
            let e = z + 16 + 8 * i;
            if img[e] & 3 == 1 {
                img[e] = (img[e] & !3) | 3; // HEAD1 -> HEAD2
            }
        }
        patched += 1;
    }
    assert!(patched >= 5, "only {patched} files patched");
}

/// `patch`: optional rewrite of the finished image before it is read back.
fn run_mode_with(name: &str, args: &[&str], strict: bool, patch: Option<fn(&mut Vec<u8>)>) {
    if !tool("mkfs.erofs") {
        eprintln!("mkfs.erofs not installed: skipping {name}");
        return;
    }
    let base: PathBuf = std::env::temp_dir().join(format!("metallium-erofs-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let tree = base.join("tree");
    make_tree(&tree);
    let img = base.join("img.erofs");
    let out = Command::new("mkfs.erofs").args(args).arg(&img).arg(&tree).output().unwrap();
    assert!(out.status.success(), "mkfs.erofs {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    let mut data = fs::read(&img).unwrap();
    if let Ok(keep) = std::env::var("METALLIUM_KEEP") {
        // debugging aid: leave the image and its source tree behind
        let dst = PathBuf::from(keep).join(name);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(&dst).unwrap();
        fs::copy(&img, dst.join("img.erofs")).unwrap();
        Command::new("cp").arg("-r").arg(&tree).arg(dst.join("tree")).status().unwrap();
    }
    if let Some(p) = patch {
        p(&mut data);
    }
    let efs = Erofs::new(data.as_slice()).unwrap();
    let mut stats = (0, 0);
    compare(&efs, "/", &tree, strict, &mut stats);
    eprintln!("{name}: {} files verified byte-for-byte, {} reported Unsupported (image {} KiB)", stats.0, stats.1, data.len() / 1024);
    if strict {
        assert_eq!(stats.1, 0);
    }
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn uncompressed() {
    run_mode("plain", &[], true);
}
#[test]
fn lz4_compact_indexes() {
    run_mode("lz4", &["-zlz4"], true);
}
#[test]
fn lz4hc_compact_indexes() {
    run_mode("lz4hc", &["-zlz4hc,12"], true);
}
#[test]
fn lz4_legacy_full_indexes() {
    run_mode("legacy", &["-zlz4hc", "-E", "legacy-compress"], true);
}
#[test]
fn lz4_small_blocks() {
    run_mode("blk1024", &["-zlz4", "-b", "1024"], false);
}
#[test]
fn big_physical_clusters() {
    run_mode("big-lz4", &["-zlz4hc", "-C16384"], true);
    run_mode("big-lz4-64k", &["-zlz4hc", "-C65536"], true);
    run_mode("big-zstd", &["-zzstd", "-C16384"], true);
    run_mode("big-zstd-19", &["-zzstd,level=19", "-C65536"], true);
}
#[test]
fn unsupported_features_never_return_wrong_data() {
    run_mode("ztail", &["-zlz4", "-E", "ztailpacking"], false);
    run_mode("ztail-zstd", &["-zzstd", "-E", "ztailpacking"], false);
    run_mode("fragments", &["-zlz4", "-E", "fragments"], false);
}
#[test]
fn zstd_compact_indexes() {
    run_mode("zstd", &["-zzstd"], true);
}
#[test]
fn zstd_high_level_and_legacy_indexes() {
    run_mode("zstd19", &["-zzstd,level=19"], true);
    run_mode("zstd-legacy", &["-zzstd", "-E", "legacy-compress"], true);
}
#[test]
fn zstd_small_blocks() {
    run_mode("zstd1024", &["-zzstd", "-b", "1024"], false);
}
#[test]
fn second_algorithm_slot_head2_synthetic() {
    run_mode_with("head2", &["-zzstd", "-E", "legacy-compress"], true, Some(move_zstd_to_head2));
}
