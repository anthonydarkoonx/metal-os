//! Endpoints: typed wrappers over the IPC system calls. A message is three words.
use crate::io::{Error, Result};

/// A capability slot holding an endpoint.
#[derive(Clone, Copy, Debug)]
pub struct Endpoint(pub u64);

pub struct Request {
    /// The sender's task id (set by the kernel; threads show their own id, `SYSINFO` page 5 names their program).
    pub sender: u64,
    pub words: [u64; 3],
    /// A call (the sender waits for `reply`) rather than a one-way send.
    pub is_call: bool,
}

impl Endpoint {
    /// Make a new endpoint with the `Factory` capability in `factory`; the creator holds send, receive and grant.
    pub fn create(factory: u64) -> Result<Endpoint> {
        match crate::ep_create(factory) {
            s if s >= 0 => Ok(Endpoint(s as u64)),
            e => Err(Error::from_kernel(e)),
        }
    }

    /// Send and wait for the answer.
    pub fn call(&self, w: [u64; 3]) -> Result<[u64; 3]> {
        match crate::call(self.0, w) {
            (0, r) => Ok(r),
            (e, _) => Err(Error::from_kernel(e)),
        }
    }

    /// Send without waiting for an answer (the message is queued; `WouldBlock` if the queue is full).
    pub fn send(&self, w: [u64; 3]) -> Result<()> {
        match crate::svc(crate::IPC_SEND, self.0, w[0], w[1], w[2], 0)[0] as i64 {
            0 => Ok(()),
            e => Err(Error::from_kernel(e)),
        }
    }

    /// Wait for the next message.
    pub fn recv(&self) -> Result<Request> {
        match crate::recv(self.0) {
            Ok(m) => Ok(Request { sender: m.sender, words: m.w, is_call: m.call }),
            Err(e) => Err(Error::from_kernel(e)),
        }
    }

    /// The next message if there is one.
    pub fn try_recv(&self) -> Result<Option<Request>> {
        let r = crate::svc(crate::IPC_RECV, self.0, 1, 0, 0, 0);
        match r[0] as i64 {
            0 => Ok(Some(Request { sender: r[1], words: [r[2], r[3], r[4]], is_call: r[5] != 0 })),
            -10 => Ok(None),
            e => Err(Error::from_kernel(e)),
        }
    }

    /// Give up the capability and everything derived from it; for the creator of the endpoint that ends it.
    pub fn destroy(self) -> Result<()> {
        match crate::revoke_self(self.0) {
            e if e >= 0 => Ok(()),
            e => Err(Error::from_kernel(e)),
        }
    }
}

/// Answer the call being served.
pub fn reply(w: [u64; 3]) -> Result<()> {
    match crate::reply(w) {
        0 => Ok(()),
        e => Err(Error::from_kernel(e)),
    }
}

/// Hand a capability of yours to a task (needs the grant right): the slot it lands in there.
pub fn grant(slot: u64, to_task: u64, rights_mask: u16) -> Result<u64> {
    match crate::svc(crate::CAP_GRANT, slot, to_task, rights_mask as u64, 0, 0)[0] as i64 {
        s if s >= 0 => Ok(s as u64),
        e => Err(Error::from_kernel(e)),
    }
}
