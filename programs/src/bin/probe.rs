#![no_std]
#![no_main]
use metal_programs::ability::*;
use metal_programs::*;

static SLEEPER: &[u8] = include_bytes!("../../build/sleeper.bin");

// Sample program that checks what rights it really has and reports the answer with `save` (word 0; word 1 is 0xFEED once it has answered), then ends cleanly:
//   bit 0: it could open a file          (only with `permission = files`)
//   bit 1: the program endpoint refused a control request (START) that only the control endpoint accepts
//   bit 2: it has no spawn capability    (starting a program through slot 6 failed)
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
        if spawn(6, &Spawn::new(SLEEPER)) < 0 {
            code |= 4;
        }
        save(code, 0xFEED);
        exit(0);
    }
}

ability!("probe", |_c| Probe);
