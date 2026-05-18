//! ELF64 parser for aarch64 PIE binaries.
//!
//! Zero-dependency, operates on `&[u8]` slices with bounds checking.
//! All structs use `#[repr(C, packed)]` and are read via `read_unaligned`.

// --- ELF constants ---

// ELF identification
pub const ELFMAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
pub const ELFCLASS64: u8 = 2;
pub const ELFDATA2LSB: u8 = 1;

// Object file types
pub const ET_DYN: u16 = 3;

// Machine types
pub const EM_AARCH64: u16 = 183;

// Program header types
pub const PT_NULL: u32 = 0;
pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_INTERP: u32 = 3;
pub const PT_NOTE: u32 = 4;
pub const PT_PHDR: u32 = 6;
pub const PT_GNU_STACK: u32 = 0x6474e551;
pub const PT_GNU_RELRO: u32 = 0x6474e552;

// Program header flags
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

// Dynamic section tags
pub const DT_NULL: i64 = 0;
pub const DT_NEEDED: i64 = 1;
pub const DT_PLTGOT: i64 = 3;
pub const DT_STRTAB: i64 = 5;
pub const DT_SYMTAB: i64 = 6;
pub const DT_RELA: i64 = 7;
pub const DT_RELASZ: i64 = 8;
pub const DT_RELAENT: i64 = 9;
pub const DT_STRSZ: i64 = 10;
pub const DT_SYMENT: i64 = 11;
pub const DT_INIT: i64 = 12;
pub const DT_FINI: i64 = 13;
pub const DT_PLTREL: i64 = 20;
pub const DT_JMPREL: i64 = 23;
pub const DT_PLTRELSZ: i64 = 2;
pub const DT_INIT_ARRAY: i64 = 25;
pub const DT_INIT_ARRAYSZ: i64 = 27;
pub const DT_FINI_ARRAY: i64 = 26;
pub const DT_FINI_ARRAYSZ: i64 = 28;
pub const DT_FLAGS_1: i64 = 0x6ffffffb;
pub const DT_RELACOUNT: i64 = 0x6ffffff9;

// Relocation types (aarch64)
pub const R_AARCH64_RELATIVE: u32 = 0x403;
pub const R_AARCH64_GLOB_DAT: u32 = 0x401;
pub const R_AARCH64_JUMP_SLOT: u32 = 0x402;

// Auxiliary vector types
pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_ENTRY: u64 = 9;
pub const AT_HWCAP: u64 = 16;
pub const AT_RANDOM: u64 = 25;

// --- ELF structures ---

