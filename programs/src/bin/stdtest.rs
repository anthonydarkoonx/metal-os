#![no_std]
#![no_main]
extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::{format, vec};
use core::alloc::Layout;
use metal_programs::io::{ErrorKind, Read, Write};
use metal_programs::ipc::{self, Endpoint};
use metal_programs::process::Command;
use metal_programs::sync::mpsc::channel;
use metal_programs::sync::{Arc, Condvar, Mutex, Once, OnceLock, RwLock};
use metal_programs::time::{Duration, Instant, SystemTime};
use metal_programs::*;

// Exercises the std-like layer end to end. Exit code 0 = everything held; otherwise section * 100 + the number of the
// check that did not. Progress goes to standard output, a marker to standard error. Needs: standard input "first\nsecond",
// the spawn capability in slot 6 and a factory in slot 5, file access, a heap of ~1500 pages and 8 threads.

macro_rules! check {
    ($sect:expr, $n:expr, $cond:expr) => {
        if !$cond {
            eprintln!("check {} of section {} failed: {}", $n, $sect, stringify!($cond));
            return $sect * 100 + $n;
        }
    };
}

fn allocation() -> u64 {
    let mut v: Vec<u32> = Vec::new();
    for i in 0..100_000u32 {
        v.push(i.wrapping_mul(2654435761));
    }
    check!(1, 1, v.iter().enumerate().all(|(i, &x)| x == (i as u32).wrapping_mul(2654435761)));
    let mut m = BTreeMap::new();
    for i in 0..2000u32 {
        m.insert(format!("key{i}"), vec![i as u8; (i % 50) as usize]);
    }
    check!(1, 2, m.len() == 2000 && m["key1999"].len() == 49 && m["key7"] == vec![7u8; 7]);
    let big = vec![0xA5u8; 300_000];
    check!(1, 3, big.iter().all(|&b| b == 0xA5));
    let l = Layout::from_size_align(5000, 4096).unwrap();
    let p = unsafe { alloc::alloc::alloc(l) };
    check!(1, 4, !p.is_null() && p as usize % 4096 == 0);
    unsafe { alloc::alloc::dealloc(p, l) };
    let b: Box<dyn Fn(u32) -> u32> = Box::new(|x| x + 1);
    check!(1, 5, b(41) == 42);
    drop((v, m, big));
    // memory comes back and joins up again: a block as big as the three together can be had
    let before = heap::free_bytes();
    let again = vec![1u8; 400_000];
    check!(1, 6, again.len() == 400_000 && before >= 400_000);
    drop(again);
    // many small blocks, freed in a scrambled order, leave one big free run
    let mut small: Vec<Box<[u8; 24]>> = (0..1000).map(|i| Box::new([i as u8; 24])).collect();
    let mut k = 7usize;
    while !small.is_empty() {
        k = (k * 31 + 11) % small.len();
        small.swap_remove(k);
    }
    drop(small);
    check!(1, 7, heap::free_bytes() >= before - 64);
    // ... one run big enough for the 400 000-byte block again, taken without asking the kernel for more
    let f0 = heap::free_bytes();
    let x = vec![0u8; 400_000];
    let f1 = heap::free_bytes();
    check!(1, 8, x.len() == 400_000 && f1 + 400_000 <= f0 + 64);
    0
}

fn standard_streams() -> u64 {
    println!("out: hello {}", 42);
    eprintln!("err: warning {}", 7);
    let mut line = String::new();
    let n = io::stdin().read_line(&mut line).unwrap();
    check!(2, 1, n == 6 && line == "first\n");
    let mut rest = String::new();
    io::stdin().read_to_string(&mut rest).unwrap();
    check!(2, 2, rest == "second");
    check!(2, 3, io::stdin().read(&mut [0u8; 4]).unwrap() == 0);
    writeln_all(b"raw bytes\n");
    0
}

fn writeln_all(b: &[u8]) {
    io::stdout().write_all(b).unwrap();
}

