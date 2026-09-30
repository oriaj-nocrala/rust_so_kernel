# G5 layer 2: BO_EXPORT / BO_IMPORT in the kernel adapter (kernel/src/drivers/dev_nvgpu.rs, the ioctl plumbing in
# kernel/src/process/syscall/fs.rs). Each mutant must make nvgpu_sw_test fail: scripts/gpu-mutate-qemu.py nvgpu/mutations/sharing_kernel.py
TEST = "nvgpu_sw_test"
D = "kernel/src/drivers/dev_nvgpu.rs"
F = "kernel/src/process/syscall/fs.rs"
MUTS = [
    (D, "a descriptor's share never releases its storage", "        let (heap, off) = heap_of(self.backing);\n        release_storage(heap, off, self.size);", "        let _ = heap_of(self.backing);"),
    (D, "export takes no hold", "        let (heap, off) = heap_of(backing);\n        hold_storage(heap, off);\n        Ok(Box::new(BoFile", "        let _ = heap_of(backing);\n        Ok(Box::new(BoFile"),
    (D, "the last holder is not the only one that frees", "    *n -= 1;\n    if *n > 0 {\n        return;\n    }", "    *n -= 1;"),
    (D, "the pages are not discarded when the last holder goes", "        st.arena.discard(off, size);\n", "        let _ = size;\n"),
    (D, "the descriptor does not name its BO for an import", "        if request as u32 != uapi::IOC_BO_IMPORT {\n            return None;\n        }\n        let r: uapi::BoImport = read_user(arg).ok()?;\n        usize::try_from(r.fd).ok()", "        let _ = (request, arg);\n        None"),
    (D, "import flags are not checked", "        let mut r: uapi::BoImport = read_user(arg)?;\n        if r.flags != 0 {\n            return Err(errno::EINVAL);\n        }", "        let mut r: uapi::BoImport = read_user(arg)?;"),
    (D, "export flags are not checked", "        let r: uapi::BoExport = read_user(arg)?;\n        if r.flags != 0 {\n            return Err(errno::EINVAL);\n        }", "        let r: uapi::BoExport = read_user(arg)?;"),
    (D, "a BO descriptor cannot be dup'd", "        Some(Box::new(BoFile { share: self.share.clone() }))\n    }\n\n    fn device_ref", "        None\n    }\n\n    fn device_ref"),
    (D, "a BO descriptor names no object", "        Some(self.share.clone() as Arc<dyn Any + Send + Sync>)\n    }\n}\n\n/// The descriptor", "        None\n    }\n}\n\n/// The descriptor") if False else
    (D, "a BO descriptor names no object", "    fn device_ref(&self) -> Option<Arc<dyn Any + Send + Sync>> {\n        Some(self.share.clone() as Arc<dyn Any + Send + Sync>)", "    fn device_ref(&self) -> Option<Arc<dyn Any + Send + Sync>> {\n        None"),
    (D, "a negative descriptor is not refused", "        if r.fd < 0 {\n            return Err(errno::EBADF);\n        }\n", ""),
    (D, "the imported size is not reported", "        r.size_out = size;\n        if let Err(e) = write_user(arg, r) {\n            let _ = dev.bo_free(handle);", "        r.size_out = 0;\n        if let Err(e) = write_user(arg, r) {\n            let _ = dev.bo_free(handle);"),
    (F, "the new descriptor is not close-on-exec", "                        let _ = table.set_cloexec(n, true);\n", ""),
    (F, "a failed allocation of the descriptor is reported as success", "                    Err(_) => errno::EMFILE,\n                }),\n                None => None,", "                    Err(_) => 0,\n                }),\n                None => None,"),
]

