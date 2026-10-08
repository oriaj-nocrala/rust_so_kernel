//! Whether a file is a program that Enter or a double click runs, as the panel's launcher does: a regular file with an `x` bit whose
//! first bytes are an ELF header or a `#!` line. Anything else is opened by its extension (`open_with`) or not at all.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Program {
    Elf,
    /// A `#!` script, with the interpreter's path from its first line.
    Script { interpreter: String },
}

/// How many bytes of the file [`detect`] wants: the ELF magic, or a `#!` line up to its end.
pub const HEAD: usize = 128;

/// `mode` is `st_mode`, `head` the file's first bytes (at most [`HEAD`] are looked at).
pub fn detect(mode: u32, head: &[u8]) -> Option<Program> {
    if mode & 0o170_000 != 0o100_000 || mode & 0o111 == 0 {
        return None;
    }
    if head.starts_with(b"\x7fELF") {
        return Some(Program::Elf);
    }
    let line = head.strip_prefix(b"#!")?;
    let line = &line[..line.iter().position(|&b| b == b'\n').unwrap_or(line.len())];
    let line = std::str::from_utf8(line).ok()?;
    let interpreter = line.split_whitespace().next()?;
    Some(Program::Script { interpreter: interpreter.into() })
}

/// The file names of the programs that open a window, from command lines (the launcher's and the open-with table's): each
/// command's first word, without its directory.
pub fn windowed<'a>(commands: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut v: Vec<String> = commands
        .filter_map(|c| c.split_whitespace().next())
        .map(|w| w.rsplit('/').next().unwrap_or(w).to_string())
        .filter(|w| !w.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}
