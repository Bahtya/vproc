//! PIE ELF loader — maps LOAD segments, applies relocations.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::elf;

/// Loaded ELF image in memory.
pub struct LoadedImage {
    /// Base address where the PIE was mapped.
    pub base: usize,
    /// Total span of all LOAD segments.
    pub total_size: usize,
    /// Absolute entry point address (base + e_entry).
    pub entry: usize,
    /// Absolute address of program headers in memory (for auxv AT_PHDR).
    pub phdr_addr: usize,
    /// Number of program headers.
    pub phnum: u16,
    /// Size of each program header entry.
    pub phentsize: u16,
    /// Interpreter path, if this is a dynamic binary.
    pub interp_path: Option<String>,
}

impl LoadedImage {
    /// Returns the interpreter entry point if this binary has one.
    /// The interpreter must be loaded separately and its entry returned.
    pub fn is_dynamic(&self) -> bool {
        self.interp_path.is_some()
    }
}

// Bump allocator for load addresses.
// Starts at 128 GiB — Android aarch64 limits user VA to ~512 GiB,
// and existing mappings typically sit below ~100 GiB.
static NEXT_BASE: AtomicUsize = AtomicUsize::new(0x20_0000_0000);

const PAGE_SIZE: usize = 4096;
const SLOT_ALIGN: usize = 64 * 1024; // 64 KiB, matches typical PIE alignment

fn page_align_down(addr: usize) -> usize {
    addr & !(PAGE_SIZE - 1)
}

// MAP_FIXED_NOREPLACE = 0x100000 (Linux 4.17+)
const MAP_FIXED_NOREPLACE: i32 = 0x100000;

