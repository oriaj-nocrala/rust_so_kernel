// vfs/src/mount.rs
//
// The mount table: longest-prefix-match path resolution, symlink following
// (with an ELOOP guard), and the VFS-level mutation operations
// (mkdir/symlink/unlink/rmdir/rename, with rollback on a failed rename).
//
// PATH RESOLUTION
// ───────────────
//   resolve("/dev/console")
//     1. Longest-prefix match → mount at "/dev"
//     2. rel_path = "console"
//     3. DevFs.root().lookup("console") → DevInode
//
// OPEN
// ────
//   open(path, flags) = resolve(path)?.open(flags)
//   Returns a Box<dyn FileHandle> ready for read/write in the FD table.
//
// `MountTable` is a plain struct with methods, not a global — the kernel
// adapter (`kernel/src/fs/vfs.rs`) owns the single `static` instance and
// free functions that delegate to it, same shape as `EXT2: Once<Ext2Fs>`.
// See `docs/fs/vfs-extraction-plan.md`, step 4.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use crate::lock::Mutex;

use crate::file::FileHandle;
use crate::inode::{Filesystem, Inode};
use crate::path::{normalize_path, split_parent};
use crate::types::{Errno, FileType, OpenFlags, Stat};

struct MountEntry {
    /// Absolute path prefix (e.g. "/dev", "/").  No trailing slash.
    prefix: &'static str,
    fs:     Arc<dyn Filesystem>,
}

/// Symlink chains longer than this are rejected with `ELOOP`, same spirit
/// as real Linux's (much larger) `MAXSYMLINKS` — this kernel only ever
/// produces short, deliberately-built chains (procfs), so a small bound
/// is enough to catch a real cycle without being a meaningful limitation.
const MAX_SYMLINK_HOPS: u32 = 8;

/// A table of mounted filesystems, resolved by longest-prefix match.
///
/// Not a global by itself — construct one (`MountTable::new()`) and let the
/// owner (the kernel adapter, in real use; a test, in host tests) decide
/// how it's stored and shared.
pub struct MountTable {
    /// Kept sorted by descending prefix length after every `mount()` call,
    /// so `find()` can do a simple linear scan and stop at the first hit.
    entries: Mutex<Vec<MountEntry>>,
}

impl MountTable {
    /// Construct an empty mount table.
    pub const fn new() -> Self {
        Self { entries: Mutex::new(Vec::new()) }
    }

    /// Mount `fs` at `prefix`.
    ///
    /// The table is kept sorted longest-prefix-first so that `resolve` can
    /// do a simple linear scan and stop at the first match.
    pub fn mount(&self, prefix: &'static str, fs: Arc<dyn Filesystem>) {
        let mut table = self.entries.lock();
        table.push(MountEntry { prefix, fs });
        table.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
    }

    /// Locate the mount covering `path` by longest-prefix match, returning
    /// its prefix and a cloned handle to its filesystem.
    ///
    /// Locks `entries` and drops the guard again before returning, instead
    /// of returning the guard (or a reference into it) — deliberately, not
    /// an oversight. The caller (`resolve_inner`) is about to call into
    /// `Filesystem::root()` and then walk `Inode::lookup()` on whatever
    /// this hands back, and `fs::initramfs`'s root directory
    /// (`RootDirInode::lookup`/`::readdir`) calls straight back into this
    /// same `MountTable` via `direct_children("/")` *from inside* that
    /// walk. `spin::Mutex` is not reentrant, so holding this lock across
    /// either call would self-deadlock the first `ls /` a real boot ever
    /// does. `find()` is the one place in this file allowed to touch the
    /// lock around an `Inode`/`Filesystem` call — because it doesn't make
    /// one. See `docs/fs/vfs-extraction-plan.md`'s decision #3.
    fn find(&self, path: &str) -> Option<(&'static str, Arc<dyn Filesystem>)> {
        let table = self.entries.lock();
        let entry = table.iter().find(|e| {
            path == e.prefix
                || path.starts_with(e.prefix)
                    && (e.prefix == "/"
                        || path[e.prefix.len()..].starts_with('/'))
        })?;
        Some((entry.prefix, entry.fs.clone()))
    }