fn files() -> u64 {
    let path = "/data/std_test.txt";
    let _ = fs::remove_file(path);
    check!(3, 1, fs::write(path, b"hello, files\nsecond line\n").is_ok());
    check!(3, 2, fs::read_to_string(path).as_deref() == Ok("hello, files\nsecond line\n"));
    let md = fs::metadata(path).unwrap();
    check!(3, 3, md.len == 25 && md.is_file());
    let mut f = fs::File::open(path).unwrap();
    f.seek(fs::SeekFrom::Start(7)).unwrap();
    let mut b = [0u8; 5];
    f.read_exact(&mut b).unwrap();
    check!(3, 4, &b == b"files");
    check!(3, 5, f.seek(fs::SeekFrom::End(-5)).unwrap() == 20);
    let mut tail = String::new();
    f.read_to_string(&mut tail).unwrap();
    check!(3, 6, tail == "line\n");
    drop(f);
    check!(3, 7, fs::create_dir("/data/stdtest_dir").is_ok());
    check!(3, 8, fs::write("/data/stdtest_dir/inner", b"x").is_ok());
    let names = fs::read_dir("/data/stdtest_dir").unwrap();
    check!(3, 9, names == vec![fs::DirEntry { name: "inner".to_string(), is_dir: false }]);
    let top = fs::read_dir("/data").unwrap();
    check!(3, 10, top.iter().any(|e| e.name == "stdtest_dir" && e.is_dir) && top.iter().any(|e| e.name == "std_test.txt" && !e.is_dir));
    check!(3, 11, fs::remove_dir("/data/stdtest_dir").unwrap_err().kind == ErrorKind::NotEmpty);
    check!(3, 12, fs::remove_file("/data/stdtest_dir/inner").is_ok() && fs::remove_dir("/data/stdtest_dir").is_ok());
    check!(3, 13, fs::File::open("/data/nothing_here").err().map(|e| e.kind) == Some(ErrorKind::NotFound));
    check!(3, 14, fs::write("/system/x", b"no").unwrap_err().kind == ErrorKind::ReadOnly);
    check!(3, 15, fs::remove_file(path).is_ok() && !fs::exists(path));
    // an update that fails to commit leaves the old file: here simply that overwriting replaces the whole content
    fs::write(path, b"one").unwrap();
    fs::write(path, b"2").unwrap();
    check!(3, 16, fs::read(path).unwrap() == b"2");
    let _ = fs::remove_file(path);
    0
}

fn threads() -> u64 {
    let hs: Vec<_> = (0..4u64).map(|i| thread::spawn(move || i * i)).collect();
    check!(4, 1, hs.into_iter().map(|h| h.join()).collect::<Vec<_>>() == vec![0, 1, 4, 9]);
    let data = vec![1u64, 2, 3, 4, 5, 6, 7, 8];
    let mut total = 0;
    let mut sums = [0u64; 2];
    thread::scope(|s| {
        let (a, b) = data.split_at(4);
        let (sa, sb) = sums.split_at_mut(1);
        s.spawn(|| sa[0] = a.iter().sum());
        s.spawn(|| sb[0] = b.iter().sum());
    });
    total += sums[0] + sums[1];
    check!(4, 2, total == 36 && sums == [10, 26]);
    let shared = Arc::new(Mutex::new(Vec::new()));
    let hs: Vec<_> = (0..4)
        .map(|i| {
            let shared = shared.clone();
            thread::spawn(move || {
                for j in 0..100 {
                    shared.lock().push(i * 1000 + j);
                }
            })
        })
        .collect();
    hs.into_iter().for_each(|h| h.join());
    let mut v = shared.lock().clone();
    v.sort();
    check!(4, 3, v.len() == 400 && v[0] == 0 && v[399] == 3099);
    // a condition variable: a consumer waits for a producer
    let pair = Arc::new((Mutex::new(0u32), Condvar::new()));
    let p2 = pair.clone();
    let consumer = thread::spawn(move || {
        let (m, cv) = &*p2;
        let g = cv.wait_while(m.lock(), |n| *n < 5);
        *g
    });
    for _ in 0..5 {
        thread::sleep(Duration::from_millis(10));
        *pair.0.lock() += 1;
        pair.1.notify_all();
    }
    check!(4, 4, consumer.join() == 5);
    let (tx, rx) = channel::<u32>();
    let senders: Vec<_> = (0..3u32)
        .map(|i| {
            let tx = tx.clone();
            thread::spawn(move || {
                for j in 0..100 {
                    tx.send(i * 100 + j).unwrap();
                }
            })
        })
        .collect();
    drop(tx);
    let mut got: Vec<u32> = rx.iter().collect();
    senders.into_iter().for_each(|h| h.join());
    got.sort();
    check!(4, 5, got.len() == 300 && got.iter().enumerate().all(|(i, &x)| x == (i / 100 * 100 + i % 100) as u32));
    check!(4, 6, rx.recv().is_err());
    static ONCE: Once = Once::new();
    static COUNT: Mutex<u32> = Mutex::new(0);
    let hs: Vec<_> = (0..4).map(|_| thread::spawn(|| ONCE.call_once(|| *COUNT.lock() += 1))).collect();
    hs.into_iter().for_each(|h| h.join());
    check!(4, 7, *COUNT.lock() == 1);
    static CELL: OnceLock<String> = OnceLock::new();
    check!(4, 8, CELL.get().is_none() && CELL.get_or_init(|| "set".to_string()) == "set" && CELL.get().is_some());
    let rw = Arc::new(RwLock::new(5u32));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let rw = rw.clone();
            thread::spawn(move || (0..50).map(|_| *rw.read()).sum::<u32>())
        })
        .collect();
    *rw.write() = 5;
    check!(4, 9, readers.into_iter().all(|h| h.join() == 250));
    let (m, cv) = (Mutex::new(false), Condvar::new());
    let t0 = Instant::now();
    let (g, timed_out) = cv.wait_timeout(m.lock(), Duration::from_millis(60));
    drop(g);
    check!(4, 10, timed_out && t0.elapsed() >= Duration::from_millis(50));
    let (tx, rx) = channel::<u8>();
    let r = rx.recv_timeout(Duration::from_millis(30));
    check!(4, 11, r == Err(metal_programs::sync::mpsc::RecvTimeoutError::Timeout));
    drop(tx);
    // a scoped thread that starts another scoped thread: `scope` must wait for the grandchild too, or it ends while the grandchild still
    // uses what the scope lends it (a use after free)
    let (mut flag, mut late) = (0u32, 0u32);
    thread::scope(|s| {
        let (flag, late) = (&mut flag, &mut late);
        s.spawn(move || {
            *flag = 1;
            s.spawn(move || {
                thread::sleep(Duration::from_millis(80)); // (far longer than it takes `scope` to look at its list)
                *late = 7;
            });
        });
    });
    check!(4, 12, flag == 1 && late == 7);
    0
}

