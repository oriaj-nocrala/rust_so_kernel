// kernel/src/ipc/channel.rs
//
// Channel — the core IPC primitive.
//
// Design goals:
//   • Messages fit in one L1 cache line (64 bytes).
//   • Ring buffer is a fixed array — no heap allocation on the hot path.
//   • Blocking send/recv integrate with the existing scheduler.
//
// A Channel is unidirectional: one writer side, one reader side.
// A bidirectional socket pair is two Channels (one each way).
//
// LOCKING: All Channel operations happen inside CHANNEL_TABLE's Mutex.
// The caller must hold cli while holding that lock (same rules as SCHEDULER).

use spin::Mutex;
use alloc::vec::Vec;

// ============================================================================
// MESSAGE — one cache line
// ============================================================================

/// A single IPC message.  Exactly 64 bytes = one L1 cache line on x86.
///
/// `tag`  — application-defined message type (e.g. a Wayland opcode).
/// `len`  — number of valid bytes in `data` (0..=56).
/// `data` — inline payload.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct Message {
    pub tag:  u32,
    pub len:  u32,
    pub data: [u8; 56],
}

impl Message {
    pub const fn empty() -> Self {
        Self { tag: 0, len: 0, data: [0u8; 56] }
    }

    /// Build a message from a tag and a byte slice (truncated to 56 bytes).
    pub fn new(tag: u32, payload: &[u8]) -> Self {
        let len = core::cmp::min(payload.len(), 56) as u32;
        let mut data = [0u8; 56];
        data[..len as usize].copy_from_slice(&payload[..len as usize]);
        Self { tag, len, data }
    }

    pub fn payload(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

// ============================================================================
// RING BUFFER — fixed capacity, no alloc
// ============================================================================

const RING_CAP: usize = 16;

struct RingBuf {
    buf:  [Message; RING_CAP],
    head: usize,   // next read position
    tail: usize,   // next write position
    len:  usize,
}

impl RingBuf {
    const fn new() -> Self {
        Self {
            buf:  [Message::empty(); RING_CAP],
            head: 0,
            tail: 0,
            len:  0,
        }
    }

    fn is_empty(&self) -> bool { self.len == 0 }
    fn is_full(&self)  -> bool { self.len == RING_CAP }

    fn push(&mut self, msg: Message) -> bool {
        if self.is_full() { return false; }
        self.buf[self.tail] = msg;
        self.tail = (self.tail + 1) % RING_CAP;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<Message> {
        if self.is_empty() { return None; }
        let msg = self.buf[self.head];
        self.head = (self.head + 1) % RING_CAP;
        self.len -= 1;
        Some(msg)
    }
}

// ============================================================================
// CHANNEL
// ============================================================================

pub type ChannelId = usize;

/// State of a named server endpoint (before accept).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Listening,
    /// A client is waiting to connect; this is the client's pid.
    PendingConnect(usize),
}

/// A single channel endpoint.
///
/// For a connected socket pair:
///   - client holds `peer: Some(server_channel_id)` and vice versa.
///   - `rx` holds messages sent *to* this endpoint.
///   - to send, write into the peer's `rx`.
pub struct Channel {
    /// Messages queued for reading by this endpoint's owner.
    rx: RingBuf,

    /// PID(s) blocked waiting to read from this channel.
    read_waiters: Vec<usize>,

    /// PID(s) blocked waiting for a connect (accept waiters).
    accept_waiters: Vec<usize>,

    /// The other side of a connected pair.
    pub peer: Option<ChannelId>,

    /// Server-side state (set after bind+listen).
    pub server_state: Option<ServerState>,

    /// Path this channel is bound to (empty = unbound).
    pub bound_path: Option<[u8; 64]>,
}

impl Channel {
    pub fn new() -> Self {
        Self {
            rx: RingBuf::new(),
            read_waiters: Vec::new(),
            accept_waiters: Vec::new(),
            peer: None,
            server_state: None,
            bound_path: None,
        }
    }

    pub fn has_messages(&self) -> bool {
        !self.rx.is_empty()
    }

    /// True if the receive queue is full (no room for new messages).
    /// Used by poll/epoll to check POLLOUT readiness on the peer's channel.
    pub fn is_rx_full(&self) -> bool {
        self.rx.is_full()
    }

    /// Push a message into this channel's receive queue.
    /// Returns false if the queue is full.
    pub fn enqueue(&mut self, msg: Message) -> bool {
        self.rx.push(msg)
    }

    /// Pop a message from this channel's receive queue.
    pub fn dequeue(&mut self) -> Option<Message> {
        self.rx.pop()
    }

    /// Register a PID as blocked waiting to read.
    pub fn add_read_waiter(&mut self, pid: usize) {
        if !self.read_waiters.contains(&pid) {
            self.read_waiters.push(pid);
        }
    }

    /// Take all read waiters (to wake them).
    pub fn take_read_waiters(&mut self) -> Vec<usize> {
        core::mem::take(&mut self.read_waiters)
    }

    /// Register a PID as blocked waiting to accept.
    pub fn add_accept_waiter(&mut self, pid: usize) {
        if !self.accept_waiters.contains(&pid) {
            self.accept_waiters.push(pid);
        }
    }

    /// Take all accept waiters (to wake them).
    pub fn take_accept_waiters(&mut self) -> Vec<usize> {
        core::mem::take(&mut self.accept_waiters)
    }
}

// ============================================================================
// CHANNEL TABLE — global registry
// ============================================================================

const MAX_CHANNELS: usize = 64;

pub struct ChannelTable {
    slots: [Option<Channel>; MAX_CHANNELS],
    next_id: usize,
}

impl ChannelTable {
    pub const fn new() -> Self {
        // Can't use array initializer with non-Copy type; use a const None
        const NONE: Option<Channel> = None;
        Self {
            slots: [NONE; MAX_CHANNELS],
            next_id: 1,   // 0 = invalid sentinel
        }
    }

    /// Allocate a new channel, return its ID.
    pub fn alloc(&mut self) -> Option<ChannelId> {
        // Find a free slot starting from next_id (wrapping)
        for _ in 0..MAX_CHANNELS {
            let id = self.next_id;
            self.next_id = (self.next_id % (MAX_CHANNELS - 1)) + 1;
            if self.slots[id].is_none() {
                self.slots[id] = Some(Channel::new());
                return Some(id);
            }
        }
        None   // table full
    }

    /// Free a channel.
    pub fn free(&mut self, id: ChannelId) {
        if id < MAX_CHANNELS {
            self.slots[id] = None;
        }
    }

    pub fn get(&self, id: ChannelId) -> Option<&Channel> {
        self.slots.get(id)?.as_ref()
    }

    pub fn get_mut(&mut self, id: ChannelId) -> Option<&mut Channel> {
        self.slots.get_mut(id)?.as_mut()
    }

    /// Find a channel bound to the given path.
    pub fn find_by_path(&self, path: &[u8]) -> Option<ChannelId> {
        for (id, slot) in self.slots.iter().enumerate() {
            if let Some(ch) = slot {
                if let Some(bound) = &ch.bound_path {
                    let len = path.len().min(64);
                    if &bound[..len] == &path[..len]
                        && (len == 64 || bound[len] == 0)
                    {
                        return Some(id);
                    }
                }
            }
        }
        None
    }
}

/// Global channel table.  Protected by Mutex; caller holds cli.
pub static CHANNELS: Mutex<ChannelTable> = Mutex::new(ChannelTable::new());
