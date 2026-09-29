# Phase 6d: 2D copies and the completion interrupt in nvgpu::chan.
#   scripts/gpu-mutate.py nvgpu/src/chan.rs chan:: nvgpu/mutations/chan_rect_irq.py
M = [
    ("pub const LAUNCH_DMA_MULTI_LINE: u32 = 1 << 9;", "pub const LAUNCH_DMA_MULTI_LINE: u32 = 1 << 10;"),
    ("pub const LAUNCH_DMA_INTERRUPT_NON_BLOCKING: u32 = 2 << 5;", "pub const LAUNCH_DMA_INTERRUPT_NON_BLOCKING: u32 = 1 << 5;"),
    ("pub const LAUNCH_DMA_INTERRUPT_NON_BLOCKING: u32 = 2 << 5;", "pub const LAUNCH_DMA_INTERRUPT_NON_BLOCKING: u32 = 2 << 6;"),
    ("    push[n - 1] |= LAUNCH_DMA_INTERRUPT_NON_BLOCKING;", "    push[n - 1] = LAUNCH_DMA_INTERRUPT_NON_BLOCKING;"),
    ("    push[n - 1] |= LAUNCH_DMA_INTERRUPT_NON_BLOCKING;", "    push[n - 2] |= LAUNCH_DMA_INTERRUPT_NON_BLOCKING;"),
    ("assert!(n >= 2 && push[n - 2] == incr_header(SUBCH_COPY, CE_LAUNCH_DMA, 1), \"the push does not end in a LAUNCH_DMA\");", "assert!(n >= 2);"),
    ("w.push(LAUNCH_DMA_COPY_WITH_SEMAPHORE | if lines > 1 { LAUNCH_DMA_MULTI_LINE } else { 0 });", "w.push(LAUNCH_DMA_COPY_WITH_SEMAPHORE | if lines > 1 { 0 } else { LAUNCH_DMA_MULTI_LINE });"),
    ("w.push(LAUNCH_DMA_COPY_WITH_SEMAPHORE | if lines > 1 { LAUNCH_DMA_MULTI_LINE } else { 0 });", "w.push(LAUNCH_DMA_COPY_WITH_SEMAPHORE | if lines >= 1 { LAUNCH_DMA_MULTI_LINE } else { 0 });"),
    ("w.push(LAUNCH_DMA_COPY_WITH_SEMAPHORE | if lines > 1 { LAUNCH_DMA_MULTI_LINE } else { 0 });", "w.push(LAUNCH_DMA_COPY_WITH_SEMAPHORE);"),
    ("assert!(lines > 0 && line_bytes > 0 && line_bytes <= src_pitch.min(dst_pitch));", "assert!(lines > 0 && line_bytes > 0 && line_bytes <= src_pitch.max(dst_pitch));"),
    ("assert!(lines > 0 && line_bytes > 0 && line_bytes <= src_pitch.min(dst_pitch));", "assert!(lines > 0 && line_bytes > 0);"),
    ("assert!(lines > 0 && line_bytes > 0 && line_bytes <= src_pitch.min(dst_pitch));", "assert!(line_bytes > 0 && line_bytes <= src_pitch.min(dst_pitch));"),
    ("    w.extend([src_pitch, dst_pitch, line_bytes, lines]);", "    w.extend([dst_pitch, src_pitch, line_bytes, lines]);"),
    ("    w.extend([src_pitch, dst_pitch, line_bytes, lines]);", "    w.extend([src_pitch, dst_pitch, lines, line_bytes]);"),
    ("    copy_rect_push(src_va, dst_va, len, len, len, 1, sem_va, payload)", "    copy_rect_push(src_va, dst_va, len, len, len, 2, sem_va, payload)"),
    ("    copy_rect_push(src_va, dst_va, len, len, len, 1, sem_va, payload)", "    copy_rect_push(dst_va, src_va, len, len, len, 1, sem_va, payload)"),
    ("pub const HFENCE_VA: u64 = GPFIFO_VA + 0x30000;", "pub const HFENCE_VA: u64 = GPFIFO_VA + 0x20000;"),
    ("pub const FRAME_BYTES: u64 = 16 << 20;", "pub const FRAME_BYTES: u64 = 8 << 20;"),
]
