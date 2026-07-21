extern crate ovmf_prebuilt;

fn main() {
    // read env variables that were set in build script
    let uefi_path = env!("UEFI_PATH");
    let ovmf_code = env!("OVMF_CODE");
    let ovmf_vars = env!("OVMF_VARS");
    let ext2_disk_path = env!("EXT2_DISK_PATH");

    // choose whether to start the UEFI or BIOS image

    let mut cmd = std::process::Command::new("qemu-system-x86_64");
        // UEFI configuration with proper OVMF setup
        cmd.arg("-drive")
           .arg(format!("if=pflash,format=raw,readonly=on,file={}", ovmf_code));
        cmd.arg("-drive")
           .arg(format!("if=pflash,format=raw,file={}", ovmf_vars));
        cmd.arg("-drive")
           .arg(format!("format=raw,file={}", uefi_path));

    // ext2 disk (kernel::fs::ext2, mounted read-only at /mnt). Attached
    // explicitly to the secondary IDE channel (`ide.1`) via `-device`,
    // rather than a plain `-drive`, so it can never land on whatever
    // channel/slot the UEFI boot drive above ends up on — the kernel's ATA
    // driver (kernel/src/block/ata.rs) only ever looks at the secondary
    // channel's fixed ports (0x170/0x376), no PCI/bus enumeration needed.
    if std::path::Path::new(ext2_disk_path).exists() {
        cmd.arg("-drive")
           .arg(format!("file={},format=raw,if=none,id=ext2disk", ext2_disk_path));
        cmd.arg("-device").arg("ide-hd,drive=ext2disk,bus=ide.1");
    }

    // Add some useful QEMU options
    cmd.arg("-m").arg("512M");  // 512MB RAM
    cmd.arg("-serial").arg("stdio");  // Serial output to terminal

    // Without this, QEMU falls back to its conservative default CPU
    // model, which lacks features (e.g. FSGSBASE) that the bootloader
    // crate's UEFI stage uses unconditionally — that mismatch triggers a
    // #UD (invalid opcode) fault in OVMF before our kernel ever loads.
    cmd.arg("-cpu").arg("max");

    // AC97 sound card (kernel/src/ac97.rs) — routed through PipeWire
    // (qemu-audio-pipewire package) so DOOM/Quake's sound effects actually
    // reach real speakers, not just a `wav` capture file. Falls back to
    // "none" (silently discards the PCM, guest-visible device/DMA still
    // work) if that QEMU audio backend isn't installed — e.g. a fresh
    // clone that hasn't run `sudo pacman -S qemu-audio-pipewire` yet.
    let audiodev = if qemu_has_audiodev("pipewire") { "pipewire" } else { "none" };
    cmd.arg("-audiodev").arg(format!("{},id=snd0", audiodev));
    cmd.arg("-device").arg("AC97,audiodev=snd0");

    let mut child = cmd.spawn().unwrap();
    child.wait().unwrap();
}

/// Checks `qemu-system-x86_64 -audiodev help`'s output for a named backend
/// (e.g. "pipewire") — QEMU audio backends are separate, optional distro
/// packages (Arch: `qemu-audio-pipewire`, `qemu-audio-pa`, etc.), so a
/// backend requested but not installed would otherwise fail at spawn time
/// with an opaque QEMU error instead of falling back cleanly.
fn qemu_has_audiodev(name: &str) -> bool {
    let Ok(output) = std::process::Command::new("qemu-system-x86_64")
        .arg("-audiodev")
        .arg("help")
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == name)
}