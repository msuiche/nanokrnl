//! Build the `bootloader_api::BootInfo` handoff structure with the exact byte
//! layout the kernel (compiled for x86-64, `bootloader_api` 0.11.15) expects.
//!
//! The layout is hand-encoded with explicit 64-bit fields so it is identical
//! regardless of the emulator's *own* architecture — crucial because the wasm
//! build is 32-bit (`usize`/pointers are 4 bytes there) while the guest kernel
//! is 64-bit. The offsets below were extracted from the real crate type on an
//! x86-64 host (`offset_of!`); see SPEC.md.
//!
//! ```text
//! offset  field                              encoding
//!   0     api_version (ApiVersion, 8 bytes)  0.11.15 → 00 00 0b 00 0f 00 00 00
//!   8     memory_regions.ptr  (u64)          guest vaddr of the region array
//!  16     memory_regions.len  (u64)
//!  24     framebuffer         Optional<..>   None  (disc=1)
//!  88     physical_memory_offset Optional<u64>  Some (disc=0 @88, value @96)
//! 104     recursive_index     Optional<u16>  None
//! 112     rsdp_addr           Optional<u64>  Some/None
//! 128     tls_template        Optional<..>   None
//! 160     ramdisk_addr        Optional<u64>  None
//! 176     ramdisk_len         u64
//! 184     kernel_addr         u64
//! 192     kernel_len          u64
//! 200     kernel_image_offset u64
//! 208     kernel_stack_bottom u64
//! 216     kernel_stack_len    u64
//! 224     _test_sentinel      u64
//! 232     (size)
//! ```
//! `Optional<T>` is a `#[repr(C)]` enum: a 4-byte discriminant (Some=0, None=1)
//! followed by the payload at the type's natural alignment. `MemoryRegion` is
//! `{ start: u64, end: u64, kind }` (24 bytes); `kind` is a 4-byte discriminant
//! (Usable=0, Bootloader=1) at offset 16.

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

const BOOTINFO_SIZE: usize = 232;
const REGION_SIZE: usize = 24;

/// A memory-map entry to report to the kernel.
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub usable: bool,
}

/// Parameters the bootloader would normally fill in.
pub struct HandoffParams {
    pub physical_memory_offset: u64,
    pub kernel_image_offset: u64,
    pub kernel_addr: u64,
    pub kernel_len: u64,
    pub kernel_stack_bottom: u64,
    pub kernel_stack_len: u64,
    pub rsdp_addr: Option<u64>,
    /// Guest virtual address where the `MemoryRegion` array will be placed.
    pub regions_vaddr: u64,
}

fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Returns `(bootinfo_bytes, regions_bytes)` — the caller writes them at the
/// BootInfo / regions virtual addresses and points RDI at the BootInfo.
pub fn build(params: &HandoffParams, regions: &[Region]) -> (Vec<u8>, Vec<u8>) {
    let mut bi = vec![0u8; BOOTINFO_SIZE];
    // api_version: 0.11.15 (arch-independent value).
    bi[0..8].copy_from_slice(&[0x00, 0x00, 0x0b, 0x00, 0x0f, 0x00, 0x00, 0x00]);
    // memory_regions { ptr, len }
    put64(&mut bi, 8, params.regions_vaddr);
    put64(&mut bi, 16, regions.len() as u64);
    // framebuffer: None
    put32(&mut bi, 24, 1);
    // physical_memory_offset: Some(offset)
    put32(&mut bi, 88, 0);
    put64(&mut bi, 96, params.physical_memory_offset);
    // recursive_index: None
    put32(&mut bi, 104, 1);
    // rsdp_addr: Some/None
    match params.rsdp_addr {
        Some(r) => {
            put32(&mut bi, 112, 0);
            put64(&mut bi, 120, r);
        }
        None => put32(&mut bi, 112, 1),
    }
    // tls_template: None
    put32(&mut bi, 128, 1);
    // ramdisk_addr: None ; ramdisk_len: 0
    put32(&mut bi, 160, 1);
    // kernel_* fields
    put64(&mut bi, 184, params.kernel_addr);
    put64(&mut bi, 192, params.kernel_len);
    put64(&mut bi, 200, params.kernel_image_offset);
    put64(&mut bi, 208, params.kernel_stack_bottom);
    put64(&mut bi, 216, params.kernel_stack_len);

    let mut rb = vec![0u8; regions.len() * REGION_SIZE];
    for (i, r) in regions.iter().enumerate() {
        let o = i * REGION_SIZE;
        put64(&mut rb, o, r.start);
        put64(&mut rb, o + 8, r.end);
        put32(&mut rb, o + 16, if r.usable { 0 } else { 1 }); // Usable=0, Bootloader=1
    }
    (bi, rb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_sizes() {
        let regions = [Region { start: 0x1000, end: 0x2000, usable: true }];
        let params = HandoffParams {
            physical_memory_offset: 0xFFFF_FF00_0000_0000,
            kernel_image_offset: 0xFFFF_8000_0000_0000,
            kernel_addr: 0x80_0000,
            kernel_len: 0x1000,
            kernel_stack_bottom: 0xFFFF_8000_4000_0000,
            kernel_stack_len: 0x40000,
            rsdp_addr: None,
            regions_vaddr: 0xFFFF_8000_5001_0000,
        };
        let (bi, rb) = build(&params, &regions);
        assert_eq!(bi.len(), 232);
        assert_eq!(rb.len(), 24);
        // physical_memory_offset Some(value) at 88/96.
        assert_eq!(&bi[88..92], &[0, 0, 0, 0]); // disc Some
        assert_eq!(u64::from_le_bytes(bi[96..104].try_into().unwrap()), 0xFFFF_FF00_0000_0000);
        // memory_regions ptr/len.
        assert_eq!(u64::from_le_bytes(bi[8..16].try_into().unwrap()), 0xFFFF_8000_5001_0000);
        assert_eq!(u64::from_le_bytes(bi[16..24].try_into().unwrap()), 1);
        // region usable kind = 0.
        assert_eq!(u32::from_le_bytes(rb[16..20].try_into().unwrap()), 0);
    }
}
