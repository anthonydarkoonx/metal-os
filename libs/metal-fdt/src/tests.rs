use super::*;
use std::{vec, vec::Vec};

static V2: &[u8] = include_bytes!("../tests-data/virt-gicv2.dtb"); // qemu virt, gic-version=2, -smp 4, -m 128M
static V3: &[u8] = include_bytes!("../tests-data/virt-gicv3.dtb"); // qemu virt, gic-version=3, -smp 16, -m 512M

#[test]
fn qemu_gicv2_machine() {
    let m = machine(&Fdt::new(V2).unwrap());
    assert_eq!(m.ram, Some((0x4000_0000, 128 << 20)));
    assert_eq!(m.ncpus, 4);
    assert_eq!(&m.cpus[..4], &[0, 1, 2, 3]);
    assert_eq!(&m.capacity[..4], &[0; 4]); // (QEMU's cores are alike and say nothing)
    assert_eq!(m.gic, Some(Gic::V2 { dist: 0x0800_0000, cpu: 0x0801_0000 }));
    assert_eq!(m.conduit, Some(Conduit::Hvc));
    assert_eq!(m.uart, Some(0x0900_0000));
}

#[test]
fn qemu_gicv3_machine() {
    let m = machine(&Fdt::new(V3).unwrap());
    assert_eq!(m.ram, Some((0x4000_0000, 512 << 20)));
    assert_eq!(m.ncpus, 16);
    assert!(m.cpus[..16].iter().enumerate().all(|(i, &c)| c == i as u64));
    match m.gic {
        Some(Gic::V3 { dist, redist, redist_size }) => {
            assert_eq!(dist, 0x0800_0000);
            assert_eq!(redist, 0x080a_0000);
            assert!(redist_size >= 16 * 0x20000);
        }
        g => panic!("{g:?}"),
    }
}

#[test]
fn early_memory_agrees_with_the_walk() {
    for blob in [V2, V3] {
        let m = machine(&Fdt::new(blob).unwrap());
        // SAFETY: `blob` is a live slice and its length is the limit.
        assert_eq!(unsafe { early_memory(blob.as_ptr(), blob.len()) }, m.ram);
    }
}

#[test]
fn early_memory_survives_damage() {
    let mut seed = 99u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for blob in [V2, V3] {
        for _ in 0..4000 {
            let mut b = blob.to_vec();
            for _ in 0..1 + rnd() % 6 {
                let i = rnd() as usize % b.len();
                b[i] = rnd() as u8;
            }
            let cut = b.len() - rnd() as usize % 64;
            // SAFETY: `b` is a live vector and `cut <= b.len()` is the limit.
            let _ = unsafe { early_memory(b.as_ptr(), cut) };
        }
    }
}

#[test]
fn rejects_bad_headers() {
    assert!(Fdt::new(&[]).is_none());
    assert!(Fdt::new(&V2[..39]).is_none());
    let mut b = V2.to_vec();
    b[0] ^= 1;
    assert!(Fdt::new(&b).is_none()); // magic
    let mut b = V2.to_vec();
    b[4..8].copy_from_slice(&((V2.len() + 1) as u32).to_be_bytes());
    assert!(Fdt::new(&b).is_none()); // claims more than it has
    let mut b = V2.to_vec();
    b[8..12].copy_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
    assert!(Fdt::new(&b).is_none()); // structure block outside the blob
}

/// A tree cut off, or with its lengths changed, must never read outside the blob or loop: whatever it says, the walk ends.
#[test]
fn damaged_trees_end_cleanly() {
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for blob in [V2, V3] {
        for _ in 0..4000 {
            let mut b = blob.to_vec();
            for _ in 0..1 + rnd() % 6 {
                let i = 40 + rnd() as usize % (b.len() - 40);
                b[i] = rnd() as u8;
            }
            if let Some(f) = Fdt::new(&b) {
                let _ = machine(&f);
            }
        }
        for cut in (0..blob.len()).step_by(97) {
            if let Some(f) = Fdt::new(&blob[..cut]) {
                let _ = machine(&f);
            }
        }
    }
}

#[test]
fn walk_stops_when_asked() {
    let f = Fdt::new(V2).unwrap();
    let mut n = 0;
    f.walk(|_| {
        n += 1;
        n < 5
    });
    assert_eq!(n, 5);
}

#[test]
fn disabled_cpus_are_skipped_and_cells_are_honoured() {
    // /: #address-cells=2 #size-cells=2; /memory reg = <0 0x80000000 0 0x20000000>; /cpus #address-cells=1 with cpu@0 (okay), cpu@100 (disabled)
    let mut st: Vec<u8> = vec![];
    let mut strs: Vec<u8> = vec![];
    let mut name = |s: &str| -> u32 {
        let o = strs.len() as u32;
        strs.extend_from_slice(s.as_bytes());
        strs.push(0);
        o
    };
    let be = |v: u32| v.to_be_bytes();
    let node = |st: &mut Vec<u8>, n: &str| {
        st.extend_from_slice(&be(1));
        st.extend_from_slice(n.as_bytes());
        st.push(0);
        while st.len() % 4 != 0 {
            st.push(0);
        }
    };
    let prop = |st: &mut Vec<u8>, n: u32, v: &[u8]| {
        st.extend_from_slice(&be(3));
        st.extend_from_slice(&be(v.len() as u32));
        st.extend_from_slice(&be(n));
        st.extend_from_slice(v);
        while st.len() % 4 != 0 {
            st.push(0);
        }
    };
    let (ac, sc, reg, status, cap) = (name("#address-cells"), name("#size-cells"), name("reg"), name("status"), name("capacity-dmips-mhz"));
    node(&mut st, "");
    prop(&mut st, ac, &be(2));
    prop(&mut st, sc, &be(2));
    node(&mut st, "memory@80000000");
    prop(&mut st, reg, &[0, 0, 0, 0, 0x80, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0]);
    st.extend_from_slice(&be(2));
    node(&mut st, "cpus");
    prop(&mut st, ac, &be(1));
    prop(&mut st, sc, &be(0));
    node(&mut st, "cpu@0");
    prop(&mut st, reg, &be(0));
    prop(&mut st, cap, &be(512));
    st.extend_from_slice(&be(2));
    node(&mut st, "cpu@100");
    prop(&mut st, reg, &be(0x100));
    prop(&mut st, status, b"disabled\0");
    st.extend_from_slice(&be(2));
    node(&mut st, "cpu@101");
    prop(&mut st, reg, &be(0x101));
    prop(&mut st, cap, &be(1024));
    st.extend_from_slice(&be(2));
    st.extend_from_slice(&be(2)); // /cpus
    st.extend_from_slice(&be(2)); // /
    st.extend_from_slice(&be(9));
    let off_struct = 40 + 16;
    let off_strings = off_struct + st.len();
    let total = off_strings + strs.len();
    let mut b = vec![];
    for v in [MAGIC, total as u32, off_struct as u32, off_strings as u32, 56, 17, 16, 0, strs.len() as u32, st.len() as u32] {
        b.extend_from_slice(&be(v));
    }
    b.extend_from_slice(&[0; 16]);
    b.extend_from_slice(&st);
    b.extend_from_slice(&strs);
    let m = machine(&Fdt::new(&b).unwrap());
    assert_eq!(m.ram, Some((0x8000_0000, 0x2000_0000)));
    assert_eq!(m.ncpus, 2);
    assert_eq!(&m.cpus[..2], &[0, 0x101]);
    assert_eq!(&m.capacity[..2], &[512, 1024]);
}
