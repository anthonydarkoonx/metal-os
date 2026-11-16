//! A client of the program manager's control endpoint, for shell programs (the manager gives them a send-only copy in slot 3): which programs are
//! installed, and starting one. (`docs/PROGRAMS.md`, "Control protocol".)
use alloc::string::String;
use alloc::vec::Vec;

/// A shell program's slot for the control endpoint.
pub const SHELL_CTL_SLOT: u64 = 3;
const OP_START: u64 = 3;
const OP_LIST: u64 = 8;
const OP_NAME: u64 = 12;

/// A program id or name as the two words the protocol carries (up to 16 bytes, NUL padded).
fn pack(s: &str) -> (u64, u64) {
    let mut b = [0u8; 16];
    let n = s.len().min(16);
    b[..n].copy_from_slice(&s.as_bytes()[..n]);
    (u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap()))
}

fn unpack(a: u64, b: u64) -> String {
    let mut v = [0u8; 16];
    v[..8].copy_from_slice(&a.to_le_bytes());
    v[8..].copy_from_slice(&b.to_le_bytes());
    let n = v.iter().position(|&c| c == 0).unwrap_or(16);
    String::from_utf8_lossy(&v[..n]).into_owned()
}

fn ask(w: [u64; 3]) -> Option<[u64; 3]> {
    let (st, r) = crate::call(SHELL_CTL_SLOT, w);
    (st == 0 && r[0] == 0).then_some(r)
}

/// The installed programs: (id, name).
pub fn installed() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for i in 0..64u64 {
        let Some(r) = ask([OP_LIST, i, 0]) else { break };
        let id = unpack(r[1], r[2]);
        let name = ask([OP_NAME, r[1], r[2]]).map(|n| unpack(n[1], n[2])).filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
        out.push((id, name));
    }
    out
}

/// Start a program (its first ability, as the terminal's `prog start` does). Returns whether the manager did.
pub fn start(id: &str) -> bool {
    let (a, b) = pack(id);
    ask([OP_START, a, b]).is_some()
}

const OP_POLICY_GET: u64 = 13;
const OP_POLICY_SET: u64 = 14;

/// The permissions in the manager's bit order (docs/PROGRAMS.md, "Permission policy"): bit, short name, what it lets a program do.
pub const PERMISSIONS: [(u8, &str, &str); 6] = [
    (1, "Files", "Its own folder on the data disk"),
    (2, "Network", "Connect to other computers"),
    (4, "Windows", "Open windows on the desktop"),
    (8, "Shell", "Taskbar, launcher and system controls"),
    (16, "All files", "Every file on the disk"),
    (32, "Admin", "Take rights away from other programs"),
];

/// What a program may do: what its signed manifest declares (the most it can ever get) and what the user has taken away (stored, from the next
/// start on), and whether it runs now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub declared: u8,
    pub off: u8,
    pub running: bool,
}

impl Policy {
    /// The permissions the program gets at its next start.
    pub fn effective(&self) -> u8 {
        self.declared & !self.off
    }
}

pub fn policy(id: &str) -> Option<Policy> {
    let (a, b) = pack(id);
    let r = ask([OP_POLICY_GET, a, b])?;
    Some(Policy { declared: (r[1] >> 8) as u8 & 0x3F, off: r[1] as u8 & 0x3F, running: r[2] != u64::MAX })
}

/// Store which permissions are taken away from a program (bits as in [`PERMISSIONS`]). Returns whether the manager accepted it.
pub fn set_policy(id: &str, off: u8) -> bool {
    let (a, b) = pack(id);
    ask([OP_POLICY_SET | ((off as u64 & 0x3F) << 16), a, b]).is_some()
}
