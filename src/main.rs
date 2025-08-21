extern crate ovmf_prebuilt;

fn main() {
    // read env variables that were set in build script
    let uefi_path = env!("UEFI_PATH");
    let bios_path = env!("BIOS_PATH");
    let ovmf_code = env!("OVMF_CODE");
    let ovmf_vars = env!("OVMF_VARS");
    
    // choose whether to start the UEFI or BIOS image
    let uefi = true;

    let mut cmd = std::process::Command::new("qemu-system-x86_64");
    if uefi {
        // UEFI configuration with proper OVMF setup
        cmd.arg("-drive")
           .arg(format!("if=pflash,format=raw,readonly=on,file={}", ovmf_code));
        cmd.arg("-drive")
           .arg(format!("if=pflash,format=raw,file={}", ovmf_vars));
        cmd.arg("-drive")
           .arg(format!("format=raw,file={}", uefi_path));
    } else {
        cmd.arg("-drive").arg(format!("format=raw,file={}", bios_path));
    }
    
    // Add some useful QEMU options
    cmd.arg("-m").arg("256M");  // 256MB RAM
    cmd.arg("-serial").arg("stdio");  // Serial output to terminal
    
    let mut child = cmd.spawn().unwrap();
    child.wait().unwrap();
}