//! PCI configuration space — the `CF8`/`CFC` port pair.
//!
//! x86 PCs reach a device's 256-byte config space through two I/O ports:
//! `CONFIG_ADDRESS` (0xCF8) selects bus/device/function/register (with the
//! enable bit set), `CONFIG_DATA` (0xCFC) reads or writes the 32-bit value.
//! That's the whole mechanism — device drivers walk the bus to find their
//! hardware, then program its BARs.

use crate::hal::port::{inl, outl};

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Read a 32-bit config register of `bus`/`dev`/`func`/`reg` (reg is a byte
/// offset, must be 4-aligned).
pub fn read32(bus: u8, dev: u8, func: u8, reg: u8) -> u32 {
    let addr: u32 = 0x8000_0000
        | ((bus as u32) << 16)
        | (((dev as u32) & 0x1F) << 11)
        | (((func as u32) & 0x7) << 8)
        | ((reg as u32) & 0xFC);
    unsafe {
        outl(CONFIG_ADDRESS, addr);
        inl(CONFIG_DATA)
    }
}

/// Write a 32-bit config register (same addressing).
pub fn write32(bus: u8, dev: u8, func: u8, reg: u8, val: u32) {
    let addr: u32 = 0x8000_0000
        | ((bus as u32) << 16)
        | (((dev as u32) & 0x1F) << 11)
        | (((func as u32) & 0x7) << 8)
        | ((reg as u32) & 0xFC);
    unsafe {
        outl(CONFIG_ADDRESS, addr);
        outl(CONFIG_DATA, val);
    }
}

/// A located PCI function (bus/device numbers plus its class codes).
#[derive(Debug, Clone, Copy)]
pub struct PciFunction {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
}

/// Scan all buses/devices/functions and return the first function matching
/// `vendor`/`device` (any of `devices` accepted). Skips non-present slots
/// (vendor `0xFFFF`).
pub fn find(vendor: u16, devices: &[u16]) -> Option<PciFunction> {
    for bus in 0..=255u8 {
        for dev in 0..32u8 {
            for func in 0..8u8 {
                let id = read32(bus, dev, func, 0);
                if id == 0xFFFF_FFFF || id as u16 == 0xFFFF {
                    continue;
                }
                let (v, d) = (id as u16, (id >> 16) as u16);
                if v == vendor && devices.contains(&d) {
                    return Some(PciFunction { bus, dev, func, vendor: v, device: d });
                }
            }
        }
    }
    None
}

/// Read a BAR's 32-bit value (BAR index 0..6).
pub fn read_bar(f: &PciFunction, bar: u8) -> u32 {
    read32(f.bus, f.dev, f.func, 0x10 + bar * 4)
}

/// Write a BAR's 32-bit value.
pub fn write_bar(f: &PciFunction, bar: u8, val: u32) {
    write32(f.bus, f.dev, f.func, 0x10 + bar * 4, val);
}

/// Enable bus mastering and I/O-space access in the device's command
/// register (offset 0x04), the two enables a port-driven virtio device
/// needs.
pub fn enable_bus_master_and_io(f: &PciFunction) {
    let cmd = read32(f.bus, f.dev, f.func, 0x04);
    write32(f.bus, f.dev, f.func, 0x04, cmd | 0x5); // BusMaster(2) | IoSpace(1)
}
