// usage: zcheck <image> <path-in-image> <original-file>  — find the first offset where reads fail or differ
use metallium_erofs::Erofs;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let img = std::fs::read(&a[1]).unwrap();
    let want = std::fs::read(&a[3]).unwrap();
    let fs = Erofs::new(img.as_slice()).unwrap();
    let ino = fs.lookup(&a[2]).unwrap();
    for off in (0..want.len()).step_by(512) {
        let mut b = [0u8; 16];
        match fs.read(&ino, off as u64, &mut b) {
            Ok(n) if b[..n] == want[off..off + n] => {}
            r => {
                println!("first bad offset {off}: {r:?}");
                return;
            }
        }
    }
    println!("all ok");
}
