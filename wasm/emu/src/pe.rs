//! Minimal PE32+ loader for the interpreter (Track B).
//!
//! Maps a real x86-64 PE image into the interpreter's flat memory so it can be
//! executed. To keep the interpreter's addressing trivial (it indexes memory by
//! absolute address), the image is loaded at **VA 0** — i.e. virtual address ==
//! RVA — and base relocations are applied with `delta = -preferred_base`, which
//! turns every absolute pointer in the image into its RVA. The stack/heap live
//! higher in the same buffer. No sections protection, no TLS/imports here yet
//! (imports are resolved separately by the kernel side); this just gets the
//! bytes and the entry point in place.

/// Why a load failed.
#[derive(Debug, PartialEq, Eq)]
pub enum PeError {
    NotMz,
    NotPe,
    NotAmd64,
    NotPe32Plus,
    Truncated,
    ImageTooLarge,
}

fn u16le(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
fn u32le(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}
fn u64le(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
}

/// What [`load_pe`] produced.
#[derive(Debug)]
pub struct Loaded {
    /// Entry point (== entry RVA, since the image is at VA 0).
    pub entry: u64,
    /// Total mapped image size (bytes), i.e. SizeOfImage.
    pub image_size: usize,
}

/// Map `data` (a PE32+ image) into `mem` at VA 0 and apply base relocations.
/// `mem` must be at least `SizeOfImage` long (plus whatever the caller reserves
/// above it for stack/heap). Returns the entry point and image size.
pub fn load_pe(data: &[u8], mem: &mut [u8]) -> Result<Loaded, PeError> {
    if u16le(data, 0) != Some(0x5a4d) {
        return Err(PeError::NotMz);
    }
    let e_lfanew = u32le(data, 0x3c).ok_or(PeError::Truncated)? as usize;
    if u32le(data, e_lfanew) != Some(0x0000_4550) {
        return Err(PeError::NotPe);
    }
    let coff = e_lfanew + 4;
    if u16le(data, coff) != Some(0x8664) {
        return Err(PeError::NotAmd64);
    }
    let num_sections = u16le(data, coff + 2).ok_or(PeError::Truncated)? as usize;
    let opt_size = u16le(data, coff + 16).ok_or(PeError::Truncated)? as usize;
    let opt = coff + 20;
    if u16le(data, opt) != Some(0x20b) {
        return Err(PeError::NotPe32Plus);
    }
    let entry_rva = u32le(data, opt + 16).ok_or(PeError::Truncated)? as u64;
    let preferred_base = u64le(data, opt + 24).ok_or(PeError::Truncated)?;
    let size_of_image = u32le(data, opt + 56).ok_or(PeError::Truncated)? as usize;
    let size_of_headers = u32le(data, opt + 60).ok_or(PeError::Truncated)? as usize;
    let num_dirs = u32le(data, opt + 108).ok_or(PeError::Truncated)? as usize;

    if size_of_image == 0 || size_of_image > mem.len() {
        return Err(PeError::ImageTooLarge);
    }

    // Headers, then each section's raw data, placed at its RVA (== VA at base 0).
    for b in mem[..size_of_image].iter_mut() {
        *b = 0;
    }
    let hdr = size_of_headers.min(data.len()).min(size_of_image);
    mem[..hdr].copy_from_slice(&data[..hdr]);

    let sec_table = opt + opt_size;
    for s in 0..num_sections {
        let sh = sec_table + s * 40;
        let va = u32le(data, sh + 12).ok_or(PeError::Truncated)? as usize;
        let raw_size = u32le(data, sh + 16).ok_or(PeError::Truncated)? as usize;
        let raw_ptr = u32le(data, sh + 20).ok_or(PeError::Truncated)? as usize;
        if raw_size == 0 {
            continue;
        }
        let src = data.get(raw_ptr..raw_ptr + raw_size).ok_or(PeError::Truncated)?;
        let dst = mem.get_mut(va..va + raw_size).ok_or(PeError::ImageTooLarge)?;
        dst.copy_from_slice(src);
    }

    // Base relocations (data directory 5): rebase every absolute pointer so the
    // image works at VA 0. delta = 0 - preferred_base.
    const DIR_BASERELOC: usize = 5;
    if DIR_BASERELOC < num_dirs {
        let reloc_rva = u32le(data, opt + 112 + DIR_BASERELOC * 8).unwrap_or(0) as usize;
        let reloc_size = u32le(data, opt + 112 + DIR_BASERELOC * 8 + 4).unwrap_or(0) as usize;
        let delta = 0u64.wrapping_sub(preferred_base);
        let mut off = reloc_rva;
        let end = reloc_rva + reloc_size;
        while off + 8 <= end && off + 8 <= size_of_image {
            let page_rva = u32le(mem, off).unwrap_or(0) as usize;
            let block_size = u32le(mem, off + 4).unwrap_or(0) as usize;
            if block_size < 8 {
                break;
            }
            let entries = (block_size - 8) / 2;
            for i in 0..entries {
                let e = u16le(mem, off + 8 + i * 2).unwrap_or(0);
                let typ = e >> 12;
                let ofs = (e & 0x0fff) as usize;
                if typ == 10 {
                    // IMAGE_REL_BASED_DIR64
                    let at = page_rva + ofs;
                    if at + 8 <= size_of_image {
                        let v = u64le(mem, at).unwrap_or(0).wrapping_add(delta);
                        mem[at..at + 8].copy_from_slice(&v.to_le_bytes());
                    }
                }
            }
            off += block_size;
        }
    }

    Ok(Loaded { entry: entry_rva, image_size: size_of_image })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_pe() {
        let mut mem = vec![0u8; 1024];
        assert_eq!(load_pe(b"not a pe", &mut mem).unwrap_err(), PeError::NotMz);
    }

    /// Load the real whoami.exe (if present in the repo) and sanity-check it.
    #[test]
    fn loads_real_whoami() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../winbin/whoami.exe");
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => return, // binary not staged in this checkout; skip
        };
        let mut mem = vec![0u8; 64 * 1024 * 1024];
        let loaded = load_pe(&data, &mut mem).expect("whoami.exe should load");
        assert!(loaded.entry > 0 && (loaded.entry as usize) < loaded.image_size);
        assert_eq!(&mem[0..2], b"MZ", "headers mapped at VA 0");
        // The image should contain x86 code at the entry (not all zero).
        let e = loaded.entry as usize;
        assert!(mem[e..e + 16].iter().any(|&b| b != 0), "entry has code");
    }
}
