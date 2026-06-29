//! Long-mode (IA-32e) memory management: 4-level paging over a flat physical
//! address space.
//!
//! The seed interpreter treats linear == physical (it ran ring-3 code in a flat
//! buffer with no MMU). A full machine that boots ntoskrnl-rs must walk the
//! guest's own page tables: the kernel enables paging (`CR0.PG`), PAE
//! (`CR4.PAE`) and long mode (`EFER.LME`→`LMA`), points `CR3` at a PML4, and
//! from then on every code/data access is a virtual address translated through
//! PML4 → PDPT → PD → PT (with 1 GiB / 2 MiB large-page short-circuits).
//!
//! This module is the translation core only: given the control registers and a
//! virtual address it returns a physical address or a [`PageFault`]. It is
//! deliberately decoupled from instruction execution so it can be unit-tested by
//! hand-building page tables in a buffer. Integration into `Cpu::step`'s memory
//! path is a later milestone.

/// Control-register / EFER state relevant to translation. Mirrors the subset of
/// the architectural registers the page walk consults.
#[derive(Clone, Copy, Debug, Default)]
pub struct Paging {
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
}

// Control-register bits we care about.
pub const CR0_PG: u64 = 1 << 31; // paging enable
pub const CR4_PAE: u64 = 1 << 5; // physical address extension (required for long mode)
pub const EFER_LME: u64 = 1 << 8; // long mode enable
pub const EFER_LMA: u64 = 1 << 10; // long mode active
pub const EFER_NXE: u64 = 1 << 11; // no-execute enable

// Page-table entry bits.
const PTE_P: u64 = 1 << 0; // present
const PTE_RW: u64 = 1 << 1; // writable
const PTE_US: u64 = 1 << 2; // user-accessible
const PTE_PS: u64 = 1 << 7; // page size (large page at PDPTE/PDE level)
const PTE_NX: u64 = 1 << 63; // no-execute

/// Physical-frame address mask (bits 51:12), used for table-pointer entries.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// The kind of access being translated — selects which permission checks apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Execute,
}

/// A translation failure. `addr` is the faulting virtual address; `code` is the
/// architectural page-fault error code (the value the CPU would push for #PF):
/// bit0 P, bit1 W/R, bit2 U/S, bit4 I/D.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFault {
    pub addr: u64,
    pub code: u32,
}

impl PageFault {
    pub const P: u32 = 1 << 0; // fault was a protection violation (entry present)
    pub const WR: u32 = 1 << 1; // access was a write
    pub const US: u32 = 1 << 2; // access was from user mode (CPL==3)
    pub const ID: u32 = 1 << 4; // fault was an instruction fetch
}

/// `true` when the CPU is in IA-32e paging mode (PG && PAE && LMA). Below this
/// the kernel is still in its 16/32-bit bring-up; the seed's flat model applies
/// (virtual == physical) until the kernel turns paging on.
pub fn long_mode_paging(p: &Paging) -> bool {
    p.cr0 & CR0_PG != 0 && p.cr4 & CR4_PAE != 0 && p.efer & EFER_LMA != 0
}

/// Canonical-address check: in long mode bits 63:48 must all equal bit 47.
fn is_canonical(v: u64) -> bool {
    let top = v >> 47;
    top == 0 || top == 0x1_FFFF
}

#[inline]
fn read_u64(mem: &[u8], phys: u64) -> Option<u64> {
    let a = phys as usize;
    let bytes = mem.get(a..a + 8)?;
    Some(u64::from_le_bytes(bytes.try_into().unwrap()))
}

