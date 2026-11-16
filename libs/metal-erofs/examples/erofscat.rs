// usage: erofscat <image> [path]   — list a directory or dump a file (host tool / cross-check)
use metal_erofs::Erofs;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let img = std::fs::read(&a[1]).unwrap();
    let fs = Erofs::new(img.as_slice()).expect("not an EROFS image");
    let path = a.get(2).map(String::as_str).unwrap_or("/");
    let ino = fs.lookup(path).expect("lookup failed");
    if ino.is_dir() {
        fs.read_dir(&ino, |n, nid, t| {
            println!("{:>4} type{} {}", nid, t, String::from_utf8_lossy(n));
            true
        })
        .unwrap();
    } else {
        let mut buf = vec![0u8; ino.size as usize];
        fs.read(&ino, 0, &mut buf).unwrap();
        use std::io::Write;
        std::io::stdout().write_all(&buf).unwrap();
    }
}