# Timelines (SYNC_EXPORT / SYNC_IMPORT): same file, same test.
MUTS += [
    (D, "a timeline descriptor's share never releases it", "    fn drop(&mut self) {\n        release_sync(self.id);\n    }", "    fn drop(&mut self) {\n        let _ = self.id;\n    }"),
    (D, "the last holder does not free the timeline", "        t.refs -= 1;\n        if t.refs == 0 {\n            g.remove(&id);\n        }", "        t.refs -= 1;"),
    (D, "a hold on a shared timeline is not counted", "            Some(t) => {\n                t.refs += 1;\n                true\n            }\n            None => false,\n        }\n    }\n\n    fn shared_sync_release", "            Some(_) => true,\n            None => false,\n        }\n    }\n\n    fn shared_sync_release"),
    (D, "queued signals are never resolved by a reader", "            Some(t) => {\n                t.resolve();\n                (t.value, t.pending)\n            }", "            Some(t) => (t.value, t.pending),"),
    (D, "a signal on the software device does not complete at once", "                None => t.value = t.value.max(value),", "                None => {}"),
    (D, "a queued signal does not raise the pending value", "            t.pending = t.pending.max(value);\n            match chan {", "            match chan {"),
    (D, "a CPU signal does not raise the pending value", "            t.value = t.value.max(value);\n            t.pending = t.pending.max(value);\n        }\n    }\n\n    fn shared_sync_queue", "            t.value = t.value.max(value);\n        }\n    }\n\n    fn shared_sync_queue"),
    (D, "the timeline descriptor does not name its timeline", "    fn device_ref(&self) -> Option<Arc<dyn Any + Send + Sync>> {\n        Some(self.share.clone() as Arc<dyn Any + Send + Sync>)\n    }\n}\n\n/// An exported BO's claim", "    fn device_ref(&self) -> Option<Arc<dyn Any + Send + Sync>> {\n        None\n    }\n}\n\n/// An exported BO's claim"),
    (D, "a timeline descriptor cannot be dup'd", "        Some(Box::new(SyncFile { share: self.share.clone() }))", "        None"),
    (D, "sync import flags are not checked", "        let mut r: uapi::SyncImport = read_user(arg)?;\n        if r.flags != 0 {\n            return Err(errno::EINVAL);\n        }", "        let mut r: uapi::SyncImport = read_user(arg)?;"),
    (D, "sync export flags are not checked", "        let r: uapi::SyncExport = read_user(arg)?;\n        if r.flags != 0 {\n            return Err(errno::EINVAL);\n        }", "        let r: uapi::SyncExport = read_user(arg)?;"),
    (D, "a negative fd is not refused for a timeline import", "        let mut r: uapi::SyncImport = read_user(arg)?;\n        if r.flags != 0 {\n            return Err(errno::EINVAL);\n        }\n        if r.fd < 0 {\n            return Err(errno::EBADF);\n        }", "        let mut r: uapi::SyncImport = read_user(arg)?;\n        if r.flags != 0 {\n            return Err(errno::EINVAL);\n        }"),
    (D, "a timeline import does not look at the descriptor's type", "downcast::<SyncShare>().map_err(|_| errno::EINVAL)?;", "downcast::<SyncShare>().map_err(|_| errno::EINVAL).unwrap_or_else(|_| Arc::new(SyncShare { id: 1 }));"),
    (D, "the timeline import is not named for the lookup", "            uapi::IOC_SYNC_IMPORT => usize::try_from(read_user::<uapi::SyncImport>(arg).ok()?.fd).ok(),", "            uapi::IOC_SYNC_IMPORT => None,"),
    (D, "a channel goes without resolving shared timelines", "            resolve_all_syncs();\n            gpu::uapi::ctx_destroy(id);", "            gpu::uapi::ctx_destroy(id);"),
]

# The earlier "downcast failure" mutant of BO_IMPORT is now killable (a timeline descriptor reaches it): add it back.
MUTS.append((D, "an import of a non-BO falls through as success", "downcast::<BoShare>().map_err(|_| errno::EINVAL)?;", "downcast::<BoShare>().map_err(|_| errno::EINVAL).unwrap_or_else(|_| Arc::new(BoShare { backing: Backing::Vram { vram_off: 0 }, size: 0x1000 }));"))
