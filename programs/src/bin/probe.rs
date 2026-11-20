#![no_std]
#![no_main]
use metallium_programs::ability::*;
use metallium_programs::*;

static SLEEPER: &[u8] = include_bytes!("../../build/sleeper.bin");

// Sample program that checks what rights it really has and reports the answer with `save` (word 0; word 1 is 0xFEED once it has answered), then ends cleanly:
//   bit 0: it could open a file          (only with `permission = files`)
//   bit 1: the program endpoint refused a control request (START) that only the control endpoint accepts
//   bit 2: it has no spawn capability    (starting a program through slot 6 failed; only `permission = spawn` gives one)
//   bit 3: it may not start a program with file access (only `permission = spawn` together with `allfiles` lets it)
struct Probe;

impl Ability for Probe {
    fn on_create(&mut self, _r: Option<(u64, u64)>) {
        let mut code = 0u64;
        if fs::read("/system/hello.txt").is_ok() {
            code |= 1;
        }
        // START "x" through the program endpoint: the manager must answer E_REFUSED (4)
        if matches!(call(PROGRAM_EP_SLOT, [3, 0, 0]), (0, r) if r[0] == 4) {
            code |= 2;
        }
        // (small children: they cost this program's budget, `spawn_pages`; they end when it does)
        if spawn(6, &Spawn { heap_pages: 8, ..Spawn::new(SLEEPER) }) < 0 {
            code |= 4;
        }
        if spawn(6, &Spawn { heap_pages: 8, fs: true, ..Spawn::new(SLEEPER) }) < 0 {
            code |= 8;
        }
        save(code, 0xFEED);
        exit(0);
    }
}

ability!("probe", |_c| Probe);
