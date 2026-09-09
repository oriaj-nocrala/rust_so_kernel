// vfs/src/path.rs
//
// Pure, filesystem-independent path string manipulation used by the mount
// table (`crate::mount`) — normalizing a possibly-relative path against a
// cwd, and splitting a path into (parent, leaf) for the mutation syscalls
// (`mkdir`/`symlink`/`unlink`/`rmdir`/create-on-open).

use crate::types::Errno;
use alloc::string::String;
use alloc::vec::Vec;

/// Turn a possibly-relative `path` into a clean, normalized absolute path,
/// resolving `.`/`..` components lexically against `cwd` (or against `path`
/// itself if it's already absolute — `/a/b/../c` still needs collapsing,
/// since `resolve()`'s own `..` handling is a no-op placeholder).
///
/// Purely string-based: doesn't touch the filesystem, so it can't tell
/// `../` past `/` from `../` past a real directory — both just get dropped,
/// matching how a shell's `..` behaves at the true root.
pub fn normalize_path(cwd: &str, path: &str) -> String {
    let mut stack: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        cwd.split('/').filter(|s| !s.is_empty()).collect()
    };

    for component in path.split('/').filter(|s| !s.is_empty()) {
        match component {
            "."  => {}
            ".." => { stack.pop(); }
            name => stack.push(name),
        }
    }

    if stack.is_empty() {
        String::from("/")
    } else {
        let mut out = String::from("/");
        out.push_str(&stack.join("/"));
        out
    }
}

/// Split `path` into (parent directory path, leaf component name).
///
/// `"/tmp/sub/file"` → `("/tmp/sub", "file")`; `"/file"` → `("/", "file")`.
///
/// `pub(crate)`, not `pub`: only `crate::mount` needs this (every mutation
/// syscall — `mkdir`/`symlink`/`unlink`/`rmdir`/create-on-open — routes
/// through `MountTable`'s own methods, never calls this directly), so the
/// public API surface stays exactly what it was before this crate existed.
pub(crate) fn split_parent(path: &str) -> Result<(&str, &str), Errno> {
    let idx = path.rfind('/').ok_or(Errno::EINVAL)?;
    let leaf = &path[idx + 1..];
    if leaf.is_empty() {
        return Err(Errno::EINVAL);
    }
    let dir_path = if idx == 0 { "/" } else { &path[..idx] };
    Ok((dir_path, leaf))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_path ───────────────────────────────────────────────────

    #[test]
    fn normalize_absolute_path_is_left_alone() {
        assert_eq!(normalize_path("/wherever", "/a/b/c"), "/a/b/c");
    }

    #[test]
    fn normalize_relative_path_resolves_against_cwd() {
        assert_eq!(normalize_path("/a/b", "c/d"), "/a/b/c/d");
    }

    #[test]
    fn normalize_dot_component_is_dropped() {
        assert_eq!(normalize_path("/", "/a/./b"), "/a/b");
    }

    #[test]
    fn normalize_dotdot_pops_previous_component() {
        assert_eq!(normalize_path("/", "/a/b/../c"), "/a/c");
    }

    #[test]
    fn normalize_relative_dotdot_walks_up_from_cwd() {
        assert_eq!(normalize_path("/a/b/c", "../../d"), "/a/d");
    }

    #[test]
    fn normalize_extra_dotdot_at_root_is_dropped_without_error() {
        // Purely string-based: can't distinguish "past /" from "past a
        // real directory" — both just no-op the pop on an empty stack.
        assert_eq!(normalize_path("/", "/../../etc"), "/etc");
        assert_eq!(normalize_path("/", "/.."), "/");
    }

    #[test]
    fn normalize_result_collapsing_to_nothing_is_root() {
        assert_eq!(normalize_path("/", "/"), "/");
        assert_eq!(normalize_path("/", "/a/.."), "/");
        assert_eq!(normalize_path("/a", "."), "/a");
    }

    #[test]
    fn normalize_double_and_trailing_slashes_are_cleaned() {
        assert_eq!(normalize_path("/", "/a//b///c/"), "/a/b/c");
        assert_eq!(normalize_path("/", "/a/"), "/a");
    }

    // ── split_parent ─────────────────────────────────────────────────────

    #[test]
    fn split_parent_nested_path() {
        assert_eq!(split_parent("/tmp/sub/file"), Ok(("/tmp/sub", "file")));
    }

    #[test]
    fn split_parent_top_level_path() {
        assert_eq!(split_parent("/file"), Ok(("/", "file")));
    }

    #[test]
    fn split_parent_trailing_slash_is_einval() {
        // The "leaf" would be empty — no valid file/dir name there.
        assert_eq!(split_parent("/tmp/sub/"), Err(Errno::EINVAL));
    }

    #[test]
    fn split_parent_no_slash_at_all_is_einval() {
        assert_eq!(split_parent("relative"), Err(Errno::EINVAL));
    }
}
