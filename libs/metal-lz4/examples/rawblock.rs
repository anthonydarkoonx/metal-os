// usage: rawblock c|d <in> <out> [orig_len]   (raw LZ4 blocks, used to cross-check against the lz4 CLI)
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let input = std::fs::read(&a[2]).unwrap();
    let out = if a[1] == "c" {
        let mut d = vec![0u8; metal_lz4::compress_bound(input.len())];
        let n = metal_lz4::compress(&input, &mut d).unwrap();
        d.truncate(n);
        d
    } else {
        let mut d = vec![0u8; a[4].parse().unwrap()];
        let n = metal_lz4::decompress(&input, &mut d).unwrap();
        d.truncate(n);
        d
    };
    std::fs::write(&a[3], out).unwrap();
}
