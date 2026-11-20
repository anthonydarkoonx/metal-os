//! Programs starting programs: `Command` builds the request (path, arguments, environment, what the child may hold),
//! `Child` waits for or kills it. The child's standard streams are the parent's by default (it is given the same
//! console endpoint), or captured: `output` runs it to the end and collects what it wrote, serving its input from a
//! buffer. See docs/ABI.md for the console protocol and the request.
use crate::io::{Error, ErrorKind, Result};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// Where this program holds the right to create programs, and the right to create endpoints (the parent decides;
/// `set_capabilities` if they are elsewhere).
static SPAWN_SLOT: AtomicU64 = AtomicU64::new(6);
static FACTORY_SLOT: AtomicU64 = AtomicU64::new(5);

pub fn set_capabilities(spawn_slot: u64, factory_slot: u64) {
    SPAWN_SLOT.store(spawn_slot, Ordering::Relaxed);
    FACTORY_SLOT.store(factory_slot, Ordering::Relaxed);
}

/// End the program with `code`.
pub fn exit(code: u64) -> ! {
    crate::exit(code)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatus(pub u64);

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.0 == 0
    }
    pub fn code(&self) -> Option<u64> {
        (self.0 < u64::MAX - 1).then_some(self.0)
    }
    pub fn crashed(&self) -> bool {
        self.0 == crate::EXIT_FAULTED
    }
    pub fn killed(&self) -> bool {
        self.0 == crate::EXIT_KILLED
    }
}

pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct Command {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    env_clear: bool,
    heap_pages: u32,
    budget_pages: u32,
    threads: u8,
    class: u32,
    arg: u64,
    files: bool,
    net: bool,
    /// (my slot, child slot, rights mask)
    give: Vec<(u8, u8, u16)>,
    inherit_stdio: bool,
    /// The program's code when the caller already has it (a file inside a package), instead of a path to read.
    image: Option<alloc::sync::Arc<Vec<u8>>>,
    /// Confine the child's file access to this directory (see `scope`).
    scope: Option<String>,
}

pub struct Child {
    id: i64,
}

impl Command {
    pub fn new(program: &str) -> Command {
        Command {
            program: String::from(program),
            args: Vec::new(),
            env: Vec::new(),
            env_clear: false,
            heap_pages: 0,
            budget_pages: 0,
            threads: 0,
            class: 0,
            arg: 0,
            files: true,
            net: false,
            give: Vec::new(),
            inherit_stdio: true,
            image: None,
            scope: None,
        }
    }

    pub fn arg(&mut self, a: &str) -> &mut Command {
        self.args.push(String::from(a));
        self
    }

