//! The preview provider: makes a file's preview for `files` (`files::preview`). It is started through `cap-exec`, in capability mode,
//! with exactly two descriptors besides stdio: the file (fd 3, read-only) and the output memfd (fd 4, write-only, already sized):
//!
//!     cap-exec --fd 3:read+seek+fstat --fd 4:write+seek+fstat -- files-preview MAX_W MAX_H
//!
//! It can open nothing else (P6.4): a bug or a hostile file in a decoder reaches only this process. Exit 0 with the answer written;
//! anything else is reported by `files` from the exit status and stderr.
//!
//! For `scripts/gui-e2e.sh files`, `FILES_PREVIEW_TEST` can ask for `sleep:MS` (hang that long first) or `escape` (try to open
//! `/mnt/etc/gui/apps`, say what happened on stderr, then go on).

use std::fs::File;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileExt;
use std::process::ExitCode;

use files::preview::{self, MAX_FILE, MAX_TEXT};

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let dim = |i: usize| args.get(i).and_then(|a| a.parse::<usize>().ok()).filter(|&n| (1..=4096).contains(&n));
    let (Some(max_w), Some(max_h)) = (dim(1), dim(2)) else {
        return Err("usage: files-preview MAX_W MAX_H (the file on fd 3, the output on fd 4)".into());
    };
    match std::env::var("FILES_PREVIEW_TEST").as_deref() {
        Ok(t) if t.starts_with("sleep:") => {
            let ms = t[6..].parse().unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        Ok("escape") => match std::fs::read("/mnt/etc/gui/apps") {
            Ok(b) => eprintln!("files-preview: escape test: read /mnt/etc/gui/apps ({} bytes): NOT CONFINED", b.len()),
            Err(e) => eprintln!("files-preview: escape test: open /mnt/etc/gui/apps: {}", e),
        },
        _ => {}
    }
    // SAFETY: cap-exec put these two descriptors there for us; nothing else owns them.
    let (mut file, out) = unsafe { (File::from_raw_fd(3), File::from_raw_fd(4)) };
    let len = file.metadata().map_err(|e| format!("fstat of the file (fd 3): {}", e))?.len();
    let mut head = Vec::new();
    (&mut file).take(MAX_TEXT as u64).read_to_end(&mut head).map_err(|e| format!("reading the file: {}", e))?;
    if head.starts_with(b"\x89PNG") && len <= MAX_FILE as u64 {
        file.read_to_end(&mut head).map_err(|e| format!("reading the file: {}", e))?;
    }
    let p = preview::make(&head, len, max_w, max_h);
    let bytes = preview::encode(&p);
    out.write_all_at(&bytes, 0).map_err(|e| format!("writing the answer (fd 4, {} bytes): {}", bytes.len(), e))?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("files-preview: {}", e);
            ExitCode::FAILURE
        }
    }
}
