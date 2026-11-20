// usage: zread <image> <path> <offset> <len> <original>  — one read, compared with the original
use metallium_erofs::Erofs;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let img = std::fs::read(&a[1]).unwrap();
    let want = std::fs::read(&a[5]).unwrap();
    let (off, len): (usize, usize) = (a[3].parse().unwrap(), a[4].parse().unwrap());
    let fs = Erofs::new(img.as_slice()).unwrap();
    let ino = fs.lookup(&a[2]).unwrap();
    let mut b = vec![0u8; len];
    match fs.read(&ino, off as u64, &mut b) {
        Ok(n) => println!("read {n}, matches: {}", b[..n] == want[off..off + n]),
        Err(e) => println!("error {e:?}"),
    }
}
