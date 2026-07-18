//! ACPI table discovery — just enough to enumerate processors.
//!
//! NT's HAL builds its whole interrupt/power model on ACPI. We need exactly
//! one table today: the **MADT** ("APIC" signature), whose Processor Local
//! APIC entries list every logical processor in the machine — the input to
//! `ke::smp`'s AP startup.
//!
//! Discovery follows the spec's BIOS path: the RSDP ("RSD PTR ") lives in
//! the EBDA's first KiB or in `0xE0000..0x100000`; it points at the XSDT
//! (revision ≥ 2) or RSDT, whose entries are physical addresses of the
//! other tables. Everything is read through the physical-memory window;
//! every structure is signature- *and* checksum-validated, so a machine
//! without ACPI (nanox) cleanly reports zero processors instead of
//! parsing garbage.

use crate::mm::{phys_to_virt, PhysAddr};

/// One discovered logical processor (a MADT Processor Local APIC entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Processor {
    /// ACPI processor UID (matches `_UID`/`Processor()` declaration order).
    pub uid: u8,
    /// The local APIC ID — the address IPIs are sent to.
    pub apic_id: u8,
}

/// Maximum processors we record (MADT may list more; the excess is ignored).
pub const MAX_CPUS: usize = 8;

/// Parsed result: up to [`MAX_CPUS`] processors.
pub struct Madt {
    pub cpus: [Processor; MAX_CPUS],
    pub count: usize,
}

fn read_phys<T: Copy>(pa: u64) -> T {
    // SAFETY: `pa` is a firmware table address inside the phys window,
    // page-aligned reads of POD data; T is a plain integer/byte array.
    unsafe { (phys_to_virt(PhysAddr(pa)) as *const T).read_unaligned() }
}

/// ACPI checksum: every byte of the structure must sum to 0 mod 256.
fn checksum_ok(pa: u64, len: usize) -> bool {
    let mut sum = 0u8;
    for i in 0..len as u64 {
        sum = sum.wrapping_add(read_phys::<u8>(pa + i));
    }
    sum == 0
}

fn sig_at(pa: u64) -> [u8; 4] {
    read_phys::<[u8; 4]>(pa)
}

/// Find the RSDP. Returns its physical address.
fn find_rsdp() -> Option<u64> {
    // EBDA base: the BDA word at 0x40E is a real-mode segment.
    let ebda_seg = read_phys::<u16>(0x40E) as u64;
    let ebda = ebda_seg << 4;
    let scan = |base: u64, len: u64| -> Option<u64> {
        let mut p = base;
        while p + 20 <= base + len {
            if read_phys::<[u8; 8]>(p) == *b"RSD PTR " && checksum_ok(p, 20) {
                return Some(p);
            }
            p += 16; // RSDP is always 16-byte aligned
        }
        None
    };
    if (0x8000..0xA0000).contains(&ebda) {
        if let Some(p) = scan(ebda, 1024) {
            return Some(p);
        }
    }
    scan(0xE0000, 0x20000)
}

/// Parse the MADT and return the processor list, or `None` when the machine
/// has no usable ACPI tables (nanox) — the caller then stays uniprocessor.
pub fn processors() -> Option<Madt> {
    let rsdp = find_rsdp()?;
    // RSDP: checksum @8, OEMID @9..15, revision @15, RSDT @16, length @20,
    // XSDT @24, extended checksum @32.
    let revision = read_phys::<u8>(rsdp + 15);
    let (table_ptr_size, root) = if revision >= 2 {
        // XSDT: 8-byte entries; the full RSDP (length field at +20) must
        // checksum over its whole length.
        let len = read_phys::<u32>(rsdp + 20) as usize;
        if len < 36 || !checksum_ok(rsdp, len) {
            return None;
        }
        (8u64, read_phys::<u64>(rsdp + 24))
    } else {
        (4u64, read_phys::<u32>(rsdp + 16) as u64)
    };
    if root == 0 {
        return None;
    }
    let root_sig = sig_at(root);
    let root_len = read_phys::<u32>(root + 4) as usize;
    if root_sig != *b"XSDT" && root_sig != *b"RSDT" {
        return None;
    }
    if root_len < 36 || !checksum_ok(root, root_len) {
        return None;
    }

    // Walk the root's entries looking for the MADT.
    let entries = (root_len - 36) / table_ptr_size as usize;
    let mut madt_pa = None;
    for i in 0..entries {
        let entry_pa = root + 36 + (i as u64) * table_ptr_size;
        let table = if table_ptr_size == 8 {
            read_phys::<u64>(entry_pa)
        } else {
            read_phys::<u32>(entry_pa) as u64
        };
        if table != 0 && sig_at(table) == *b"APIC" {
            madt_pa = Some(table);
            break;
        }
    }
    let madt = madt_pa?;
    let madt_len = read_phys::<u32>(madt + 4) as usize;
    if madt_len < 44 || !checksum_ok(madt, madt_len) {
        return None;
    }

    // Entries start at +44 (36-byte header + 4-byte LAPIC address + 4-byte
    // flags). Type 0 = Processor Local APIC: len 8, uid at +2, id at +3,
    // flags at +4 (bit 0: enabled, bit 1: online-capable).
    let mut out = Madt { cpus: [Processor { uid: 0, apic_id: 0 }; MAX_CPUS], count: 0 };
    let mut p = madt + 44;
    let end = madt + madt_len as u64;
    while p + 2 <= end {
        let kind = read_phys::<u8>(p);
        let len = read_phys::<u8>(p + 1) as u64;
        if len < 2 || p + len > end {
            break;
        }
        if kind == 0 && len >= 8 && out.count < MAX_CPUS {
            let flags = read_phys::<u32>(p + 4);
            if flags & 1 != 0 {
                out.cpus[out.count] = Processor {
                    uid: read_phys::<u8>(p + 2),
                    apic_id: read_phys::<u8>(p + 3),
                };
                out.count += 1;
            }
        }
        p += len;
    }
    Some(out)
}