    /// Names of filesystems mounted exactly one path component below
    /// `parent` (e.g. `direct_children("/")` → `["dev", "tmp", "proc", ...]`).
    ///
    /// On real Linux, `/proc`, `/dev`, etc. show up in `ls /` because
    /// they're real, pre-existing empty directories in the root filesystem
    /// that a mount later overlays — traversal redirects into the mount,
    /// but the parent directory's own listing is what makes the mountpoint
    /// visible at all. This is the equivalent for our synthetic root:
    /// `fs::initramfs`'s root directory calls this to list every other
    /// mount as a real (if empty from its perspective — actual traversal
    /// never reaches them, since a longer, more specific mount prefix
    /// always wins in `resolve_inner`) subdirectory entry, instead of only
    /// ever showing its own "bin".
    pub fn direct_children(&self, parent: &str) -> Vec<&'static str> {
        let table = self.entries.lock();
        table.iter().filter_map(|e| {
            if e.prefix == parent {
                return None; // don't list the mount itself as its own child
            }
            let rel = if parent == "/" {
                e.prefix.strip_prefix('/')?
            } else {
                e.prefix.strip_prefix(parent)?.strip_prefix('/')?
            };
            if rel.is_empty() || rel.contains('/') {
                None // not a direct child: either self ("") or nested deeper
            } else {
                Some(rel)
            }
        }).collect()
    }

    /// Resolve an absolute path to its inode, following symlinks —
    /// including one at the final path component (matches
    /// `open()`/`stat()` semantics). Use `resolve_no_follow` for
    /// `lstat`/`readlink`, which must see the symlink itself rather than
    /// whatever it points to.
    ///
    /// # Errors
    /// - `EINVAL`  — path does not start with `/`
    /// - `ENOENT`  — no mount found or a path component doesn't exist
    /// - `ENOTDIR` — a non-terminal component is not a directory
    /// - `ELOOP`   — more than `MAX_SYMLINK_HOPS` symlinks chained together
    pub fn resolve(&self, path: &str) -> Result<Arc<dyn Inode>, Errno> {
        self.resolve_inner(path, true, MAX_SYMLINK_HOPS)
    }

    /// Like `resolve`, but never follows a symlink at the *final* path
    /// component — intermediate components are still always followed
    /// (real `lstat(2)`/`readlink(2)` behavior: `/a/link/b` still requires
    /// `link` to be a real, followable directory, only the leaf is left
    /// alone).
    pub fn resolve_no_follow(&self, path: &str) -> Result<Arc<dyn Inode>, Errno> {
        self.resolve_inner(path, false, MAX_SYMLINK_HOPS)
    }

    fn resolve_inner(&self, path: &str, follow_final: bool, hops_left: u32) -> Result<Arc<dyn Inode>, Errno> {
        if !path.starts_with('/') {
            return Err(Errno::EINVAL);
        }

        let (mount_prefix, fs) = self.find(path).ok_or(Errno::ENOENT)?;

        // Strip the mount prefix to get the path relative to this filesystem.
        let rel = if mount_prefix == "/" {
            &path[1..]
        } else {
            path[mount_prefix.len()..].trim_start_matches('/')
        };

        let mut node: Arc<dyn Inode> = fs.root()?;

        let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        let last_idx = components.len().checked_sub(1);

        for (i, component) in components.iter().enumerate() {
            match *component {
                "."  => { /* stay at current directory */ }
                ".." => {
                    // Every caller normalizes `..`/`.` away before a path
                    // reaches this function (`resolve_path` in the syscall
                    // layer, and this function's own relative-symlink-target
                    // handling below, both go through `normalize_path` first
                    // — verified against every `vfs::resolve*`/`vfs::open`/
                    // `fs::stat` call site in the kernel). A raw `..` showing
                    // up here anyway means some caller skipped that step; fail
                    // loudly with `EINVAL` instead of silently resolving the
                    // wrong file the way a no-op here used to.
                    return Err(Errno::EINVAL);
                }
                name => {
                    node = node.lookup(name)?;
                    let is_final = Some(i) == last_idx;
                    if node.file_type() == FileType::Symlink && (!is_final || follow_final) {
                        if hops_left == 0 {
                            return Err(Errno::ELOOP);
                        }
                        let target = node.readlink()?;
                        // A relative target resolves against the symlink's own
                        // containing directory (real symlink(2)/readlink(2)
                        // semantics), not root — matters now that ext2's real
                        // `symlink()` can produce genuinely relative targets
                        // (e.g. `symlink("realfile.txt", "/mnt/link")`, the
                        // common case for a real `ln -s`). Reconstruct that
                        // directory from the mount prefix plus every path
                        // component consumed so far (everything before this
                        // one, i.e. `components[..i]`) — `Inode` itself has no
                        // notion of "my containing directory" to ask for
                        // directly.
                        let abs_target = if target.starts_with('/') {
                            target
                        } else {
                            let mut dir_path = String::from(mount_prefix);
                            for c in &components[..i] {
                                dir_path.push('/');
                                dir_path.push_str(c);
                            }
                            normalize_path(&dir_path, &target)
                        };
                        node = self.resolve_inner(&abs_target, follow_final, hops_left - 1)?;
                    }
                }
            }
        }

        Ok(node)
    }

    /// Resolve `path` and open it, returning an FD-ready `FileHandle`.
    ///
    /// If `path` doesn't exist and `O_CREAT` is set, resolves the *parent*
    /// directory instead and asks it to `create()` the leaf component.
    pub fn open(&self, path: &str, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        match self.resolve(path) {
            Ok(inode) => inode.open(flags),
            Err(Errno::ENOENT) if flags.0 & OpenFlags::CREAT.0 != 0 => self.create_and_open(path, flags),
            Err(e) => Err(e),
        }
    }

    fn create_and_open(&self, path: &str, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        let (dir_path, leaf) = split_parent(path)?;
        let dir = self.resolve(dir_path)?;
        let inode = dir.create(leaf)?;
        inode.open(flags)
    }

    /// Resolve `path` and return its metadata.
    pub fn stat(&self, path: &str) -> Result<Stat, Errno> {
        Ok(self.resolve(path)?.stat())
    }

    /// Create a new directory at `path`.
    pub fn mkdir(&self, path: &str) -> Result<(), Errno> {
        let (dir_path, leaf) = split_parent(path)?;
        self.resolve(dir_path)?.mkdir(leaf)?;
        Ok(())
    }

    /// Create a symlink at `path` pointing at `target`. `path`'s parent
    /// directory is resolved (and must exist and be writable); `target` is
    /// stored as-is, unresolved — matches real `symlink(2)`.
    pub fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        let (dir_path, leaf) = split_parent(path)?;
        self.resolve(dir_path)?.symlink(leaf, target)?;
        Ok(())
    }

    /// Create an AF_UNIX socket node at `path` — `bind()` with a pathname
    /// address. Fails with `EEXIST` if the name is taken, which is exactly
    /// what makes a second `bind()` to the same path report `EADDRINUSE`.
    pub fn mksocket(&self, path: &str) -> Result<(), Errno> {
        let (dir_path, leaf) = split_parent(path)?;
        self.resolve(dir_path)?.mksocket(leaf)?;
        Ok(())
    }

    /// Remove the file at `path` (fails with `EISDIR` on directories).
    pub fn unlink(&self, path: &str) -> Result<(), Errno> {
        let (dir_path, leaf) = split_parent(path)?;
        self.resolve(dir_path)?.unlink(leaf)
    }

    /// Remove the empty directory at `path`.
    pub fn rmdir(&self, path: &str) -> Result<(), Errno> {
        let (dir_path, leaf) = split_parent(path)?;
        self.resolve(dir_path)?.rmdir(leaf)
    }

    /// Move/rename `old_path` to `new_path`. Both must resolve to
    /// directories on the same mounted filesystem (no cross-filesystem
    /// support — the target parent's `insert_child` will fail with
    /// `EROFS`/`ENOSYS` if not).
    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), Errno> {
        let (old_dir, old_leaf) = split_parent(old_path)?;
        let (new_dir, new_leaf) = split_parent(new_path)?;
        let old_parent = self.resolve(old_dir)?;
        let new_parent = self.resolve(new_dir)?;

        let node = old_parent.take_child(old_leaf)?;
        if let Err(e) = new_parent.insert_child(new_leaf, node.clone()) {
            // Best-effort rollback so a failed rename doesn't just lose the file.
            let _ = old_parent.insert_child(old_leaf, node);
            return Err(e);
        }
        Ok(())
    }
}

impl Default for MountTable {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::vec;
    use core::any::Any;