fn clocks() -> u64 {
    let a = Instant::now();
    thread::sleep(Duration::from_millis(50));
    let d = a.elapsed();
    check!(5, 1, d >= Duration::from_millis(40) && d < Duration::from_secs(2));
    let b = Instant::now();
    check!(5, 2, b > a && b - a >= d.min(Duration::from_millis(40)));
    let t = SystemTime::now();
    // a machine without a real-time clock has no wall time: that is `None`, not a wrong time
    check!(5, 3, t.map_or(true, |t| t.unix_secs() > 1_700_000_000));
    0
}

fn processes() -> u64 {
    let out = Command::new("hello").arg("world").numeric_arg(0).output().unwrap();
    check!(6, 1, out.status.success() && out.stdout == b"hello world\n");
    let out = Command::new("argdump").args(["a", "b c"]).env("K", "V").numeric_arg(5).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    check!(6, 2, out.status.code() == Some(3) && text.contains("argv[1]=a\n") && text.contains("argv[2]=b c\n") && text.contains("arg=5 "));
    let out = Command::new("upper").output_with_input(b"abc\nxyz").unwrap();
    check!(6, 3, out.status.code() == Some(2) && out.stdout == b"ABC\nXYZ\n");
    check!(6, 4, Command::new("nonexistent_program").output().err().map(|e| e.kind) == Some(ErrorKind::NotFound));
    let out = Command::new("crash").output().unwrap();
    check!(6, 5, out.status.crashed());
    let child = Command::new("sleeper").spawn().unwrap();
    check!(6, 6, child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    check!(6, 7, child.wait().unwrap().killed());
    // the environment is inherited unless cleared
    let out = Command::new("argdump").env_clear().output().unwrap();
    check!(6, 8, !String::from_utf8_lossy(&out.stdout).contains("PWD=/") || env::var("PWD").is_none());
    // a child that shares this program's console
    let c = Command::new("hello").arg("shared").spawn().unwrap();
    check!(6, 9, c.wait().unwrap().success());
    0
}

fn endpoints() -> u64 {
    let ep = Endpoint::create(5).unwrap();
    // a thread serves it: it holds a copy of the receive capability from the moment it was started
    let server = thread::spawn(move || {
        let mut served = 0;
        while let Ok(req) = ep.recv() {
            let [a, b, _] = req.words;
            if req.is_call {
                ipc::reply([a + b, a * b, 99]).unwrap();
            }
            served += 1;
        }
        served
    });
    let mut ok = true;
    for i in 1..=5u64 {
        ok &= ep.call([i, 10, 0]).map_or(false, |r| r == [i + 10, i * 10, 99]);
    }
    check!(7, 1, ok);
    check!(7, 2, ep.send([1, 2, 3]).is_ok());
    thread::sleep(Duration::from_millis(30));
    ep.destroy().unwrap();
    check!(7, 3, server.join() == 6);
    0
}

fn environment() -> u64 {
    let args: Vec<String> = env::args().collect();
    check!(8, 1, args.len() == 2 && args[0] == "stdtest" && args[1] == "one");
    check!(8, 2, env::var("K").as_deref() == Some("V") && env::var("NOPE").is_none());
    check!(8, 3, env::vars().iter().any(|(k, v)| k == "K" && v == "V"));
    check!(8, 4, env::arg() == 77);
    0
}

program!("stdtest", |_c| -> u64 {
    process::set_capabilities(6, 5);
    let sections: [(&str, fn() -> u64); 8] = [
        ("environment", environment),
        ("allocation", allocation),
        ("streams", standard_streams),
        ("files", files),
        ("threads", threads),
        ("clocks", clocks),
        ("processes", processes),
        ("endpoints", endpoints),
    ];
    for (name, f) in sections {
        let r = f();
        if r != 0 {
            return r;
        }
        eprintln!("section {name} ok");
    }
    0
});
