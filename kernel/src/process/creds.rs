//! Process credentials: the ids `getuid`/`setuid`/`setresuid`/… and `getgroups`/`setgroups` keep.
//!
//! This is bookkeeping with Linux's rules for who may change what; nothing else enforces it. Files have no owners (`stat` says
//! 0/0), so no open, kill or exec is refused for a uid. Everything boots as root (all ids 0). The privilege check is `euid == 0`,
//! which is what holds `CAP_SETUID`/`CAP_SETGID` on a Linux machine with no capabilities configured.
//!
//! The functions are pure (`Creds` in, `Creds` out) so the syscall layer can read a snapshot, apply one, and commit the result
//! without holding a lock across user-memory access.

use alloc::vec::Vec;

/// `(uid_t)-1` in a `setreuid`/`setresuid` argument: "leave this id alone".
pub const KEEP: u32 = u32::MAX;
/// `NGROUPS_MAX`.
pub const NGROUPS_MAX: usize = 65536;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Creds {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fsgid: u32,
    pub groups: Vec<u32>,
}

/// `EPERM`: the caller may not make this change.
#[derive(Debug, PartialEq, Eq)]
pub struct NotPermitted;

impl Creds {
    fn privileged(&self) -> bool {
        self.euid == 0
    }

    pub fn setuid(&self, uid: u32) -> Result<Creds, NotPermitted> {
        let mut n = self.clone();
        if self.privileged() {
            n.ruid = uid;
            n.euid = uid;
            n.suid = uid;
        } else if uid == self.ruid || uid == self.suid {
            n.euid = uid;
        } else {
            return Err(NotPermitted);
        }
        n.fsuid = n.euid;
        Ok(n)
    }

    pub fn setgid(&self, gid: u32) -> Result<Creds, NotPermitted> {
        let mut n = self.clone();
        if self.privileged() {
            n.rgid = gid;
            n.egid = gid;
            n.sgid = gid;
        } else if gid == self.rgid || gid == self.sgid {
            n.egid = gid;
        } else {
            return Err(NotPermitted);
        }
        n.fsgid = n.egid;
        Ok(n)
    }

    pub fn setreuid(&self, r: u32, e: u32) -> Result<Creds, NotPermitted> {
        let mut n = self.clone();
        if r != KEEP {
            if !self.privileged() && r != self.ruid && r != self.euid {
                return Err(NotPermitted);
            }
            n.ruid = r;
        }
        if e != KEEP {
            if !self.privileged() && e != self.ruid && e != self.euid && e != self.suid {
                return Err(NotPermitted);
            }
            n.euid = e;
        }
        // The saved id follows the effective one whenever the real id was set, or the effective id moved off the old real id.
        if r != KEEP || (e != KEEP && e != self.ruid) {
            n.suid = n.euid;
        }
        n.fsuid = n.euid;
        Ok(n)
    }

    pub fn setregid(&self, r: u32, e: u32) -> Result<Creds, NotPermitted> {
        let mut n = self.clone();
        if r != KEEP {
            if !self.privileged() && r != self.rgid && r != self.egid {
                return Err(NotPermitted);
            }
            n.rgid = r;
        }
        if e != KEEP {
            if !self.privileged() && e != self.rgid && e != self.egid && e != self.sgid {
                return Err(NotPermitted);
            }
            n.egid = e;
        }
        if r != KEEP || (e != KEEP && e != self.rgid) {
            n.sgid = n.egid;
        }
        n.fsgid = n.egid;
        Ok(n)
    }

    pub fn setresuid(&self, r: u32, e: u32, s: u32) -> Result<Creds, NotPermitted> {
        let allowed = |v: u32| v == KEEP || self.privileged() || v == self.ruid || v == self.euid || v == self.suid;
        if !(allowed(r) && allowed(e) && allowed(s)) {
            return Err(NotPermitted);
        }
        let mut n = self.clone();
        if r != KEEP { n.ruid = r; }
        if e != KEEP { n.euid = e; }
        if s != KEEP { n.suid = s; }
        n.fsuid = n.euid;
        Ok(n)
    }

    pub fn setresgid(&self, r: u32, e: u32, s: u32) -> Result<Creds, NotPermitted> {
        let allowed = |v: u32| v == KEEP || self.privileged() || v == self.rgid || v == self.egid || v == self.sgid;
        if !(allowed(r) && allowed(e) && allowed(s)) {
            return Err(NotPermitted);
        }
        let mut n = self.clone();
        if r != KEEP { n.rgid = r; }
        if e != KEEP { n.egid = e; }
        if s != KEEP { n.sgid = s; }
        n.fsgid = n.egid;
        Ok(n)
    }

    /// `setfsuid`: returns nothing on refusal; the caller reports the previous value either way, as Linux does.
    pub fn setfsuid(&self, uid: u32) -> Creds {
        let mut n = self.clone();
        if uid != KEEP && (self.privileged() || uid == self.ruid || uid == self.euid || uid == self.suid || uid == self.fsuid) {
            n.fsuid = uid;
        }
        n
    }

    pub fn setfsgid(&self, gid: u32) -> Creds {
        let mut n = self.clone();
        if gid != KEEP && (self.privileged() || gid == self.rgid || gid == self.egid || gid == self.sgid || gid == self.fsgid) {
            n.fsgid = gid;
        }
        n
    }

    /// `setgroups`: root only.
    pub fn setgroups(&self, list: Vec<u32>) -> Result<Creds, NotPermitted> {
        if !self.privileged() {
            return Err(NotPermitted);
        }
        let mut n = self.clone();
        n.groups = list;
        Ok(n)
    }
}
