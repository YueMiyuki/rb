//! ELF, PE, or Mach-O, and which architecture.

use std::path::Path;

pub fn describe(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    describe_bytes(&bytes)
}

fn u16le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// 64-bit little-endian ELF with a `PT_INTERP` program header (i.e. a dynamically linked program)
fn has_interp(b: &[u8]) -> bool {
    if b.get(4) != Some(&2) {
        return false;
    }
    let (Some(phoff), Some(phentsize), Some(phnum)) = (
        b.get(32..40).map(|s| u64::from_le_bytes(s.try_into().unwrap()) as usize),
        u16le(b, 54).map(usize::from),
        u16le(b, 56).map(usize::from),
    ) else {
        return false;
    };
    (0..phnum).any(|i| u32le(b, phoff + i * phentsize) == Some(3))
}

pub fn describe_bytes(b: &[u8]) -> Option<String> {
    if b.starts_with(b"\x7fELF") {
        let machine = match u16le(b, 18)? {
            0x3e => "x86-64",
            0xb7 => "aarch64",
            0x03 => "x86",
            0x28 => "arm",
            0xf3 => "riscv",
            other => return Some(format!("ELF machine {other:#x}")),
        };
        let kind = match u16le(b, 16)? {
            2 => "executable",
            3 if has_interp(b) => "PIE executable",
            3 => "shared object",
            1 => "relocatable",
            _ => "file",
        };
        return Some(format!("ELF {machine} {kind}"));
    }
    if b.starts_with(b"MZ") {
        let pe = u32le(b, 0x3c)? as usize;
        if b.get(pe..pe + 4)? != b"PE\0\0" {
            return Some("DOS executable".into());
        }
        let machine = match u16le(b, pe + 4)? {
            0x8664 => "x86-64",
            0xaa64 => "aarch64",
            0x014c => "x86",
            other => return Some(format!("PE machine {other:#x}")),
        };
        return Some(format!("PE32+ {machine} executable"));
    }
    let magic = u32le(b, 0)?;
    if magic == 0xfeedfacf {
        let arch = match u32le(b, 4)? {
            0x0100_0007 => "x86-64",
            0x0100_000c => "arm64",
            other => return Some(format!("Mach-O cputype {other:#x}")),
        };
        return Some(format!("Mach-O {arch} executable"));
    }
    if magic == 0xbebafeca {
        return Some("Mach-O universal binary".into());
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn detects_self() {
        let exe = std::env::current_exe().unwrap();
        let d = super::describe(&exe).unwrap();
        assert!(d.contains("Mach-O") || d.contains("ELF") || d.contains("PE32+"), "{d}");
    }
}