/// Translate a virtual address to physical using the guest's page tables.
///
/// When long-mode paging is not active, translation is the identity (the seed
/// model) — early boot code runs against physical memory directly.
///
/// `cpl_user` is whether the access originates from ring 3 (drives U/S checks
/// and the page-fault error code).
pub fn translate(
    mem: &[u8],
    p: &Paging,
    vaddr: u64,
    access: Access,
    cpl_user: bool,
) -> Result<u64, PageFault> {
    if !long_mode_paging(p) {
        return Ok(vaddr);
    }

    let fault = |present: bool| {
        let mut code = 0u32;
        if present {
            code |= PageFault::P;
        }
        if access == Access::Write {
            code |= PageFault::WR;
        }
        if cpl_user {
            code |= PageFault::US;
        }
        if access == Access::Execute {
            code |= PageFault::ID;
        }
        Err(PageFault { addr: vaddr, code })
    };

    if !is_canonical(vaddr) {
        return fault(false);
    }

    let nxe = p.efer & EFER_NXE != 0;
    // Accumulated permissions: a page is writable / user-accessible / executable
    // only if every level along the walk permits it.
    let mut writable = true;
    let mut user = true;
    let mut executable = true;

    // Walk PML4 → PDPT → PD → PT. `table` is the physical base of the current
    // level; `shift` selects this level's 9-bit index out of the virtual addr.
    let mut table = p.cr3 & ADDR_MASK;
    for level in 0..4 {
        let shift = 39 - level * 9;
        let index = (vaddr >> shift) & 0x1FF;
        let entry = match read_u64(mem, table + index * 8) {
            Some(e) => e,
            None => return fault(false),
        };
        if entry & PTE_P == 0 {
            return fault(false);
        }
        writable &= entry & PTE_RW != 0;
        user &= entry & PTE_US != 0;
        if nxe {
            executable &= entry & PTE_NX == 0;
        }

        // A large page terminates the walk early: 1 GiB at the PDPT level
        // (level 1), 2 MiB at the PD level (level 2).
        let large = entry & PTE_PS != 0 && (level == 1 || level == 2);
        if level == 3 || large {
            // Permission checks against the accumulated bits.
            if access == Access::Write && !writable {
                return fault(true);
            }
            if cpl_user && !user {
                return fault(true);
            }
            if access == Access::Execute && !executable {
                return fault(true);
            }
            let offset_bits = if large {
                if level == 1 { 30 } else { 21 }
            } else {
                12
            };
            let frame = entry & ADDR_MASK & !((1u64 << offset_bits) - 1);
            let offset = vaddr & ((1u64 << offset_bits) - 1);
            return Ok(frame | offset);
        }
        table = entry & ADDR_MASK;
    }
    unreachable!("4-level walk always terminates at level 3")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a buffer big enough for a few tables + target pages.
    fn buf() -> std::vec::Vec<u8> {
        std::vec![0u8; 0x40_0000] // 4 MiB
    }
    fn put(mem: &mut [u8], phys: u64, val: u64) {
        let a = phys as usize;
        mem[a..a + 8].copy_from_slice(&val.to_le_bytes());
    }

    fn lme() -> Paging {
        Paging { cr0: CR0_PG, cr3: 0x1000, cr4: CR4_PAE, efer: EFER_LMA | EFER_LME }
    }

    #[test]
    fn identity_when_paging_off() {
        let mem = buf();
        let p = Paging::default(); // PG off
        assert_eq!(translate(&mem, &p, 0xDEAD_BEEF, Access::Read, false), Ok(0xDEAD_BEEF));
    }

    #[test]
    fn maps_4k_page() {
        let mut mem = buf();
        // Tables: PML4@0x1000, PDPT@0x2000, PD@0x3000, PT@0x4000, page@0x5000.
        put(&mut mem, 0x1000, 0x2000 | PTE_P | PTE_RW);
        put(&mut mem, 0x2000, 0x3000 | PTE_P | PTE_RW);
        put(&mut mem, 0x3000, 0x4000 | PTE_P | PTE_RW);
        put(&mut mem, 0x4000, 0x5000 | PTE_P | PTE_RW);
        let p = lme();
        // vaddr 0 → first entry of every table → frame 0x5000.
        assert_eq!(translate(&mem, &p, 0x123, Access::Read, false), Ok(0x5123));
    }

    #[test]
    fn maps_2m_large_page() {
        let mut mem = buf();
        put(&mut mem, 0x1000, 0x2000 | PTE_P | PTE_RW);
        put(&mut mem, 0x2000, 0x3000 | PTE_P | PTE_RW);
        put(&mut mem, 0x3000, 0x20_0000 | PTE_P | PTE_RW | PTE_PS); // 2 MiB page at PD level
        let p = lme();
        // Offset within the 2 MiB page is preserved.
        assert_eq!(translate(&mem, &p, 0x1F_F000, Access::Read, false), Ok(0x3F_F000));
    }

    #[test]
    fn not_present_faults() {
        let mut mem = buf();
        put(&mut mem, 0x1000, 0x2000 | PTE_P); // PML4 ok
        // PDPT entry left zero → not present.
        let p = lme();
        let err = translate(&mem, &p, 0x40_0000, Access::Read, false).unwrap_err();
        assert_eq!(err.code & PageFault::P, 0); // present bit clear
    }

    #[test]
    fn write_to_readonly_faults() {
        let mut mem = buf();
        put(&mut mem, 0x1000, 0x2000 | PTE_P | PTE_RW);
        put(&mut mem, 0x2000, 0x3000 | PTE_P | PTE_RW);
        put(&mut mem, 0x3000, 0x4000 | PTE_P | PTE_RW);
        put(&mut mem, 0x4000, 0x5000 | PTE_P); // present, NOT writable
        let p = lme();
        assert_eq!(translate(&mem, &p, 0, Access::Read, false), Ok(0x5000));
        let err = translate(&mem, &p, 0, Access::Write, false).unwrap_err();
        assert_eq!(err.code & PageFault::P, PageFault::P); // protection violation
        assert_eq!(err.code & PageFault::WR, PageFault::WR);
    }

    #[test]
    fn user_access_to_supervisor_faults() {
        let mut mem = buf();
        put(&mut mem, 0x1000, 0x2000 | PTE_P | PTE_RW); // no U/S bit
        put(&mut mem, 0x2000, 0x3000 | PTE_P | PTE_RW);
        put(&mut mem, 0x3000, 0x4000 | PTE_P | PTE_RW);
        put(&mut mem, 0x4000, 0x5000 | PTE_P | PTE_RW);
        let p = lme();
        // Supervisor read is fine; user read faults (chain lacks U/S).
        assert_eq!(translate(&mem, &p, 0, Access::Read, false), Ok(0x5000));
        let err = translate(&mem, &p, 0, Access::Read, true).unwrap_err();
        assert_eq!(err.code & PageFault::US, PageFault::US);
    }

    #[test]
    fn noncanonical_faults() {
        let mem = buf();
        let p = lme();
        // bit47 clear but high bits set → non-canonical.
        let err = translate(&mem, &p, 0x0001_0000_0000_0000, Access::Read, false).unwrap_err();
        assert_eq!(err.code & PageFault::P, 0);
    }
}
