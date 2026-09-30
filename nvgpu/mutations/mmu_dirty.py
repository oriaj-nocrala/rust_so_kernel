# Mutations of the run-time dirty-table tracking of PageTables (G4c): what a bind or unbind must write back to VRAM.
#   scripts/gpu-mutate.py nvgpu/src/mmu.rs tests:: nvgpu/mutations/mmu_dirty.py
M = [
    ("tables: vec![[0u8; TABLE_SIZE]], dirty: vec![true] }", "tables: vec![[0u8; TABLE_SIZE]], dirty: vec![false] }"),
    ("self.dirty.push(true);", "self.dirty.push(false);"),
    ("wr64(&mut self.tables[table], at, v);\n        self.dirty[table] = true;", "wr64(&mut self.tables[table], at, v);"),
    ("wr64(&mut self.tables[table], at, v);\n        self.dirty[table] = true;", "wr64(&mut self.tables[table], at, v);\n        self.dirty[0] = true;"),
    ("if core::mem::take(d) {", "if *d {"),
    ("if core::mem::take(d) {", "if !core::mem::take(d) {"),
    ("out.push((base + (i * TABLE_SIZE) as u64, self.tables[i]));", "out.push((base + ((i + 1) * TABLE_SIZE) as u64, self.tables[i]));"),
    ("out.push((base + (i * TABLE_SIZE) as u64, self.tables[i]));", "out.push((base + (i * TABLE_SIZE) as u64, self.tables[0]));"),
]
