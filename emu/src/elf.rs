//! Minimal ELF64 loader — enough to place a freestanding x86-64 kernel image
//! into physical memory and find its entry point.
//!
//! ntoskrnl-rs ships as an ELF (the `bootloader` crate normally maps it and
//! hands over a `BootInfo`). This loader copies the `PT_LOAD` segments to their
//! physical addresses; wiring up the full bootloader handoff (a constructed
//! `BootInfo` + the bootloader's page tables) is the remaining step before the
//! real kernel runs (see SPEC.md §"Booting the real kernel").

/// A loadable segment: where it goes in physical memory and its bytes.
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub paddr: u64,
    pub vaddr: u64,
    pub file_off: usize,
    pub file_size: usize,
    pub mem_size: usize,
    /// `PF_X` execute / `PF_W` write / `PF_R` read flags.
    pub flags: u32,
}

/// Parsed ELF64 metadata.
#[derive(Debug)]
pub struct Elf<'a> {
    pub entry: u64,
    pub segments: heapless_vec::Vec,
    image: &'a [u8],
}

const PT_LOAD: u32 = 1;

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd64(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ElfError {
    NotElf,
    Not64,
    NotLittleEndian,
    NotX86_64,
    Truncated,
    TooManySegments,
}

impl<'a> Elf<'a> {
    /// Parse an ELF64 little-endian x86-64 image.
    pub fn parse(image: &'a [u8]) -> Result<Self, ElfError> {
        if image.len() < 64 || &image[0..4] != b"\x7fELF" {
            return Err(ElfError::NotElf);
        }
        if image[4] != 2 {
            return Err(ElfError::Not64);
        }
        if image[5] != 1 {
            return Err(ElfError::NotLittleEndian);
        }
        if rd16(image, 18) != 0x3E {
            return Err(ElfError::NotX86_64);
        }
        let entry = rd64(image, 24);
        let phoff = rd64(image, 32) as usize;
        let phentsize = rd16(image, 54) as usize;
        let phnum = rd16(image, 56) as usize;
        if phoff + phnum * phentsize > image.len() {
            return Err(ElfError::Truncated);
        }
        let mut segments = heapless_vec::Vec::new();
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            if rd32(image, ph) != PT_LOAD {
                continue;
            }
            let seg = Segment {
                flags: rd32(image, ph + 4),
                file_off: rd64(image, ph + 8) as usize,
                vaddr: rd64(image, ph + 16),
                paddr: rd64(image, ph + 24),
                file_size: rd64(image, ph + 32) as usize,
                mem_size: rd64(image, ph + 40) as usize,
            };
            if segments.push(seg).is_err() {
                return Err(ElfError::TooManySegments);
            }
        }
        Ok(Elf { entry, segments, image })
    }

    /// Bytes of a segment as they appear in the file (length `file_size`).
    pub fn segment_bytes(&self, seg: &Segment) -> &'a [u8] {
        &self.image[seg.file_off..seg.file_off + seg.file_size]
    }
}

/// A tiny fixed-capacity vector so the loader stays `no_std` without an
/// allocator (program-header counts are small).
pub mod heapless_vec {
    pub const CAP: usize = 64;

    #[derive(Debug)]
    pub struct Vec {
        data: [super::Segment; CAP],
        len: usize,
    }
    impl Vec {
        pub fn new() -> Self {
            Vec {
                data: [super::Segment {
                    paddr: 0,
                    vaddr: 0,
                    file_off: 0,
                    file_size: 0,
                    mem_size: 0,
                    flags: 0,
                }; CAP],
                len: 0,
            }
        }
        pub fn push(&mut self, s: super::Segment) -> Result<(), ()> {
            if self.len >= CAP {
                return Err(());
            }
            self.data[self.len] = s;
            self.len += 1;
            Ok(())
        }
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
        pub fn iter(&self) -> core::slice::Iter<'_, super::Segment> {
            self.data[..self.len].iter()
        }
    }
    impl Default for Vec {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal but valid ELF64 with one PT_LOAD segment.
    fn minimal_elf(entry: u64, paddr: u64, payload: &[u8]) -> std::vec::Vec<u8> {
        let ehsize = 64usize;
        let phentsize = 56usize;
        let phoff = ehsize;
        let data_off = ehsize + phentsize;
        let mut b = std::vec![0u8; data_off + payload.len()];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // 64-bit
        b[5] = 1; // little-endian
        b[6] = 1; // version
        b[16..18].copy_from_slice(&2u16.to_le_bytes()); // e_type ET_EXEC
        b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // x86-64
        b[24..32].copy_from_slice(&entry.to_le_bytes()); // e_entry
        b[32..40].copy_from_slice(&(phoff as u64).to_le_bytes()); // e_phoff
        b[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
        b[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum = 1
        // program header
        let ph = phoff;
        b[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        b[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes()); // R+X
        b[ph + 8..ph + 16].copy_from_slice(&(data_off as u64).to_le_bytes()); // p_offset
        b[ph + 16..ph + 24].copy_from_slice(&paddr.to_le_bytes()); // p_vaddr
        b[ph + 24..ph + 32].copy_from_slice(&paddr.to_le_bytes()); // p_paddr
        b[ph + 32..ph + 40].copy_from_slice(&(payload.len() as u64).to_le_bytes()); // p_filesz
        b[ph + 40..ph + 48].copy_from_slice(&(payload.len() as u64).to_le_bytes()); // p_memsz
        b[data_off..].copy_from_slice(payload);
        b
    }

    #[test]
    fn parses_minimal_elf() {
        let img = minimal_elf(0x40_1000, 0x40_0000, &[0x90, 0xF4]); // nop; hlt
        let elf = Elf::parse(&img).unwrap();
        assert_eq!(elf.entry, 0x40_1000);
        assert_eq!(elf.segments.len(), 1);
        let seg = elf.segments.iter().next().unwrap();
        assert_eq!(seg.paddr, 0x40_0000);
        assert_eq!(elf.segment_bytes(seg), &[0x90, 0xF4]);
    }

    #[test]
    fn rejects_non_elf() {
        assert_eq!(Elf::parse(b"not an elf at all............").unwrap_err(), ElfError::NotElf);
    }
}
