//! Print what `metallium_map` makes of a .map file (the independent check of tools/mkmap.py, see `make map-check`).
fn main() {
    let path = std::env::args().nth(1).expect("usage: mapinfo FILE.map");
    let bytes = std::fs::read(&path).unwrap();
    match metallium_map::Package::parse(&bytes) {
        Ok(p) => {
            let m = &p.manifest;
            println!("ok id={} version={} unsigned={} heap_pages={} abilities={} files={}", m.id, m.version, m.unsigned, m.heap_pages, m.abilities.len(), p.files().len());
        }
        Err(e) => println!("refused {e:?}"),
    }
}
