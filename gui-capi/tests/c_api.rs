//! The C API (`include/gui_capi.h`) driven from C, as `vk_comp.c` drives it: `tests/c_test.c` is compiled with the host's `cc` and linked
//! with this crate's static library. A missing compiler fails the test rather than skipping it.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn the_c_program_drives_the_window_manager() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // the static library is a cargo artifact of its own (`cargo test` builds only the test harness and the rlib)
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = Command::new(&cargo)
        .args(["build", "--lib"])
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", root.join("target/capi-host"))
        .status()
        .expect("cargo build");
    assert!(st.success(), "the static library does not build");
    let lib = root.join("target/capi-host/debug/libgui_capi.a");
    let exe = std::env::temp_dir().join(format!("gui-capi-c-{}", std::process::id()));
    let st = Command::new("cc")
        .args(["-std=gnu11", "-Wall", "-Werror", "-Wno-unused-function", "-O1"])
        .arg("-I")
        .arg(root.join("include"))
        .arg("-I")
        .arg(root.join("../userspace/c/include"))
        .arg(root.join("tests/c_test.c"))
        .arg(&lib)
        .args(["-lpthread", "-ldl", "-lm"])
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("cc is needed to test the C API");
    assert!(st.success(), "the C test does not compile or link");
    let out = Command::new(&exe).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "the C test failed:\n{}", text.lines().filter(|l| l.starts_with("FAIL")).collect::<Vec<_>>().join("\n"));
    assert!(text.contains("gui_capi: DONE"), "{}", text);
}