    // A minimal in-memory filesystem for exercising `MountTable` without
    // touching real hardware — dirs hold named children (files, dirs,
    // symlinks, or nested mounts of their own), files hold a byte buffer,
    // symlinks hold a target string. Just enough for the tests below:
    // lookup, stat/file_type, readlink, create, mkdir, symlink, unlink,
    // rmdir, take_child, insert_child, open.

    struct TestFile {
        content: Mutex<Vec<u8>>,
    }

    impl TestFile {
        fn new(content: &[u8]) -> Arc<Self> {
            Arc::new(Self { content: Mutex::new(content.to_vec()) })
        }
    }

    impl Inode for TestFile {
        fn stat(&self) -> Stat {
            Stat::regular(1, self.content.lock().len() as i64)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct TestLink {
        target: String,
    }

    impl TestLink {
        fn new(target: &str) -> Arc<Self> {
            Arc::new(Self { target: target.to_string() })
        }
    }

    impl Inode for TestLink {
        fn stat(&self) -> Stat {
            Stat::symlink(1, self.target.len() as i64)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn readlink(&self) -> Result<String, Errno> {
            Ok(self.target.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct TestDir {
        children: Mutex<BTreeMap<String, Arc<dyn Inode>>>,
    }

    impl TestDir {
        fn new() -> Arc<Self> {
            Arc::new(Self { children: Mutex::new(BTreeMap::new()) })
        }

        fn with(self: Arc<Self>, name: &str, node: Arc<dyn Inode>) -> Arc<Self> {
            self.children.lock().insert(name.to_string(), node);
            self
        }

        /// Non-consuming sibling of `with()`, for inserting into an
        /// already-`let`-bound `Arc<TestDir>` (e.g. inside a loop) without
        /// moving it away from the variable a later `mount_root_with` call
        /// still needs.
        fn insert(&self, name: &str, node: Arc<dyn Inode>) {
            self.children.lock().insert(name.to_string(), node);
        }
    }

    impl Inode for TestDir {
        fn stat(&self) -> Stat {
            Stat::dir(1)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
            self.children.lock().get(name).cloned().ok_or(Errno::ENOENT)
        }
        fn create(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
            let node: Arc<dyn Inode> = TestFile::new(b"");
            self.children.lock().insert(name.to_string(), node.clone());
            Ok(node)
        }
        fn mkdir(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
            let node: Arc<dyn Inode> = TestDir::new();
            self.children.lock().insert(name.to_string(), node.clone());
            Ok(node)
        }
        fn unlink(&self, name: &str) -> Result<(), Errno> {
            let mut children = self.children.lock();
            match children.get(name) {
                Some(n) if n.file_type() == FileType::Directory => Err(Errno::EISDIR),
                Some(_) => { children.remove(name); Ok(()) }
                None => Err(Errno::ENOENT),
            }
        }
        fn rmdir(&self, name: &str) -> Result<(), Errno> {
            let mut children = self.children.lock();
            match children.get(name) {
                Some(n) if n.file_type() != FileType::Directory => Err(Errno::ENOTDIR),
                Some(n) => {
                    let sub = n.as_any().downcast_ref::<TestDir>().expect("TestDir child");
                    if !sub.children.lock().is_empty() {
                        return Err(Errno::ENOTEMPTY);
                    }
                    children.remove(name);
                    Ok(())
                }
                None => Err(Errno::ENOENT),
            }
        }
        fn take_child(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
            self.children.lock().remove(name).ok_or(Errno::ENOENT)
        }
        fn insert_child(&self, name: &str, node: Arc<dyn Inode>) -> Result<(), Errno> {
            let mut children = self.children.lock();
            if children.contains_key(name) {
                return Err(Errno::EEXIST);
            }
            children.insert(name.to_string(), node);
            Ok(())
        }
        fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn Inode>, Errno> {
            let node: Arc<dyn Inode> = TestLink::new(target);
            self.children.lock().insert(name.to_string(), node.clone());
            Ok(node)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct TestFs {
        root: Arc<TestDir>,
    }

    impl TestFs {
        fn new(root: Arc<TestDir>) -> Arc<Self> {
            Arc::new(Self { root })
        }
    }

    impl Filesystem for TestFs {
        fn name(&self) -> &str {
            "testfs"
        }
        fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
            Ok(self.root.clone() as Arc<dyn Inode>)
        }
    }

    fn mount_root_with(table: &MountTable, root: Arc<TestDir>) {
        table.mount("/", TestFs::new(root));
    }

    // ── Longest-prefix match ─────────────────────────────────────────────

    #[test]
    fn longest_prefix_wins_over_root() {
        let table = MountTable::new();
        let root = TestDir::new().with("dev", TestFile::new(b"root-dev-shadowed"));
        mount_root_with(&table, root);
        let dev_root = TestDir::new().with("console", TestFile::new(b"console-data"));
        table.mount("/dev", TestFs::new(dev_root));

        let node = table.resolve("/dev/console").expect("resolves via /dev mount");
        let file = node.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"console-data");
    }

    #[test]
    fn longest_prefix_wins_regardless_of_mount_order() {
        let table = MountTable::new();
        // Mount in an order different from prefix length: "/", then the
        // longer "/dev/input", then the shorter "/dev" — `mount()` must
        // re-sort every time, not just append.
        mount_root_with(&table, TestDir::new());
        let input_root = TestDir::new().with("event0", TestFile::new(b"event0-data"));
        table.mount("/dev/input", TestFs::new(input_root));
        let dev_root = TestDir::new(); // deliberately has no "input" child
        table.mount("/dev", TestFs::new(dev_root));

        let node = table.resolve("/dev/input/event0").expect("resolves via /dev/input");
        let file = node.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"event0-data");
    }

    #[test]
    fn prefix_match_requires_component_boundary() {
        // "/dev" must not match "/device/x" — real prefix matching, not a
        // raw string prefix. See MountTable::find's boundary check
        // (`path[prefix.len()..].starts_with('/')`).
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new());
        table.mount("/dev", TestFs::new(TestDir::new()));

        assert_eq!(table.resolve("/device/x").err(), Some(Errno::ENOENT));
    }

    #[test]
    fn relative_path_is_einval() {
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new());
        assert_eq!(table.resolve("no/leading/slash").err(), Some(Errno::EINVAL));
    }

    #[test]
    fn no_covering_mount_is_enoent() {
        let table = MountTable::new();
        // Nothing mounted at all.
        assert_eq!(table.resolve("/anything").err(), Some(Errno::ENOENT));
    }

    // ── Components ───────────────────────────────────────────────────────

    #[test]
    fn dot_component_does_not_change_directory() {
        let table = MountTable::new();
        let root = TestDir::new().with("f", TestFile::new(b"data"));
        mount_root_with(&table, root);
        assert!(table.resolve("/./f").is_ok());
    }

    #[test]
    fn raw_dotdot_is_einval_on_purpose() {
        // NOT a bug: every real caller normalizes `..`/`.` away via
        // `normalize_path` before a path ever reaches `resolve_inner` (see
        // that function's own doc comment on the ".." arm). A raw `..`
        // getting this far means some caller skipped normalization, and
        // resolving it wrong (silently landing on the wrong file) would be
        // far worse than a loud EINVAL. Do not "fix" this into a real
        // parent-directory pop.
        let table = MountTable::new();
        // "a" must actually exist as a directory, or resolution fails with
        // ENOENT on the "a" component itself before ever reaching the
        // ".." arm this test means to exercise.
        let root = TestDir::new().with("a", TestDir::new() as Arc<dyn Inode>);
        mount_root_with(&table, root);
        assert_eq!(table.resolve("/a/../b").err(), Some(Errno::EINVAL));
    }

    #[test]
    fn missing_component_is_enoent() {
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new());
        assert_eq!(table.resolve("/nope").err(), Some(Errno::ENOENT));
    }

    #[test]
    fn non_directory_intermediate_component_is_enotdir() {
        let table = MountTable::new();
        let root = TestDir::new().with("f", TestFile::new(b"data"));
        mount_root_with(&table, root);
        // "f" is a file, not a directory — descending into "f/g" must fail
        // because `lookup` on a TestFile inherits the default ENOTDIR.
        assert_eq!(table.resolve("/f/g").err(), Some(Errno::ENOTDIR));
    }

    // ── Symlinks ─────────────────────────────────────────────────────────

    #[test]
    fn resolve_follows_final_symlink_no_follow_does_not() {
        let table = MountTable::new();
        let root = TestDir::new()
            .with("target.txt", TestFile::new(b"real-data"))
            .with("link", TestLink::new("target.txt"));
        mount_root_with(&table, root);

        let followed = table.resolve("/link").expect("resolve follows the final symlink");
        assert_eq!(followed.file_type(), FileType::Regular);

        let not_followed = table.resolve_no_follow("/link").expect("resolve_no_follow succeeds");
        assert_eq!(not_followed.file_type(), FileType::Symlink);
    }

    #[test]
    fn intermediate_symlink_is_followed_by_both() {
        let table = MountTable::new();
        let target_dir = TestDir::new().with("f", TestFile::new(b"data"));
        let root = TestDir::new()
            .with("realdir", target_dir as Arc<dyn Inode>)
            .with("linkdir", TestLink::new("realdir"));
        mount_root_with(&table, root);

        assert!(table.resolve("/linkdir/f").is_ok());
        assert!(table.resolve_no_follow("/linkdir/f").is_ok());
    }

    #[test]
    fn absolute_symlink_target_resolves_as_is() {
        let table = MountTable::new();
        let root = TestDir::new()
            .with("real.txt", TestFile::new(b"abs-data"))
            .with("link", TestLink::new("/real.txt"));
        mount_root_with(&table, root);

        let node = table.resolve("/link").expect("resolves via absolute target");
        let file = node.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"abs-data");
    }