    pub fn args<'a>(&mut self, a: impl IntoIterator<Item = &'a str>) -> &mut Command {
        self.args.extend(a.into_iter().map(String::from));
        self
    }

    pub fn env(&mut self, k: &str, v: &str) -> &mut Command {
        self.env.push((String::from(k), String::from(v)));
        self
    }

    pub fn env_clear(&mut self) -> &mut Command {
        self.env_clear = true;
        self
    }

    /// Heap quota of the child in pages (default: 256).
    pub fn heap_pages(&mut self, n: u32) -> &mut Command {
        self.heap_pages = n;
        self
    }

    /// Pages the child may hand down to programs of its own (default: none). They come out of this program's budget.
    pub fn budget_pages(&mut self, n: u32) -> &mut Command {
        self.budget_pages = n;
        self
    }

    /// How many threads besides itself the child may have.
    pub fn threads(&mut self, n: u8) -> &mut Command {
        self.threads = n;
        self
    }

    /// Scheduling class: 0 interactive, 1 background, 2 heavy.
    pub fn class(&mut self, c: u32) -> &mut Command {
        self.class = c;
        self
    }

    /// The numeric argument the child starts with (`env::arg()`).
    pub fn numeric_arg(&mut self, a: u64) -> &mut Command {
        self.arg = a;
        self
    }

    /// Whether the child gets file-system access (default yes; silently dropped if this program may not hand it out).
    pub fn files(&mut self, yes: bool) -> &mut Command {
        self.files = yes;
        self
    }

    /// Whether the child gets network access (default no; silently dropped if this program may not hand it out).
    pub fn network(&mut self, yes: bool) -> &mut Command {
        self.net = yes;
        self
    }

    /// Give the child a copy of the capability in my slot `mine` as its slot `theirs` (0..7), rights ANDed with `mask`.
    pub fn give(&mut self, mine: u8, theirs: u8, mask: u16) -> &mut Command {
        self.give.push((mine, theirs, mask));
        self
    }

    /// Start this code (an ELF image) instead of reading `program` from a file; `program` still names it for `argv[0]`.
    pub fn image(&mut self, bytes: Vec<u8>) -> &mut Command {
        // (shared, not copied, when the command starts: a large image must not be held twice)
        self.image = Some(alloc::sync::Arc::new(bytes));
        self
    }

    /// Confine the child's file access to the directory `dir` (which must exist): for the child, `/data` is that directory and
    /// nothing outside it can be named. Set before the child runs its first instruction; if it cannot be set the child is not
    /// started. Needs file access for the child (`files(true)`), otherwise it has none to confine.
    pub fn scope(&mut self, dir: &str) -> &mut Command {
        self.scope = Some(String::from(dir));
        self
    }

    fn find_image(&self) -> Result<alloc::sync::Arc<Vec<u8>>> {
        if let Some(i) = &self.image {
            return Ok(i.clone());
        }
        let candidates: Vec<String> = if self.program.contains('/') {
            alloc::vec![self.program.clone()]
        } else {
            alloc::vec![alloc::format!("/system/bin/{}", self.program), alloc::format!("/data/bin/{}", self.program)]
        };
        let mut last = Error::new(ErrorKind::NotFound);
        for c in &candidates {
            match crate::fs::read(c) {
                Ok(b) => return Ok(alloc::sync::Arc::new(b)),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn request(&self, console: Option<u64>, fs: bool, net: bool) -> [u8; 96] {
        let mut b = [0u8; 96];
        b[4..8].copy_from_slice(&((fs as u32) | (net as u32) << 1 | 4 /* the kernel places the child's mappings */ | (self.threads as u32) << 8).to_le_bytes());
        b[8..12].copy_from_slice(&self.budget_pages.to_le_bytes());
        b[24..28].copy_from_slice(&self.class.to_le_bytes());
        b[28..32].copy_from_slice(&self.heap_pages.to_le_bytes());
        b[32..40].copy_from_slice(&self.arg.to_le_bytes());
        for i in 0..8 {
            b[64 + 4 * i] = crate::NONE;
        }
        for &(mine, theirs, mask) in &self.give {
            if (theirs as usize) < 8 {
                b[64 + 4 * theirs as usize] = mine;
                b[66 + 4 * theirs as usize..68 + 4 * theirs as usize].copy_from_slice(&mask.to_le_bytes());
            }
        }
        if let Some(c) = console {
            // (send, and the right to pass it on: the child may start programs that share the console)
            b[64 + 4 * 7] = c as u8;
            b[66 + 4 * 7..68 + 4 * 7].copy_from_slice(&0b101u16.to_le_bytes());
        }
        b
    }

    fn argument_block(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for a in core::iter::once(&self.program).chain(self.args.iter()) {
            v.extend_from_slice(a.as_bytes());
            v.push(0);
        }
        v.push(0);
        let mut put = |k: &str, val: &str| {
            v.extend_from_slice(k.as_bytes());
            v.push(b'=');
            v.extend_from_slice(val.as_bytes());
            v.push(0);
        };
        if !self.env_clear {
            for (k, val) in crate::env::vars() {
                if !self.env.iter().any(|(k2, _)| *k2 == k) {
                    put(&k, &val);
                }
            }
        }
        for (k, val) in &self.env {
            put(k, val);
        }
        v
    }

    fn start(&self, console: Option<u64>) -> Result<Child> {
        let image = self.find_image()?;
        let block = self.argument_block();
        if block.len() > 1024 {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let slot = SPAWN_SLOT.load(Ordering::Relaxed);
        // what the child asks for, then less if the parent may not hand it out
        for (fs, net) in [(self.files, self.net), (self.files, false), (false, false)] {
            let mut rq = self.request(console, fs, net);
            rq[40..48].copy_from_slice(&(block.as_ptr() as u64).to_le_bytes());
            rq[48..56].copy_from_slice(&(block.len() as u64).to_le_bytes());
            let scope = self.scope.clone().filter(|_| fs);
            let mut hook = |tid: u64| -> i64 {
                match &scope {
                    Some(dir) => match crate::fs::scope(tid, dir) {
                        Ok(()) => 0,
                        Err(_) => -5,
                    },
                    None => 0,
                }
            };
            let id = metallium_loader::spawn_with(|i, a1, a2, a3, a4| crate::svc(i, a1, a2, a3, a4, 0)[0], slot, &mut rq, &image, &mut hook);
            match id {
                id if id > 0 => return Ok(Child { id }),
                -1 if fs || net => continue, // not allowed to hand out file or network access: try with less
                e => return Err(Error::from_kernel(e)),
            }
        }
        Err(Error::new(ErrorKind::PermissionDenied))
    }

    /// Start the child with the endpoint in my slot `console_slot` as its standard streams (the caller serves it: see
    /// docs/ABI.md, "Console").
    pub fn spawn_with_console(&mut self, console_slot: u64) -> Result<Child> {
        self.start(Some(console_slot))
    }

    /// Start the child with this program's standard streams.
    pub fn spawn(&mut self) -> Result<Child> {
        let console = if self.inherit_stdio { Some(crate::STDOUT_SLOT) } else { None };
        // (a parent without a console endpoint just lets the child use the kernel console)
        self.start(console.filter(|_| crate::console_is_endpoint()))
    }

    /// Run the child to the end with its input taken from `stdin`, and collect what it wrote to standard output and error.
    pub fn output_with_input(&mut self, stdin: &[u8]) -> Result<Output> {
        let ep = crate::ep_create(FACTORY_SLOT.load(Ordering::Relaxed));
        if ep < 0 {
            return Err(Error::from_kernel(ep));
        }
        let ep = ep as u64;
        let child = match self.start(Some(ep)) {
            Ok(c) => c,
            Err(e) => {
                crate::revoke_self(ep);
                return Err(e);
            }
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input = stdin;
        let mut want: Option<usize> = None;
        let status = loop {
            if want.is_none() {
                let m = crate::svc(crate::IPC_RECV, ep, 1, 0, 0, 0);
                if m[0] as i64 == 0 {
                    let (w0, w1, w2) = (m[2], m[3], m[4]);
                    match w0 & 0xFF {
                        op @ (1 | 3) => {
                            let n = (((w0 >> 8) & 0xFF) as usize).min(22);
                            let mut b = [0u8; 22];
                            b[..6].copy_from_slice(&(w0 >> 16).to_le_bytes()[..6]);
                            b[6..14].copy_from_slice(&w1.to_le_bytes());
                            b[14..22].copy_from_slice(&w2.to_le_bytes());
                            if op == 1 { &mut out } else { &mut err }.extend_from_slice(&b[..n]);
                            crate::reply([0, 0, 0]);
                        }
                        2 => want = Some((((w0 >> 8) & 0xFF) as usize).clamp(1, 23)),
                        _ => {
                            crate::reply([0, 0, 0]);
                        }
                    }
                    continue;
                }
            }
            if let Some(max) = want.take() {
                let n = input.len().min(max); // n = 0 is the end of the input
                let mut b = [0u8; 24];
                b[..n].copy_from_slice(&input[..n]);
                input = &input[n..];
                let word = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
                crate::reply([(n as u64) | (word(0) << 8), word(7), word(15)]);
                continue;
            }
            let w = crate::wait(child.id as u64, true);
            if w != -10 {
                break ExitStatus(w as u64);
            }
            crate::sleep(1);
        };
        crate::revoke_self(ep);
        Ok(Output { status, stdout: out, stderr: err })
    }

    pub fn output(&mut self) -> Result<Output> {
        self.output_with_input(&[])
    }
}

impl Child {
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Wait for the child to end.
    pub fn wait(self) -> Result<ExitStatus> {
        match crate::wait(self.id as u64, false) {
            -5 => Err(Error::new(ErrorKind::InvalidInput)),
            c => Ok(ExitStatus(c as u64)),
        }
    }

    /// The status if the child has ended, `None` if it still runs.
    pub fn try_wait(&self) -> Result<Option<ExitStatus>> {
        match crate::wait(self.id as u64, true) {
            -10 => Ok(None),
            -5 => Err(Error::new(ErrorKind::InvalidInput)),
            c => Ok(Some(ExitStatus(c as u64))),
        }
    }

    pub fn kill(&self) -> Result<()> {
        match crate::svc(crate::KILL, self.id as u64, 0, 0, 0, 0)[0] as i64 {
            0 => Ok(()),
            e => Err(Error::from_kernel(e)),
        }
    }
}
