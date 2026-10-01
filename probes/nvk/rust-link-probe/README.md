# Rust (std, musl) in the same static executable as NVK

Question (2026-09-30): can the GPU compositor's machine be Rust, with the Vulkan renderer in C, linked into one executable with NVK?
Answer: yes, measured on the host and on constanos in QEMU.

- `cargo build --release --target x86_64-unknown-linux-musl` here makes a **staticlib with std** (threads, `println!`, collections all used).
- Link it with `crender.o` (a C function the Rust side calls) and the stubs, with `build.py`'s own link line (`clang++ -static`, the whole of NVK,
  `--gc-sections`): no duplicate symbols with NVK's Rust staticlibs (`libnak_rs.a`), no C `main` needed (Rust exports `main`).
- A Rust-owned `main` does not get `std::env::args()` (musl has no `.init_array` capture): use `main`'s own `argc`/`argv`.
- `text` (parley + swash) and `gui` build for `x86_64-unknown-linux-musl` too.
- The 87 MB executable (15 MB stripped) ran in QEMU (`threads`, `println!`, NVK's `vk_icdGetInstanceProcAddr`).
