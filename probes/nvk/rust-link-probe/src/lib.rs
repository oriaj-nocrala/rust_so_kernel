use std::ffi::{c_char, c_int, c_void};
extern "C" {
    fn c_render(n: c_int) -> c_int;                                   // C side (the renderer would be here)
    fn vk_icdGetInstanceProcAddr(i: *mut c_void, name: *const c_char) -> *mut c_void;
}
// Rust owns main: no C main in the link.
#[no_mangle]
pub extern "C" fn main(argc: c_int, _argv: *const *const c_char) -> c_int {
    let args: Vec<String> = std::env::args().collect();
    let p = unsafe { vk_icdGetInstanceProcAddr(std::ptr::null_mut(), c"vkCreateInstance".as_ptr()) };
    let r = unsafe { c_render(argc) };
    println!("rust main: argc {} (std sees {}), NVK entry point {}, c_render -> {}", argc, args.len(), if p.is_null() { "missing" } else { "found" }, r);
    0
}
