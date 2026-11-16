extern crate std;
use super::*;
use std::vec::Vec;

fn elf(segs: &[(u32, u32, u64, u64, u64, u64)], entry: u64) -> Vec<u8> {
    // (type, flags, offset, vaddr, filesz, memsz)
    let mut b = std::vec![0u8; 64 + 56 * segs.len().max(1)];
    b[0..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
    b[4] = 2;
    b[5] = 1;
    b[6] = 1;
    b[16..18].copy_from_slice(&2u16.to_le_bytes());
    b[18..20].copy_from_slice(&183u16.to_le_bytes());
    b[24..32].copy_from_slice(&entry.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
    for (i, s) in segs.iter().enumerate() {
        let o = 64 + 56 * i;
        b[o..o + 4].copy_from_slice(&s.0.to_le_bytes());
        b[o + 4..o + 8].copy_from_slice(&s.1.to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&s.2.to_le_bytes());
        b[o + 16..o + 24].copy_from_slice(&s.3.to_le_bytes());
        b[o + 32..o + 40].copy_from_slice(&s.4.to_le_bytes());
        b[o + 40..o + 48].copy_from_slice(&s.5.to_le_bytes());
    }
    b.resize(0x3000, 0);
    b
}

const B: u64 = USER_BASE as u64;

fn flat(code_end: u64, mem: u64, len: usize) -> Vec<u8> {
    let mut b = std::vec![0u8; len];
    b[0..8].copy_from_slice(&0x4D4F_5355u64.to_le_bytes());
    b[8..16].copy_from_slice(&code_end.to_le_bytes());
    b[16..24].copy_from_slice(&mem.to_le_bytes());
    b[24..30].copy_from_slice(b"hello\0");
    b
}

#[test]
fn flat_image() {
    let p = plan(&flat(4096, 16384, 4200)).unwrap();
    assert_eq!(p.entry, USER_BASE + 0x20);
    assert_eq!(&p.name[..5], b"hello");
    let pages: Vec<_> = p.pages(&flat(4096, 16384, 4200)).map(|(va, _, f)| (va, f)).collect();
    assert_eq!(pages, std::vec![(USER_BASE, X | R), (USER_BASE + 4096, W | R), (USER_BASE + 8192, W | R), (USER_BASE + 12288, W | R)]);
    for (code_end, mem, len) in [(0, 4096, 64), (4096, 4096, 4096), (4096, 100, 64), (8192, 4096, 64), (4096, 1 << 30, 64), (4097, 8192, 64), (4096, 8192, 9000)] {
        if (code_end, mem) == (4096, 4096) {
            continue; // valid: no data part
        }
        assert!(plan(&flat(code_end, mem, len)).is_err(), "{code_end} {mem} {len}");
    }
    assert!(plan(&flat(4096, 8192, 31)).is_err());
}

#[test]
fn elf_basic_and_shared_pages() {
    let e = elf(&[(1, 5, 0, B, 0x200, 0x200), (1, 6, 0x1000, B + 0x1000, 0x100, 0x2000)], B + 0x100);
    let p = plan(&e).unwrap();
    assert_eq!(p.entry, USER_BASE + 0x100);
    let pg: Vec<_> = p.pages(&e).map(|(v, _, f)| (v - USER_BASE, f)).collect();
    assert_eq!(pg, std::vec![(0, X | R), (0x1000, W | R), (0x2000, W | R)]);
    // read-only data sharing the page of the text: the page keeps execute, gains nothing writable
    let e = elf(&[(1, 5, 0, B, 0x200, 0x200), (1, 4, 0x200, B + 0x200, 0x100, 0x100)], B);
    assert_eq!(plan(&e).unwrap().pages(&e).map(|(_, _, f)| f).collect::<Vec<_>>(), std::vec![X | R]);
    // file bytes of both segments land in the shared page
    let mut e = elf(&[(1, 5, 0x1000, B, 4, 4), (1, 4, 0x1004, B + 4, 4, 4)], B);
    e[0x1000..0x1008].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let (_, page, _) = plan(&e).unwrap().pages(&e).next().unwrap();
    assert_eq!(&page[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(page[8..].iter().all(|&b| b == 0));
}

#[test]
fn elf_refusals() {
    let ok = |segs: &[(u32, u32, u64, u64, u64, u64)], entry: u64| plan(&elf(segs, entry)).is_ok();
    assert!(ok(&[(1, 5, 0, B, 16, 16)], B));
    assert!(!ok(&[(1, 7, 0, B, 16, 16)], B), "writable and executable");
    assert!(!ok(&[(1, 5, 0, B, 16, 16), (1, 6, 0x100, B + 0x100, 16, 16)], B), "w+x by sharing a page");
    assert!(!ok(&[(1, 5, 0, B, 16, 16)], B + 0x2000), "entry outside");
    assert!(!ok(&[(1, 5, 0, B, 16, 16)], B + 2), "entry misaligned");
    assert!(!ok(&[(1, 6, 0, B, 16, 16)], B), "entry in data");
    assert!(!ok(&[(1, 5, 0, B - 0x1000, 16, 16)], B), "below base");
    assert!(!ok(&[(1, 5, 0, B + MAX_IMAGE as u64, 16, 16)], B), "above");
    assert!(!ok(&[(1, 5, 0, B, 16, 1 << 40)], B), "huge");
    assert!(!ok(&[(1, 5, 0, u64::MAX - 8, 16, 16)], B), "wraps");
    assert!(!ok(&[(1, 5, 0, B, 32, 16)], B), "filesz>memsz");
    assert!(!ok(&[(1, 5, 1 << 40, B, 16, 16)], B), "file offset");
    assert!(!ok(&[(1, 5, 0x2ff8, B, 16, 16)], B), "file bytes past the end");
    assert!(!ok(&[(3, 4, 0, 0, 0, 0), (1, 5, 0, B, 16, 16)], B), "interp");
    assert!(!ok(&[(2, 4, 0, 0, 0, 0), (1, 5, 0, B, 16, 16)], B), "dynamic");
    assert!(!ok(&[(1, 5, 0, B, 0, 0)], B), "nothing to load");
    assert!(!ok(&[], B));
    let mut e = elf(&[(1, 5, 0, B, 16, 16)], B);
    e[16] = 3; // ET_DYN (PIE)
    assert!(plan(&e).is_err());
    let mut e = elf(&[(1, 5, 0, B, 16, 16)], B);
    e[18] = 62; // x86-64
    assert!(plan(&e).is_err());
    let mut e = elf(&[(1, 5, 0, B, 16, 16)], B);
    e[4] = 1; // 32-bit
    assert!(plan(&e).is_err());
    let mut e = elf(&[(1, 5, 0, B, 16, 16)], B);
    let n = e.len() as u64 - 8;
    e[32..40].copy_from_slice(&n.to_le_bytes()); // program headers past the end
    assert!(plan(&e).is_err());
    let many: Vec<_> = (0..17).map(|i| (1u32, 4u32, 0u64, B + 0x1000 * i, 16u64, 16u64)).collect();
    assert!(!ok(&many, B));
    assert!(plan(&elf(&[(1, 5, 0, B, 16, 16)], B)[..40]).is_err(), "truncated header");
}

/// A position-independent image: text at 0, data at 0x1000 with two relative relocations (and the dynamic section and the
/// table in the data segment's file bytes).
fn pie(relocs: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut e = elf(&[(1, 5, 0, 0, 0x200, 0x200), (1, 6, 0x1000, 0x1000, 0x200, 0x2000), (2, 6, 0x1040, 0x1040, 0x40, 0x40)], 0x100);
    e[16] = 3;
    let dynamic = [(7u64, 0x1080u64), (8, 24 * relocs.len() as u64), (9, 24), (0, 0)];
    for (i, (t, v)) in dynamic.iter().enumerate() {
        e[0x1040 + 16 * i..0x1048 + 16 * i].copy_from_slice(&t.to_le_bytes());
        e[0x1048 + 16 * i..0x1050 + 16 * i].copy_from_slice(&v.to_le_bytes());
    }
    for (i, (at, info, add)) in relocs.iter().enumerate() {
        let o = 0x1080 + 24 * i;
        e[o..o + 8].copy_from_slice(&at.to_le_bytes());
        e[o + 8..o + 16].copy_from_slice(&info.to_le_bytes());
        e[o + 16..o + 24].copy_from_slice(&add.to_le_bytes());
    }
    e
}

#[test]
fn position_independent_images() {
    let base = 0x12_3456_7000usize;
    let e = pie(&[(0x1000, 1027, 0x100), (0x10f8, 1027, 0x1234)]);
    let mut p = plan(&e).unwrap();
    assert!(p.is_pie());
    p.rebase(base);
    assert_eq!(p.entry_addr(), base + 0x100);
    let pg: Vec<_> = p.pages(&e).collect();
    assert_eq!(pg.iter().map(|(v, _, f)| (*v - base, *f)).collect::<Vec<_>>(), std::vec![(0, X | R), (0x1000, W | R), (0x2000, W | R)]);
    // the relocated words hold base + addend; the rest of the file bytes are untouched
    assert_eq!(&pg[1].1[..8], &((base + 0x100) as u64).to_le_bytes());
    assert_eq!(&e[0x1000..0x1008], &[0u8; 8], "the image itself is not changed");
    assert_eq!(&pg[1].1[0x40..0x48], &e[0x1040..0x1048]);
    assert_eq!(&pg[1].1[0xf8..0x100], &((base + 0x1234) as u64).to_le_bytes());
    let bad = |r: &[(u64, u64, u64)]| plan(&pie(r)).is_err();
    assert!(bad(&[(0x1ff8, 1027, 1)]), "target not in the file bytes");
    assert!(bad(&[(0x1004, 1027, 1)]), "misaligned target");
    assert!(bad(&[(0x1000, 257, 1)]), "absolute relocation");
    assert!(bad(&[(0x1000, 1027 | 5 << 32, 1)]), "symbol relocation");
    assert!(bad(&[(0x3000, 1027, 1)]), "target outside");
    assert!(plan(&pie(&[])).is_ok(), "no relocations");
    // a table the file does not hold
    let mut e = pie(&[(0x1000, 1027, 0x100)]);
    e[0x1048 + 16..0x1050 + 16].copy_from_slice(&(24u64 * 4000).to_le_bytes());
    assert!(plan(&e).is_err());
    // DT_NEEDED
    let mut e = pie(&[]);
    e[0x1040 + 16..0x1048 + 16].copy_from_slice(&1u64.to_le_bytes()); // the tag of entry 1: DT_NEEDED
    assert!(plan(&e).is_err());
    // relocations out of order are refused
    assert!(bad(&[(0x1008, 1027, 1), (0x1000, 1027, 1)]), "unsorted table");
    // a bigger image than a fixed one may be
    let big = elf(&[(1, 5, 0, 0, 0x200, 0x200), (1, 6, 0x1000, 0x1000, 0x100, (4 << 20) + 0x1000)], 0x100);
    let mut big = big;
    big[16] = 3;
    assert!(plan(&big).is_ok());
    big[16] = 2;
    assert!(plan(&big).is_err());
    // a fixed executable may not carry a dynamic section
    let mut e = pie(&[]);
    e[16] = 2;
    assert!(plan(&e).is_err());
}

/// Mutate valid images at random: whatever comes back is either refused or consistent.
#[test]
fn random_images_never_break_the_invariants() {
    let mut s = 0x1234_5678_9abc_def1u64;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let seeds = [
        elf(&[(1, 5, 0, B, 0x200, 0x200), (1, 6, 0x1000, B + 0x1000, 0x100, 0x2000)], B + 0x100),
        elf(&[(1, 5, 0, B, 0x200, 0x200)], B),
        flat(4096, 16384, 5000),
    ];
    let mut accepted = 0;
    for i in 0..300_000 {
        let mut img = seeds[i % seeds.len()].clone();
        for _ in 0..1 + next() % 6 {
            let at = (next() % img.len().min(64 + 56 * 3 + 8) as u64) as usize;
            img[at] = next() as u8;
        }
        if next() % 8 == 0 {
            let n = (next() % img.len() as u64) as usize;
            img.truncate(n);
        }
        let Ok(p) = plan(&img) else { continue };
        accepted += 1;
        let mut seen_entry = false;
        for (va, page, f) in p.pages(&img) {
            assert!(va >= USER_BASE && va + PAGE <= USER_BASE + MAX_IMAGE && va % PAGE == 0);
            assert!(!(f & X != 0 && f & W != 0));
            if (va..va + PAGE).contains(&p.entry) {
                assert!(f & X != 0);
                seen_entry = true;
            }
            let _ = page;
        }
        assert!(seen_entry);
    }
    assert!(accepted > 1000, "the mutations should leave plenty of valid images ({accepted})");
}

#[test]
fn spawn_drives_the_three_calls_in_order() {
    use std::cell::RefCell;
    std::thread_local!(static LOG: RefCell<Vec<(u64, u64)>> = RefCell::new(Vec::new()));
    fn sys(id: u64, a1: u64, a2: u64, _: u64, _: u64) -> u64 {
        LOG.with(|l| l.borrow_mut().push((id, a1)));
        match id {
            SPAWN => 30,
            TASK_LOAD => {
                let lr = unsafe { core::slice::from_raw_parts(a2 as *const u8, 24) };
                assert_eq!(u32::from_le_bytes(lr[16..20].try_into().unwrap()), 1);
                0
            }
            _ => 0,
        }
    }
    let img = flat(4096, 8192, 100);
    let mut rq = [0u8; REQ_SIZE];
    rq[8] = 77; // the budget is the caller's and stays
    rq[12] = 78; // stale image fields are cleared
    assert_eq!(spawn(sys, 26, &mut rq, &img), 30);
    assert_eq!(&rq[56..61], b"hello");
    assert_eq!((rq[8], rq[12]), (77, 0));
    let log = LOG.with(|l| l.borrow().iter().map(|e| e.0).collect::<Vec<_>>());
    assert_eq!(log, std::vec![SPAWN, TASK_LOAD, TASK_LOAD, TASK_START]);
    // a load that fails condemns and collects the embryo
    LOG.with(|l| l.borrow_mut().clear());
    fn bad(id: u64, _: u64, _: u64, _: u64, _: u64) -> u64 {
        LOG.with(|l| l.borrow_mut().push((id, 0)));
        match id {
            SPAWN => 31,
            TASK_LOAD => (-3i64) as u64,
            _ => 0,
        }
    }
    assert_eq!(spawn(bad, 26, &mut [0u8; REQ_SIZE], &img), -3);
    let log = LOG.with(|l| l.borrow().iter().map(|e| e.0).collect::<Vec<_>>());
    assert_eq!(log, std::vec![SPAWN, TASK_LOAD, KILL, WAIT]);
    assert_eq!(spawn(bad, 26, &mut [0u8; REQ_SIZE], &[0u8; 100]), E_INVAL);
}
