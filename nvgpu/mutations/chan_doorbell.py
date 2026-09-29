# Example for scripts/gpu-mutate.py: the device table and the doorbell token (boots #103-#105).
#   scripts/gpu-mutate.py nvgpu/src/chan.rs chan:: nvgpu/mutations/chan_doorbell.py
M = [
    ("pub const ENGINE_COPY2: u32 = 0xb;", "pub const ENGINE_COPY2: u32 = 0x9;"),
    ("pub const CTRL_FIFO_GET_DEVICE_INFO_TABLE: u32 = 0x2080_1112;", "pub const CTRL_FIFO_GET_DEVICE_INFO_TABLE: u32 = 0x2080_1113;"),
    ("pub const DEVICE_INFO_PARAMS_SIZE: usize = 12 + 32 * DEVICE_ENTRY_SIZE;", "pub const DEVICE_INFO_PARAMS_SIZE: usize = 8 + 32 * DEVICE_ENTRY_SIZE;"),
    ("const DEVICE_ENTRY_SIZE: usize = 100;", "const DEVICE_ENTRY_SIZE: usize = 96;"),
    ("const ENGINE_INFO_RM_ENGINE_TYPE: usize = 2;", "const ENGINE_INFO_RM_ENGINE_TYPE: usize = 1;"),
    ("const ENGINE_INFO_RUNLIST: usize = 3;", "const ENGINE_INFO_RUNLIST: usize = 2;"),
    ("if params.len() < DEVICE_INFO_PARAMS_SIZE {", "if params.len() < DEVICE_INFO_PARAMS_SIZE - 1 {"),
    ("let n = (get32(params, 4) as usize).min(32);", "let n = (get32(params, 4) as usize).min(33);"),
    ("let n = (get32(params, 4) as usize).min(32);", "let n = (get32(params, 4) as usize).min(6);"),
    ("let e = 12 + i * DEVICE_ENTRY_SIZE;", "let e = 8 + i * DEVICE_ENTRY_SIZE;"),
    ("(runlist << 16) | chid", "(runlist << 15) | chid"),
    ("(runlist << 16) | chid", "(chid << 16) | runlist"),
]