fn page_align_up(addr: usize) -> usize {
    (addr + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Allocate a slot in the address space for a PIE binary.
fn alloc_slot(size: usize) -> usize {
    let aligned_size = (size + SLOT_ALIGN - 1) & !(SLOT_ALIGN - 1);
    NEXT_BASE.fetch_add(aligned_size + PAGE_SIZE, Ordering::Relaxed)
}

/// Convert Phdr flags to mmap protection bits.
fn prot_from_flags(flags: u32) -> i32 {
    let mut prot = 0;
    if flags & elf::PF_R != 0 {
        prot |= libc::PROT_READ;
    }
    if flags & elf::PF_W != 0 {
        prot |= libc::PROT_WRITE;
    }
    if flags & elf::PF_X != 0 {
        prot |= libc::PROT_EXEC;
    }
    prot
}

/// Load a PIE ELF binary into memory.
///
/// Steps:
/// 1. Parse and validate ELF header
/// 2. Find LOAD segments, calculate total span
/// 3. Reserve address space with PROT_NONE
/// 4. Map each LOAD segment with proper permissions
/// 5. Apply R_AARCH64_RELATIVE relocations
/// 6. Return LoadedImage
pub fn load_pie(data: &[u8]) -> Result<LoadedImage, String> {
    let hdr = elf::parse_header(data)?;
    elf::validate_pie(&hdr)?;

    let phdrs = elf::program_headers(data, &hdr)?;
    let loads = elf::load_segments(&phdrs);
    if loads.is_empty() {
        return Err("no LOAD segments".into());
    }

    // Calculate total span
    let (_min_vaddr, span) = elf::load_span(&phdrs);
    let total_mapped = page_align_up(span as usize);

    // Allocate a slot
    let base = alloc_slot(total_mapped);

    // Reserve the entire region as PROT_NONE first
    let reserve = unsafe {
        libc::mmap(
            base as *mut std::ffi::c_void,
            total_mapped,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if reserve == libc::MAP_FAILED {
        return Err(format!(
            "mmap reserve failed at {:#x}: errno {}",
            base,
            unsafe { *libc::__errno() }
        ));
    }

    // Map each LOAD segment
    for phdr in &loads {
        map_load_segment(data, base, phdr)?;
    }

    // Apply RELATIVE relocations
    let dyns = elf::dynamic_entries(data, &phdrs)?;
    if !dyns.is_empty() {
        let relas = elf::rela_entries_from_dynamic(data, &dyns)?;
        apply_relative_relocations(base, &relas)?;
    }

    // Find interpreter path
    let interp_path = elf::interpreter_path(data, &phdrs)?;

    let entry = base + (hdr.e_entry as usize);
    let phdr_addr = base + (hdr.e_phoff as usize);

    Ok(LoadedImage {
        base,
        total_size: total_mapped,
        entry,
        phdr_addr,
        phnum: hdr.e_phnum,
        phentsize: hdr.e_phentsize,
        interp_path,
    })
}

/// Map a single PT_LOAD segment.
fn map_load_segment(
    data: &[u8],
    base: usize,
    phdr: &elf::Phdr,
) -> Result<(), String> {
    let prot = prot_from_flags(phdr.p_flags);
    let seg_vaddr = phdr.p_vaddr as usize;
    let seg_filesz = phdr.p_filesz as usize;
    let seg_memsz = phdr.p_memsz as usize;
    let seg_offset = phdr.p_offset as usize;

    // Page-align the mapping boundaries
    let page_start = page_align_down(seg_vaddr);
    let page_end = page_align_up(seg_vaddr + seg_memsz);
    let map_size = page_end - page_start;
    let map_addr = base + page_start;

    // Offset within the first page before the segment data starts
    let page_offset = seg_vaddr - page_start;

    // Map anonymous memory at the target address (RW for writing)
    let ptr = unsafe {
        libc::mmap(
            map_addr as *mut std::ffi::c_void,
            map_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(format!(
            "mmap LOAD at {:#x} size {:#x}: errno {}",
            map_addr,
            map_size,
            unsafe { *libc::__errno() }
        ));
    }

    // Copy file data into the mapped region
    if seg_filesz > 0 {
        if seg_offset + seg_filesz > data.len() {
            return Err("LOAD segment extends beyond file".into());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                data[seg_offset..seg_offset + seg_filesz].as_ptr(),
                ptr.add(page_offset) as *mut u8,
                seg_filesz,
            );
        }
    }
    // BSS (memsz > filesz) is already zero-filled by MAP_ANONYMOUS

    // Set final permissions
    let rc = unsafe {
        libc::mprotect(
            map_addr as *mut std::ffi::c_void,
            map_size,
            prot,
        )
    };
    if rc != 0 {
        return Err(format!(
            "mprotect at {:#x}: errno {}",
            map_addr,
            unsafe { *libc::__errno() }
        ));
    }

    Ok(())
}

/// Apply R_AARCH64_RELATIVE relocations.
fn apply_relative_relocations(base: usize, relas: &[elf::Rela]) -> Result<usize, String> {
    let mut count = 0;
    for rela in relas {
        let rtype = elf::rela_type(rela.r_info);
        if rtype != elf::R_AARCH64_RELATIVE {
            continue;
        }
        let target = base + (rela.r_offset as usize);
        let value = (base as u64).wrapping_add(rela.r_addend as u64);
        unsafe {
            std::ptr::write_volatile(target as *mut u64, value);
        }
        count += 1;
    }
    Ok(count)
}

/// Build the auxiliary vector for a loaded ELF.
pub fn build_auxv(image: &LoadedImage, interp_base: usize) -> Vec<[u64; 2]> {
    let mut auxv = Vec::new();

    // Read HWCAP from /proc/self/auxv
    let hwcap = read_hwcap();

    auxv.push([elf::AT_PHDR, image.phdr_addr as u64]);
    auxv.push([elf::AT_PHENT, image.phentsize as u64]);
    auxv.push([elf::AT_PHNUM, image.phnum as u64]);
    auxv.push([elf::AT_PAGESZ, PAGE_SIZE as u64]);
    auxv.push([elf::AT_ENTRY, image.entry as u64]);
    auxv.push([elf::AT_BASE, interp_base as u64]);
    if let Some(hwcap) = hwcap {
        auxv.push([elf::AT_HWCAP, hwcap]);
    }
    // AT_RANDOM: point to 16 static random bytes
    auxv.push([elf::AT_RANDOM, RANDOM_BYTES.as_ptr() as u64]);
    auxv.push([elf::AT_NULL, 0]);

    auxv
}

/// Random bytes for AT_RANDOM (16 bytes).
static RANDOM_BYTES: [u8; 16] = [
    0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE,
    0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0,
];

/// Read AT_HWCAP from /proc/self/auxv.
fn read_hwcap() -> Option<u64> {
    let data = std::fs::read("/proc/self/auxv").ok()?;
    // auxv entries are pairs of u64: (type, value)
    if data.len() < 16 {
        return None;
    }
    for i in (0..data.len() - 15).step_by(16) {
        let kind = u64::from_ne_bytes(data[i..i + 8].try_into().ok()?);
        let value = u64::from_ne_bytes(data[i + 8..i + 16].try_into().ok()?);
        if kind == elf::AT_HWCAP {
            return Some(value);
        }
        if kind == elf::AT_NULL {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_real_binary() {
        let paths = [
            "/data/data/com.termux/files/usr/glibc/bin/true",
            "/data/data/com.termux/files/usr/glibc/bin/ls",
            "/data/data/com.termux/files/usr/bin/true",
        ];
        for path in &paths {
            let data = match std::fs::read(path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let result = load_pie(&data);
            assert!(result.is_ok(), "load_pie({}): {:?}", path, result.err());
            let image = result.unwrap();
            assert_ne!(image.entry, 0);
            assert_ne!(image.base, 0);
            assert!(image.phnum > 0);
            println!(
                "loaded {} at {:#x}, entry {:#x}, {} phdrs, interp={:?}",
                path, image.base, image.entry, image.phnum, image.interp_path
            );
            return; // one success is enough
        }
        panic!("no test binary found");
    }
}