    #[test]
    fn relative_symlink_target_resolves_against_its_own_directory_not_root() {
        // The common real-`ln -s` case: mount something at a non-"/"
        // prefix, put a symlink two levels deep with a bare relative
        // target, and confirm it resolves against its own containing
        // directory (/mnt/sub), not against the mount's root or "/".
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new().with("sibling.txt", TestFile::new(b"WRONG-root-file")));

        let sub = TestDir::new()
            .with("sibling.txt", TestFile::new(b"correct-mnt-sub-file"))
            .with("link", TestLink::new("sibling.txt"));
        let mnt_root = TestDir::new().with("sub", sub as Arc<dyn Inode>);
        table.mount("/mnt", TestFs::new(mnt_root));

        let node = table.resolve("/mnt/sub/link").expect("relative symlink resolves");
        let file = node.as_any().downcast_ref::<TestFile>().expect("TestFile");
        // Negative half: must NOT have resolved against "/" (that file's
        // content is deliberately different).
        assert_eq!(&file.content.lock()[..], b"correct-mnt-sub-file");
    }

    #[test]
    fn symlink_chain_of_exactly_max_hops_still_resolves() {
        let table = MountTable::new();
        let root = TestDir::new();
        // Build a chain link0 -> link1 -> ... -> link7 -> target.txt, i.e.
        // exactly MAX_SYMLINK_HOPS (8) hops from link0 to the real file.
        root.insert("target.txt", TestFile::new(b"chain-end"));
        for i in (0..8u32).rev() {
            let next = if i == 7 { "target.txt".to_string() } else { alloc::format!("link{}", i + 1) };
            root.insert(&alloc::format!("link{i}"), TestLink::new(&next));
        }
        mount_root_with(&table, root);

        let node = table.resolve("/link0").expect("exactly 8 hops still resolves");
        let file = node.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"chain-end");
    }

    #[test]
    fn symlink_chain_longer_than_max_hops_is_eloop() {
        let table = MountTable::new();
        let root = TestDir::new();
        root.insert("target.txt", TestFile::new(b"chain-end"));
        // 9 hops: one more than MAX_SYMLINK_HOPS.
        for i in (0..9u32).rev() {
            let next = if i == 8 { "target.txt".to_string() } else { alloc::format!("link{}", i + 1) };
            root.insert(&alloc::format!("link{i}"), TestLink::new(&next));
        }
        mount_root_with(&table, root);

        assert_eq!(table.resolve("/link0").err(), Some(Errno::ELOOP));
    }

    #[test]
    fn self_referencing_symlink_is_eloop_not_a_hang_or_overflow() {
        let table = MountTable::new();
        let root = TestDir::new();
        root.insert("self", TestLink::new("self"));
        mount_root_with(&table, root);

        assert_eq!(table.resolve("/self").err(), Some(Errno::ELOOP));
    }

    // ── open / mutations ─────────────────────────────────────────────────

    #[test]
    fn open_missing_path_without_creat_is_enoent() {
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new());
        assert_eq!(table.open("/nope", OpenFlags::RDONLY).err(), Some(Errno::ENOENT));
    }

    #[test]
    fn open_missing_path_with_creat_creates_it() {
        let table = MountTable::new();
        let root = TestDir::new();
        mount_root_with(&table, root.clone());
        assert!(table.open("/new.txt", OpenFlags(OpenFlags::CREAT.0)).is_err());
        // TestFile::open always errors (ENOSYS) in this toy fs, but the
        // *create* must still have happened — verify the effect directly.
        assert!(root.children.lock().contains_key("new.txt"));
    }

    #[test]
    fn open_with_creat_and_missing_parent_reports_parent_error_not_a_phantom_create() {
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new());
        // "/nope/new.txt": parent "/nope" doesn't exist.
        let err = table.open("/nope/new.txt", OpenFlags(OpenFlags::CREAT.0)).err();
        assert_eq!(err, Some(Errno::ENOENT));
    }

    #[test]
    fn mkdir_delegates_to_correct_parent() {
        let table = MountTable::new();
        let root = TestDir::new();
        mount_root_with(&table, root.clone());
        table.mkdir("/sub").expect("mkdir");
        let child = root.children.lock().get("sub").cloned().expect("sub exists");
        assert_eq!(child.file_type(), FileType::Directory);
    }

    #[test]
    fn symlink_syscall_delegates_to_correct_parent() {
        let table = MountTable::new();
        let root = TestDir::new();
        mount_root_with(&table, root.clone());
        table.symlink("target.txt", "/link").expect("symlink");
        let child = root.children.lock().get("link").cloned().expect("link exists");
        assert_eq!(child.file_type(), FileType::Symlink);
        assert_eq!(child.readlink().unwrap(), "target.txt");
    }

    #[test]
    fn unlink_delegates_to_correct_parent() {
        let table = MountTable::new();
        let root = TestDir::new().with("f", TestFile::new(b"x"));
        mount_root_with(&table, root.clone());
        table.unlink("/f").expect("unlink");
        assert!(!root.children.lock().contains_key("f"));
    }

    #[test]
    fn rmdir_delegates_to_correct_parent() {
        let table = MountTable::new();
        let root = TestDir::new().with("sub", TestDir::new() as Arc<dyn Inode>);
        mount_root_with(&table, root.clone());
        table.rmdir("/sub").expect("rmdir");
        assert!(!root.children.lock().contains_key("sub"));
    }

    #[test]
    fn rename_moves_the_node() {
        let table = MountTable::new();
        let root = TestDir::new().with("old.txt", TestFile::new(b"payload"));
        mount_root_with(&table, root.clone());
        table.rename("/old.txt", "/new.txt").expect("rename");
        assert!(!root.children.lock().contains_key("old.txt"));
        let moved = root.children.lock().get("new.txt").cloned().expect("new.txt exists");
        let file = moved.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"payload");
    }

    #[test]
    fn rename_failure_rolls_back_to_the_original_name() {
        // insert_child on the destination fails with EEXIST because the
        // name is already taken — the node must end up back under its
        // original name in the source directory, not lost.
        let table = MountTable::new();
        let root = TestDir::new()
            .with("old.txt", TestFile::new(b"payload"))
            .with("new.txt", TestFile::new(b"already-here"));
        mount_root_with(&table, root.clone());

        let err = table.rename("/old.txt", "/new.txt").err();
        assert_eq!(err, Some(Errno::EEXIST));

        // Rolled back: old.txt is back, still holding its own content.
        let restored = root.children.lock().get("old.txt").cloned().expect("old.txt restored");
        let file = restored.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"payload");
        // The pre-existing new.txt was untouched by the failed rename.
        let existing = root.children.lock().get("new.txt").cloned().expect("new.txt still there");
        let file2 = existing.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file2.content.lock()[..], b"already-here");
    }

    // ── direct_children ──────────────────────────────────────────────────

    #[test]
    fn direct_children_of_root_lists_every_top_level_mount() {
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new());
        table.mount("/dev", TestFs::new(TestDir::new()));
        table.mount("/tmp", TestFs::new(TestDir::new()));
        table.mount("/proc", TestFs::new(TestDir::new()));

        let mut children = table.direct_children("/");
        children.sort();
        assert_eq!(children, vec!["dev", "proc", "tmp"]);
    }

    #[test]
    fn direct_children_excludes_nested_two_level_mounts() {
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new());
        table.mount("/a", TestFs::new(TestDir::new()));
        table.mount("/a/b", TestFs::new(TestDir::new()));

        let children = table.direct_children("/");
        assert!(children.contains(&"a"));
        assert!(!children.contains(&"b"), "a/b must not appear as a direct child of /");
    }

    #[test]
    fn direct_children_of_a_non_root_prefix() {
        let table = MountTable::new();
        mount_root_with(&table, TestDir::new());
        table.mount("/dev", TestFs::new(TestDir::new()));
        table.mount("/dev/input", TestFs::new(TestDir::new()));

        assert_eq!(table.direct_children("/dev"), vec!["input"]);
    }

    // ── The lock-reentrancy invariant ────────────────────────────────────
    //
    // Reproduces the exact shape of the real bug this step's plan warns
    // about: `fs::initramfs::RootDirInode::lookup`/`::readdir` call back
    // into `vfs::direct_children("/")` — on the SAME MountTable — from
    // *inside* `resolve_inner`'s own walk. `spin::Mutex` is not reentrant,
    // so if any method here ever starts holding `entries` locked while
    // calling into `Inode`/`Filesystem`, this test HANGS (spins forever)
    // rather than failing cleanly.
    //
    // That hang is the point: it is the signal that someone broke the
    // invariant `find()`'s doc comment describes. Do NOT delete or
    // "fix" this test by making it tolerant of a hang (e.g. wrapping it in
    // a timeout) if it ever starts failing that way — a passing suite that
    // no longer catches this regression is worse than a red one that does.
    // If this test hangs, the fix is in `MountTable`, not in the test.

    struct CallbackDir {
        table: &'static MountTable,
    }

    impl Inode for CallbackDir {
        fn stat(&self) -> Stat {
            Stat::dir(1)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn lookup(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
            // The reentrant call: locks `self.table.entries` again, from
            // inside a `lookup()` that `resolve_inner` is calling while
            // walking a path resolved against this very table. Ignores
            // the result — the point is just to exercise the call and
            // prove it doesn't hang; unconditionally succeeding afterward
            // lets the test assert the whole `resolve()` completed too.
            let _children = self.table.direct_children("/");
            Ok(TestFile::new(b"found-via-callback") as Arc<dyn Inode>)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct CallbackFs {
        root: Arc<CallbackDir>,
    }

    impl Filesystem for CallbackFs {
        fn name(&self) -> &str {
            "callbackfs"
        }
        fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
            Ok(self.root.clone() as Arc<dyn Inode>)
        }
    }

    #[test]
    fn direct_children_callback_from_lookup_does_not_self_deadlock() {
        // `Box::leak` gives the callback inode a `&'static MountTable` to
        // call back into — mirrors the real kernel's `static MOUNTS`, only
        // scoped to this one test instead of the whole process.
        let table: &'static MountTable = Box::leak(Box::new(MountTable::new()));

        let callback_dir = Arc::new(CallbackDir { table });
        table.mount("/", Arc::new(CallbackFs { root: callback_dir }));
        // Something for direct_children("/") to actually find, so the
        // reentrant call inside `lookup()` isn't operating on an empty
        // table (closer to the real `ls /` shape, which sees "dev",
        // "tmp", "mnt", "proc").
        table.mount("/dev", TestFs::new(TestDir::new()));

        // "/probe" isn't itself a registered mount prefix, so it resolves
        // via the "/" mount (CallbackFs) and its root's `lookup("probe")`
        // — which is where the reentrant `direct_children("/")` call
        // happens. If `find()` (or anything else in `MountTable`) ever
        // holds `entries` locked while calling into `root()`/`lookup()`,
        // this call hangs right here instead of returning.
        let node = table.resolve("/probe").expect("resolves through the callback lookup");
        assert_eq!(node.file_type(), FileType::Regular);
    }

    // ── Mutating-op reentrancy probes (family A) ────────────────────────
    //
    // The template test above (`direct_children_callback_from_lookup_does_
    // not_self_deadlock`) only exercises `resolve()`'s read-only path via
    // `Inode::lookup`. `MountTable`'s mutating methods (`rename`, `mkdir`,
    // `symlink`, `unlink`, `rmdir`) each call `self.resolve(...)` to get a
    // parent `Inode` *and then* call one or more further `Inode` trait
    // methods (`take_child`/`insert_child`/`mkdir`/`symlink`/`unlink`/
    // `rmdir`) on the result — a second wave of calls into arbitrary
    // filesystem code that `find()`'s doc comment never explicitly
    // discusses, because at the time it was written none of those callers
    // existed yet. Every probe below is family A: reentrancy probes,
    // deterministic, no threads — a toy `Inode` reenters the very
    // `MountTable` it is mounted on, from inside one of these second-wave
    // calls. They model this kernel's single-core failure mode (the same
    // thread reentering a `spin::Mutex` it already holds), not SMP
    // contention — there is no second vCPU here to contend with. Per the
    // template test's own convention: if any of `MountTable`'s methods
    // ever starts holding `entries` locked across one of these calls, the
    // affected test HANGS rather than failing cleanly — that hang is the
    // signal, not a bug in the test.

    /// A toy directory whose every mutating operation first makes a
    /// reentrant call back into the same `MountTable` it is mounted on,
    /// before touching its own state. Models the real kernel shape where
    /// a directory op reaches back into VFS-global state mid-operation —
    /// `fs::initramfs::RootDirInode::lookup` calling `vfs::direct_children`
    /// from inside `resolve_inner`'s walk, or `fs::procfs` needing a fresh
    /// `SCHEDULER` lock of its own from inside a call the VFS is already
    /// in the middle of (see `sys_getdents64`'s doc comment in
    /// `kernel/src/process/syscall.rs` for that second example).
    struct ReentrantOpsDir {
        table: &'static MountTable,
        children: Mutex<BTreeMap<String, Arc<dyn Inode>>>,
    }

    impl ReentrantOpsDir {
        fn new(table: &'static MountTable) -> Arc<Self> {
            Arc::new(Self { table, children: Mutex::new(BTreeMap::new()) })
        }

        /// The reentrant call itself. Result ignored — the only thing
        /// under test is that it *returns* (rather than hanging) and
        /// that the assertion inside it holds, proving `entries` was
        /// actually unlocked at this point, not just that the call
        /// happened to not need the lock.
        fn probe(&self) {
            let children = self.table.direct_children("/");
            assert!(
                children.contains(&"dev"),
                "reentrant direct_children(\"/\") must still see /dev — proves \
                 the table lock was free, not just that this call got lucky"
            );
        }
    }

    impl Inode for ReentrantOpsDir {
        fn stat(&self) -> Stat {
            Stat::dir(1)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn mkdir(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
            self.probe();
            let node: Arc<dyn Inode> = TestDir::new();
            self.children.lock().insert(name.to_string(), node.clone());
            Ok(node)
        }
        fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn Inode>, Errno> {
            self.probe();
            let node: Arc<dyn Inode> = TestLink::new(target);
            self.children.lock().insert(name.to_string(), node.clone());
            Ok(node)
        }
        fn unlink(&self, name: &str) -> Result<(), Errno> {
            self.probe();
            let mut children = self.children.lock();
            match children.get(name) {
                Some(n) if n.file_type() == FileType::Directory => Err(Errno::EISDIR),
                Some(_) => { children.remove(name); Ok(()) }
                None => Err(Errno::ENOENT),
            }
        }
        fn rmdir(&self, name: &str) -> Result<(), Errno> {
            self.probe();
            let mut children = self.children.lock();
            match children.get(name) {
                Some(n) if n.file_type() != FileType::Directory => Err(Errno::ENOTDIR),
                Some(n) => {
                    let sub = n.as_any().downcast_ref::<TestDir>().expect("TestDir child");
                    if !sub.children.lock().is_empty() {
                        return Err(Errno::ENOTEMPTY);
                    }
                    children.remove(name);
                    Ok(())
                }
                None => Err(Errno::ENOENT),
            }
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct ReentrantOpsFs {
        root: Arc<ReentrantOpsDir>,
    }

    impl Filesystem for ReentrantOpsFs {
        fn name(&self) -> &str {
            "reentrant-ops-fs"
        }
        fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
            Ok(self.root.clone() as Arc<dyn Inode>)
        }
    }

    /// Family A reentrancy probe (deterministic, no threads).
    ///
    /// Models `MountTable::mkdir` calling `Inode::mkdir` on a parent
    /// directory that itself reaches back into the same `MountTable` mid-
    /// call (the `fs::initramfs`/`fs::procfs` shape described above the
    /// `ReentrantOpsDir` definition). Not SMP contention — this kernel is
    /// single-core; the risk modeled is the same thread reentering a
    /// `spin::Mutex` it already holds.
    ///
    /// If `MountTable::mkdir` (or `resolve`, which it calls first) is ever
    /// changed to hold `entries` locked across the `Inode::mkdir` call,
    /// this test HANGS instead of failing — that hang is the signal, same
    /// convention as the template test above.
    #[test]
    fn mkdir_reentrant_probe_does_not_self_deadlock() {
        let table: &'static MountTable = Box::leak(Box::new(MountTable::new()));
        let root = ReentrantOpsDir::new(table);
        table.mount("/", Arc::new(ReentrantOpsFs { root: root.clone() }));
        table.mount("/dev", TestFs::new(TestDir::new()));

        table.mkdir("/sub").expect("mkdir completes without self-deadlock");

        let child = root.children.lock().get("sub").cloned().expect("sub was created");
        assert_eq!(child.file_type(), FileType::Directory);
    }

    /// Family A reentrancy probe (deterministic, no threads). Same shape
    /// as `mkdir_reentrant_probe_does_not_self_deadlock`, for
    /// `MountTable::symlink` → `Inode::symlink`. Models single-core
    /// reentrancy, not SMP contention. Expected failure mode if the
    /// invariant is ever broken: a HANG, not a normal test failure.
    #[test]
    fn symlink_reentrant_probe_does_not_self_deadlock() {
        let table: &'static MountTable = Box::leak(Box::new(MountTable::new()));
        let root = ReentrantOpsDir::new(table);
        table.mount("/", Arc::new(ReentrantOpsFs { root: root.clone() }));
        table.mount("/dev", TestFs::new(TestDir::new()));

        table.symlink("target.txt", "/link").expect("symlink completes without self-deadlock");

        let child = root.children.lock().get("link").cloned().expect("link was created");
        assert_eq!(child.file_type(), FileType::Symlink);
        assert_eq!(child.readlink().unwrap(), "target.txt");
    }

    /// Family A reentrancy probe (deterministic, no threads). Same shape
    /// as the two probes above, for `MountTable::unlink` → `Inode::unlink`.
    /// Models single-core reentrancy (the same thread re-locking
    /// `MountTable::entries`), not SMP contention. Expected failure mode
    /// if the invariant is ever broken: a HANG, not a normal test failure.
    #[test]
    fn unlink_reentrant_probe_does_not_self_deadlock() {
        let table: &'static MountTable = Box::leak(Box::new(MountTable::new()));
        let root = ReentrantOpsDir::new(table);
        root.children.lock().insert("f".to_string(), TestFile::new(b"x") as Arc<dyn Inode>);
        table.mount("/", Arc::new(ReentrantOpsFs { root: root.clone() }));
        table.mount("/dev", TestFs::new(TestDir::new()));

        table.unlink("/f").expect("unlink completes without self-deadlock");

        assert!(!root.children.lock().contains_key("f"));
    }

    /// Family A reentrancy probe (deterministic, no threads). Same shape
    /// as the probes above, for `MountTable::rmdir` → `Inode::rmdir`.
    /// Models single-core reentrancy, not SMP contention. Expected failure
    /// mode if the invariant is ever broken: a HANG, not a normal test
    /// failure.
    #[test]
    fn rmdir_reentrant_probe_does_not_self_deadlock() {
        let table: &'static MountTable = Box::leak(Box::new(MountTable::new()));
        let root = ReentrantOpsDir::new(table);
        root.children.lock().insert("sub".to_string(), TestDir::new() as Arc<dyn Inode>);
        table.mount("/", Arc::new(ReentrantOpsFs { root: root.clone() }));
        table.mount("/dev", TestFs::new(TestDir::new()));

        table.rmdir("/sub").expect("rmdir completes without self-deadlock");

        assert!(!root.children.lock().contains_key("sub"));
    }

    // ── rename: reentrancy + rollback probes (family A) ─────────────────

    /// A toy directory used only for `rename` reentrancy probes: like
    /// `ReentrantOpsDir`, its `take_child`/`insert_child` each make a
    /// reentrant call back into the same `MountTable` before touching
    /// their own state — modeling `MountTable::rename`'s two calls into
    /// arbitrary `Inode` code (the "detach" and "attach" halves) reaching
    /// back into VFS-global state mid-rename.
    struct ReentrantRenameDir {
        table: &'static MountTable,
        children: Mutex<BTreeMap<String, Arc<dyn Inode>>>,
    }

    impl ReentrantRenameDir {
        fn new(table: &'static MountTable) -> Arc<Self> {
            Arc::new(Self { table, children: Mutex::new(BTreeMap::new()) })
        }
    }

    impl Inode for ReentrantRenameDir {
        fn stat(&self) -> Stat {
            Stat::dir(1)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn take_child(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
            // Reentrant call from the "detach" half of rename.
            let children = self.table.direct_children("/");
            assert!(children.contains(&"dev"), "reentrant call must see /dev");
            self.children.lock().remove(name).ok_or(Errno::ENOENT)
        }
        fn insert_child(&self, name: &str, node: Arc<dyn Inode>) -> Result<(), Errno> {
            // Reentrant call from the "attach" half of rename.
            let children = self.table.direct_children("/");
            assert!(children.contains(&"dev"), "reentrant call must see /dev");
            let mut children = self.children.lock();
            if children.contains_key(name) {
                return Err(Errno::EEXIST);
            }
            children.insert(name.to_string(), node);
            Ok(())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct ReentrantRenameFs {
        root: Arc<ReentrantRenameDir>,
    }

    impl Filesystem for ReentrantRenameFs {
        fn name(&self) -> &str {
            "reentrant-rename-fs"
        }
        fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
            Ok(self.root.clone() as Arc<dyn Inode>)
        }
    }

    /// Family A reentrancy probe (deterministic, no threads).
    ///
    /// `MountTable::rename` calls `old_parent.take_child(...)` and then
    /// `new_parent.insert_child(...)` — here both are the *same*
    /// `ReentrantRenameDir`, each of whose implementations reenters the
    /// table via `direct_children("/")` before doing anything else. Models
    /// this kernel's single-core reentrancy failure mode (the same thread
    /// re-locking `MountTable::entries`), not SMP contention — by the time
    /// `rename` calls `take_child`/`insert_child`, `resolve()`'s own
    /// internal `find()` has already locked-and-dropped the table guard
    /// (see `find()`'s doc comment) — this probe exists to keep it that
    /// way as `rename` itself evolves.
    ///
    /// If `rename` (or `resolve`) is ever changed to hold `entries` locked
    /// across either call, this test HANGS instead of failing — that hang
    /// is the signal, same convention as the template test.
    #[test]
    fn rename_reentrant_take_child_and_insert_child_does_not_self_deadlock() {
        let table: &'static MountTable = Box::leak(Box::new(MountTable::new()));
        let root = ReentrantRenameDir::new(table);
        root.children.lock().insert("old.txt".to_string(), TestFile::new(b"payload") as Arc<dyn Inode>);
        table.mount("/", Arc::new(ReentrantRenameFs { root: root.clone() }));
        table.mount("/dev", TestFs::new(TestDir::new()));

        table.rename("/old.txt", "/new.txt").expect("rename completes without self-deadlock");

        let moved = root.children.lock().get("new.txt").cloned().expect("new.txt present");
        let file = moved.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"payload");
    }

    /// A directory whose `insert_child` always fails — regardless of
    /// whether the name is already taken — modeling a filesystem-level
    /// insert failure unrelated to `EEXIST` (e.g. ext2 running out of
    /// directory blocks/inodes mid-rename). Used to probe `rename`'s
    /// rollback path independently of the pre-existing
    /// `rename_failure_rolls_back_to_the_original_name` test above, which
    /// only exercises the `EEXIST` failure — this makes sure the rollback
    /// isn't accidentally coupled to that one specific error.
    struct AlwaysFailInsertDir;

    impl Inode for AlwaysFailInsertDir {
        fn stat(&self) -> Stat {
            Stat::dir(1)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn insert_child(&self, _name: &str, _node: Arc<dyn Inode>) -> Result<(), Errno> {
            Err(Errno::EIO)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct AlwaysFailInsertFs {
        root: Arc<AlwaysFailInsertDir>,
    }

    impl Filesystem for AlwaysFailInsertFs {
        fn name(&self) -> &str {
            "always-fail-insert-fs"
        }
        fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
            Ok(self.root.clone() as Arc<dyn Inode>)
        }
    }

    /// Family A probe — not a reentrancy probe by itself (no callback into
    /// `MountTable`), but the rollback counterpart the task asks for
    /// alongside the reentrancy probes above: a cross-directory rename
    /// whose destination `insert_child` fails for a reason that has
    /// nothing to do with `EEXIST`. Asserts on observable state (the file
    /// is back under its original name, holding its original content),
    /// not on the returned `Errno` — a rollback that silently drops the
    /// node would still return the same `Err(EIO)` here, so only checking
    /// the error value would miss exactly the bug this probe is for.
    #[test]
    fn rename_rollback_restores_source_when_destination_insert_always_fails() {
        let table = MountTable::new();
        let old_root = TestDir::new().with("old.txt", TestFile::new(b"payload"));
        table.mount("/olddir", TestFs::new(old_root.clone()));
        table.mount("/newdir", Arc::new(AlwaysFailInsertFs { root: Arc::new(AlwaysFailInsertDir) }));

        let err = table.rename("/olddir/old.txt", "/newdir/new.txt").err();
        assert!(err.is_some(), "rename must fail since the destination always rejects the insert");

        let restored = old_root.children.lock().get("old.txt").cloned()
            .expect("old.txt must be restored by rollback, not lost");
        let file = restored.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"payload");
    }

    /// A directory combining both toy behaviors above: its `insert_child`
    /// reentrantly calls back into the same `MountTable` *and* always
    /// fails afterward. Models the bonus case the task calls out
    /// explicitly: a rollback path that is both reentrant-unsafe and
    /// buggy at once — exactly where a badly-written rollback is most
    /// likely to either lose the file or hang.
    struct ReentrantAlwaysFailInsertDir {
        table: &'static MountTable,
    }

    impl Inode for ReentrantAlwaysFailInsertDir {
        fn stat(&self) -> Stat {
            Stat::dir(1)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn insert_child(&self, _name: &str, _node: Arc<dyn Inode>) -> Result<(), Errno> {
            let children = self.table.direct_children("/");
            assert!(children.contains(&"dev"), "reentrant call must see /dev");
            Err(Errno::EIO)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct ReentrantAlwaysFailInsertFs {
        root: Arc<ReentrantAlwaysFailInsertDir>,
    }

    impl Filesystem for ReentrantAlwaysFailInsertFs {
        fn name(&self) -> &str {
            "reentrant-always-fail-insert-fs"
        }
        fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
            Ok(self.root.clone() as Arc<dyn Inode>)
        }
    }

    /// Family A reentrancy + rollback probe combined (deterministic, no
    /// threads) — the bonus case. Models single-core reentrancy, not SMP
    /// contention. Two things must both hold, and the doc comments on the
    /// two probes above explain why each matters on its own:
    /// - No self-deadlock: if `rename`'s rollback call ever holds
    ///   `entries` locked across `insert_child`, this test HANGS instead
    ///   of failing — that hang is the signal.
    /// - No lost file: the rollback must still restore `old.txt` to its
    ///   original directory even though the failing `insert_child` also
    ///   made a reentrant call on its way to failing.
    #[test]
    fn rename_rollback_with_reentrant_failing_insert_child_restores_source_without_hanging() {
        let table: &'static MountTable = Box::leak(Box::new(MountTable::new()));
        let old_root = TestDir::new().with("old.txt", TestFile::new(b"payload"));
        table.mount("/olddir", TestFs::new(old_root.clone()));
        table.mount(
            "/newdir",
            Arc::new(ReentrantAlwaysFailInsertFs { root: Arc::new(ReentrantAlwaysFailInsertDir { table }) }),
        );
        table.mount("/dev", TestFs::new(TestDir::new()));

        let err = table.rename("/olddir/old.txt", "/newdir/new.txt").err();
        assert!(err.is_some(), "rename must fail since the destination always rejects the insert");

        let restored = old_root.children.lock().get("old.txt").cloned()
            .expect("old.txt must be restored by rollback, not lost");
        let file = restored.as_any().downcast_ref::<TestFile>().expect("TestFile");
        assert_eq!(&file.content.lock()[..], b"payload");
    }
}