#[repr(C, packed)]
#[derive(Debug, Clone)]
pub struct Ehdr {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

#[repr(C, packed)]
#[derive(Debug, Clone)]
pub struct Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

#[repr(C, packed)]
#[derive(Debug, Clone)]
pub struct Dyn {
    pub d_tag: i64,
    pub d_val: u64,
}

#[repr(C, packed)]
#[derive(Debug, Clone)]
pub struct Rela {
    pub r_offset: u64,
    pub r_info: u64,
    pub r_addend: i64,
}

#[repr(C, packed)]
#[derive(Debug, Clone)]
pub struct Sym {
    pub st_name: u32,
    pub st_info: u8,
    pub st_other: u8,
    pub st_shndx: u16,
    pub st_value: u64,
    pub st_size: u64,
}

// --- Parsing functions ---

/// Read a packed struct from a byte slice at the given offset.
fn read_at<T>(data: &[u8], offset: usize) -> Result<T, String> {
    let size = std::mem::size_of::<T>();
    let end = offset.checked_add(size).ok_or("offset overflow")?;
    if end > data.len() {
        return Err(format!(
            "read at {}+{} exceeds data len {}",
            offset, size, data.len()
        ));
    }
    Ok(unsafe { std::ptr::read_unaligned(data.as_ptr().add(offset) as *const T) })
}

/// Parse and validate the ELF header.
pub fn parse_header(data: &[u8]) -> Result<Ehdr, String> {
    if data.len() < 64 {
        return Err("data too small for ELF header".into());
    }
    let hdr: Ehdr = read_at(data, 0)?;

    // Validate magic
    if hdr.e_ident[0..4] != ELFMAG {
        return Err("not an ELF file (bad magic)".into());
    }
    if hdr.e_ident[4] != ELFCLASS64 {
        return Err("not a 64-bit ELF".into());
    }
    if hdr.e_ident[5] != ELFDATA2LSB {
        return Err("not little-endian".into());
    }
    let machine: u16 = hdr.e_machine;
    if machine != EM_AARCH64 {
        return Err(format!("not aarch64 (machine={})", machine));
    }
    Ok(hdr)
}

/// Validate that the ELF is a PIE (position-independent executable).
pub fn validate_pie(hdr: &Ehdr) -> Result<(), String> {
    let etype = hdr.e_type;
    if etype != ET_DYN {
        return Err(format!("not PIE (type={}, expected DYN=3)", etype));
    }
    Ok(())
}

/// Extract program headers as a Vec.
pub fn program_headers(data: &[u8], hdr: &Ehdr) -> Result<Vec<Phdr>, String> {
    let count = hdr.e_phnum as usize;
    let entsize = hdr.e_phentsize as usize;
    let base = hdr.e_phoff as usize;

    if count == 0 {
        return Ok(vec![]);
    }

    let mut phdrs = Vec::with_capacity(count);
    for i in 0..count {
        let offset = base + i * entsize;
        phdrs.push(read_at(data, offset)?);
    }
    Ok(phdrs)
}

/// Find the PT_INTERP segment and return the interpreter path.
pub fn interpreter_path(data: &[u8], phdrs: &[Phdr]) -> Result<Option<String>, String> {
    for phdr in phdrs {
        if phdr.p_type == PT_INTERP {
            let start = phdr.p_offset as usize;
            let len = phdr.p_filesz as usize;
            if start + len > data.len() {
                return Err("PT_INTERP extends beyond file".into());
            }
            // The path is a null-terminated C string
            let bytes = &data[start..start + len];
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(len);
            let s = String::from_utf8_lossy(&bytes[..end]).into_owned();
            return Ok(Some(s));
        }
    }
    Ok(None)
}

/// Get all PT_LOAD segments.
pub fn load_segments(phdrs: &[Phdr]) -> Vec<&Phdr> {
    phdrs.iter().filter(|p| p.p_type == PT_LOAD).collect()
}

/// Parse the DYNAMIC segment into a Vec of entries.
pub fn dynamic_entries(data: &[u8], phdrs: &[Phdr]) -> Result<Vec<Dyn>, String> {
    for phdr in phdrs {
        if phdr.p_type == PT_DYNAMIC {
            let start = phdr.p_offset as usize;
            let size = phdr.p_filesz as usize;
            if start + size > data.len() {
                return Err("PT_DYNAMIC extends beyond file".into());
            }
            let count = size / 16;
            let mut entries = Vec::with_capacity(count);
            for i in 0..count {
                let entry: Dyn = read_at(data, start + i * 16)?;
                if entry.d_tag == DT_NULL {
                    break;
                }
                entries.push(entry);
            }
            return Ok(entries);
        }
    }
    Ok(vec![])
}

/// Extract RELA entries from the dynamic table.
/// `base` is added to DT_RELA to get the file offset (for PIE, DT_RELA is a vaddr).
pub fn rela_entries_from_dynamic(data: &[u8], dyns: &[Dyn]) -> Result<Vec<Rela>, String> {
    let mut rela_offset = 0u64;
    let mut rela_size = 0u64;
    for d in dyns {
        match d.d_tag {
            DT_RELA => rela_offset = d.d_val,
            DT_RELASZ => rela_size = d.d_val,
            _ => {}
        }
    }
    if rela_size == 0 {
        return Ok(vec![]);
    }
    let count = rela_size as usize / 24; // sizeof(Rela) = 24
    let start = rela_offset as usize;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        entries.push(read_at(data, start + i * 24)?);
    }
    Ok(entries)
}

/// Get the relocation type from an r_info field.
pub fn rela_type(info: u64) -> u32 {
    (info & 0xffffffff) as u32
}

/// Get the symbol index from an r_info field.
pub fn rela_sym(info: u64) -> u32 {
    (info >> 32) as u32
}

/// Calculate the total address span of LOAD segments (for mmap reservation).
pub fn load_span(phdrs: &[Phdr]) -> (u64, u64) {
    let loads = load_segments(phdrs);
    if loads.is_empty() {
        return (0, 0);
    }
    let min_vaddr = loads.iter().map(|p| p.p_vaddr).min().unwrap();
    let max_vaddr = loads
        .iter()
        .map(|p| p.p_vaddr + p.p_memsz)
        .max()
        .unwrap();
    (min_vaddr, max_vaddr - min_vaddr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_real_binary() {
        let paths = [
            "/data/data/com.termux/files/usr/glibc/bin/ls",
            "/data/data/com.termux/files/usr/bin/ls",
        ];
        let data = paths
            .iter()
            .filter_map(|p| std::fs::read(p).ok())
            .next()
            .expect("no test binary found");

        let hdr = parse_header(&data).unwrap();
        assert_eq!(&hdr.e_ident[0..4], &ELFMAG);
        assert_eq!(hdr.e_ident[4], ELFCLASS64);
        assert_eq!(hdr.e_ident[5], ELFDATA2LSB);
        assert_eq!({hdr.e_machine}, EM_AARCH64);
        assert_eq!({hdr.e_type}, ET_DYN);

        let phdrs = program_headers(&data, &hdr).unwrap();
        assert!(!phdrs.is_empty());

        let loads = load_segments(&phdrs);
        assert_eq!(loads.len(), 2, "expected 2 LOAD segments");

        let interp = interpreter_path(&data, &phdrs).unwrap();
        assert!(interp.is_some(), "dynamic binary should have interpreter");

        let dyns = dynamic_entries(&data, &phdrs).unwrap();
        assert!(!dyns.is_empty());

        let relas = rela_entries_from_dynamic(&data, &dyns).unwrap();
        assert!(!relas.is_empty());
        let relative_count = relas
            .iter()
            .filter(|r| rela_type(r.r_info) == R_AARCH64_RELATIVE)
            .count();
        assert!(relative_count > 0, "should have RELATIVE relocations");
    }

    #[test]
    fn test_reject_bad_magic() {
        let data = vec![0u8; 128];
        assert!(parse_header(&data).is_err());
    }
}
