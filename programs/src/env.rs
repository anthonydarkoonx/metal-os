//! The command line and environment (the argument block the parent supplied, see docs/ABI.md).
use alloc::string::String;

pub struct Args {
    inner: alloc::vec::IntoIter<String>,
}

impl Iterator for Args {
    type Item = String;
    fn next(&mut self) -> Option<String> {
        self.inner.next()
    }
}

fn ctx() -> crate::Ctx {
    let (p, l, a, par) = crate::rt::saved();
    crate::Ctx::new(a, par, p, l)
}

/// The command line, program name first.
pub fn args() -> Args {
    let c = ctx();
    let v: alloc::vec::Vec<String> = (0..c.argc()).filter_map(|i| c.argv(i)).map(String::from).collect();
    Args { inner: v.into_iter() }
}

pub fn var(key: &str) -> Option<String> {
    ctx().env(key).map(String::from)
}

/// The environment as (key, value) pairs.
pub fn vars() -> alloc::vec::Vec<(String, String)> {
    ctx().environment().map(|(k, v)| (String::from(k), String::from(v))).collect()
}

/// The numeric argument the parent passed (`x1` at start).
pub fn arg() -> u64 {
    crate::rt::saved().2
}

/// The task id of the parent.
pub fn parent() -> u64 {
    crate::rt::saved().3
}
