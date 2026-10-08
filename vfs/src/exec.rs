//! What `execve` does with a file's first bytes before the ELF loader sees it: a `#!` script names the program that runs it
//! (Linux's `binfmt_script`). Pure, so the rules are host-tested; the kernel (`process::syscall::process_ctl::exec_from`) reads
//! the files and loops.

use alloc::string::String;
use alloc::vec::Vec;

/// How much of a script's first line counts (Linux's `BINPRM_BUF_SIZE`).
pub const SHEBANG_MAX: usize = 256;
/// How many scripts may name another script before the real program (Linux allows a few levels too).
pub const MAX_SCRIPT_DEPTH: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum Image {
    /// An ELF file: the loader's.
    Elf,
    /// `#!interpreter [arg]`: run `interpreter` with `[interpreter, arg?, script, argv[1..]]`.
    Script { interpreter: String, arg: Option<String> },
    /// Neither: `ENOEXEC` (a shell then runs it itself, as POSIX says).
    Unknown,
}

/// The kind of program `head` (the file's first bytes) starts. A `#!` line with no interpreter, or one whose interpreter does
/// not end within [`SHEBANG_MAX`] bytes, is `Unknown`. Everything after the interpreter up to the end of the line, without
/// the blanks around it, is one argument, as on Linux.
pub fn classify(head: &[u8]) -> Image {
    if head.starts_with(b"\x7fELF") {
        return Image::Elf;
    }
    let Some(rest) = head.strip_prefix(b"#!") else { return Image::Unknown };
    let line_max = &rest[..rest.len().min(SHEBANG_MAX - 2)];
    let (line, complete) = match line_max.iter().position(|&b| b == b'\n') {
        Some(n) => (&line_max[..n], true),
        None => (line_max, rest.len() <= SHEBANG_MAX - 2),
    };
    let blank = |b: &u8| *b == b' ' || *b == b'\t';
    let line = &line[line.iter().position(|b| !blank(b)).unwrap_or(line.len())..];
    let end = line.iter().position(|b| blank(b) || *b == 0).unwrap_or(line.len());
    let (interp, after) = line.split_at(end);
    // an interpreter cut by the buffer's end is not the one meant
    if interp.is_empty() || (!complete && after.is_empty()) {
        return Image::Unknown;
    }
    let after = after.split(|&b| b == 0).next().unwrap_or(b"");
    let arg: Vec<u8> = {
        let s = after.iter().position(|b| !blank(b)).unwrap_or(after.len());
        let e = after.iter().rposition(|b| !blank(b)).map_or(s, |i| i + 1);
        after[s..e.max(s)].to_vec()
    };
    let Ok(interpreter) = core::str::from_utf8(interp) else { return Image::Unknown };
    Image::Script {
        interpreter: interpreter.into(),
        arg: if arg.is_empty() { None } else { Some(String::from_utf8_lossy(&arg).into_owned()) },
    }
}

/// The interpreter's argv: `[interpreter, arg?, script, old_argv[1..]]` (the old `argv[0]` is dropped, as on Linux).
pub fn script_argv(interpreter: &str, arg: Option<&str>, script: &str, old_argv: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let z = |s: &str| {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    };
    let mut v = alloc::vec![z(interpreter)];
    if let Some(a) = arg {
        v.push(z(a));
    }
    v.push(z(script));
    v.extend(old_argv.iter().skip(1).cloned());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(i: &str, a: Option<&str>) -> Image {
        Image::Script { interpreter: i.into(), arg: a.map(String::from) }
    }

    #[test]
    fn elf_scripts_and_the_rest() {
        assert_eq!(classify(b"\x7fELF\x02\x01"), Image::Elf);
        assert_eq!(classify(b"#!/bin/sh\necho hi\n"), script("/bin/sh", None));
        assert_eq!(classify(b"#! /bin/sh\n"), script("/bin/sh", None));
        assert_eq!(classify(b"#!\t/usr/bin/env  python3 -u \n"), script("/usr/bin/env", Some("python3 -u")));
        assert_eq!(classify(b"#!/bin/sh"), script("/bin/sh", None), "no newline, but the whole file was read");
        assert_eq!(classify(b"#!/bin/sh -e\r\n"), script("/bin/sh", Some("-e\r")), "a CR is part of the argument, as on Linux");
        assert_eq!(classify(b"echo hi\n"), Image::Unknown);
        assert_eq!(classify(b""), Image::Unknown);
        assert_eq!(classify(b"#!\n"), Image::Unknown);
        assert_eq!(classify(b"#!   \n"), Image::Unknown);
        assert_eq!(classify(b"#!/bin/\xffsh\n"), Image::Unknown);
    }

    #[test]
    fn the_first_line_counts_up_to_256_bytes() {
        // an interpreter that ends inside the buffer, with an argument cut by it: kept, the argument truncated
        let mut long_arg = b"#!/bin/sh ".to_vec();
        long_arg.extend(core::iter::repeat_n(b'a', 400));
        long_arg.push(b'\n');
        match classify(&long_arg) {
            Image::Script { interpreter, arg: Some(a) } => {
                assert_eq!(interpreter, "/bin/sh");
                assert_eq!(a.len(), SHEBANG_MAX - 2 - "/bin/sh ".len());
            }
            other => panic!("{:?}", other),
        }
        // an interpreter longer than the buffer: refused rather than run cut
        let mut long_interp = b"#!/".to_vec();
        long_interp.extend(core::iter::repeat_n(b'x', 400));
        assert_eq!(classify(&long_interp), Image::Unknown);
    }

    #[test]
    fn the_interpreter_gets_the_script_and_the_old_arguments() {
        let old = alloc::vec![b"hi.sh\0".to_vec(), b"one\0".to_vec(), b"two\0".to_vec()];
        let v = script_argv("/bin/sh", Some("-e"), "/tmp/hi.sh", &old);
        let strs: Vec<&[u8]> = v.iter().map(|a| a.as_slice()).collect();
        assert_eq!(strs, [&b"/bin/sh\0"[..], b"-e\0", b"/tmp/hi.sh\0", b"one\0", b"two\0"]);
        let v = script_argv("/bin/sh", None, "/tmp/hi.sh", &[]);
        assert_eq!(v.len(), 2);
    }
}
