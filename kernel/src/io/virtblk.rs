//! virtio-blk — a block device over the legacy (transitional) virtio PCI
//! interface. This is the storage stack's foundation: one virtqueue, one
//! sector at a time, synchronous completion.
//!
//! The shape, per the virtio 0.9 legacy spec: find the device on the PCI
//! bus (vendor `0x1AF4`, device `0x1001`), take its BAR0 I/O port window,
//! reset → ACKNOWLEDGE → DRIVER → FEATURES_OK → publish one vring
//! (descriptor table + available ring + used ring in guest RAM, addressed
//! by page frame) → DRIVER_OK. A request is a 3-descriptor chain
//! (16-byte header, 512-byte data, 1-byte status); the device DMAs to and
//! from *physical* addresses and flips `used.idx` when done.

use crate::hal::{pci, port};
use crate::ke::spinlock::SpinLock;
use crate::mm::{self, PhysAddr};

// Legacy virtio register offsets (from the I/O BAR).
const REG_HOST_FEATURES: u16 = 0x00;
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_ADDRESS: u16 = 0x08;
const REG_QUEUE_NUM: u16 = 0x0C;
const REG_QUEUE_SEL: u16 = 0x0E;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_DEVICE_STATUS: u16 = 0x12;
const REG_ISR_STATUS: u16 = 0x13;
const REG_CAPACITY: u16 = 0x14; // u32 (0.9) / low u32 of u64 (1.0 legacy)
const REG_CAPACITY_HI: u16 = 0x18;

// DeviceStatus bits.
const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_DRIVER_OK: u8 = 4;

// Descriptor flags.
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

// Request types.
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;

const SECTOR: usize = 512;

struct VirtBlk {
    io_base: u16,
    /// The vring's page frame number in guest RAM.
    queue_pfn: u32,
    /// The device-reported queue depth (drives all ring offsets).
    queue_num: u16,
    /// Virtual address of the vring (descriptor table at offset 0).
    vram: *mut u8,
    /// Total sectors reported by the device.
    capacity: u64,
}
// SAFETY: the vring pages are pool-backed and only touched under BLK's lock.
unsafe impl Send for VirtBlk {}

static BLK: SpinLock<Option<VirtBlk>> = SpinLock::new(None);

/// Request scratch (header + status), in the physical window like every
/// other kernel allocation, so the device can DMA it directly.
#[repr(C, align(16))]
struct Scratch {
    hdr: [u8; 16],
    status: u8,
}
static mut SCRATCH: Scratch = Scratch { hdr: [0; 16], status: 0xFF };

/// Ring offsets inside the vring for a device-reported queue depth `n`
/// (descriptor table at 0, available ring right after, used ring on the
/// next page boundary — the device computes these from ITS queue num).
const fn avail_off(n: u16) -> usize {
    16 * n as usize
}
const fn used_off(n: u16) -> usize {
    (avail_off(n) + 6 + 2 * n as usize + 4095) & !4095
}
/// Total vring bytes for depth `n`, page-rounded.
const fn vring_pages(n: u16) -> usize {
    (used_off(n) + 6 + 8 * n as usize + 4095) / 4096
}

fn status(v: &VirtBlk, s: u8) {
    unsafe { port::outb(v.io_base + REG_DEVICE_STATUS, s) };
}

/// Probe and initialize the device. Returns false (absent) without side
/// effects, true with the queue live.
pub fn init() -> bool {
    let Some(f) = pci::find(0x1AF4, &[0x1001]) else { return false };
    let bar = pci::read_bar(&f, 0);
    if bar & 1 == 0 {
        return false; // an MMIO BAR would need a different register map
    }
    let io_base = (bar & !3) as u16;
    pci::enable_bus_master_and_io(&f);

    let mut v = VirtBlk { io_base, queue_pfn: 0, queue_num: 0, vram: core::ptr::null_mut(), capacity: 0 };
    status(&v, 0);
    // Legacy reset is asynchronous: wait for the device to report it done
    // (status reads back 0) before programming anything, or the queue setup
    // races the reset.
    {
        let mut spins: u64 = 0;
        while unsafe { port::inb(io_base + REG_DEVICE_STATUS) } != 0 {
            spins += 1;
            if spins > 100_000_000 {
                return false;
            }
            core::hint::spin_loop();
        }
    }
    status(&v, STATUS_ACK);
    status(&v, STATUS_ACK | STATUS_DRIVER);
    // Features: accept everything the host offers (a zero acceptance is
    // legal, but some device implementations sulk at it).
    unsafe {
        let host = port::inl(io_base + REG_HOST_FEATURES);
        port::outl(io_base + REG_GUEST_FEATURES, host);
    }
    status(&v, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
    unsafe {
        port::outw(io_base + REG_QUEUE_SEL, 0);
        let n = port::inw(io_base + REG_QUEUE_NUM);
        if n == 0 {
            return false;
        }
        let Some(vram_pa) = mm::phys::mm_allocate_contiguous_pages(vring_pages(n)) else {
            return false;
        };
        let vram = mm::phys_to_virt(vram_pa);
        v.queue_num = n;
        v.queue_pfn = (vram_pa.0 >> 12) as u32;
        v.vram = vram;
        port::outl(io_base + REG_QUEUE_ADDRESS, v.queue_pfn);
        let lo = port::inl(io_base + REG_CAPACITY) as u64;
        let hi = port::inl(io_base + REG_CAPACITY_HI) as u64;
        v.capacity = (hi << 32) | lo;
    }
    status(&v, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    crate::kd_println!(
        "VBLK: virtio-blk online, queue depth {}, {} sectors ({} MiB)",
        v.queue_num,
        v.capacity,
        v.capacity / 2048
    );
    *BLK.lock() = Some(v);
    true
}

/// Total sectors (0 when no device is present).
pub fn capacity_sectors() -> u64 {
    BLK.lock().as_ref().map_or(0, |v| v.capacity)
}

/// Physical address of a window-backed virtual address (DMA translation).
fn phys(va: u64) -> u64 {
    mm::virt::mm_get_physical_address(va).map_or(0, |p: PhysAddr| p.0)
}

/// One descriptor (16 bytes) at `vram + i*16`. Writes are volatile: this
/// memory is shared with the device.
unsafe fn set_desc(vram: *mut u8, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
    unsafe {
        let d = vram.add(i * 16) as *mut u64;
        d.write_volatile(addr);
        (d as *mut u32).add(2).write_volatile(len);
        (d as *mut u16).add(6).write_volatile(flags);
        (d as *mut u16).add(7).write_volatile(next);
    }
}

/// Run one 3-descriptor request synchronously. Returns the device status
/// byte (0 = `VIRTIO_BLK_S_OK`).
fn request(v: &VirtBlk, rtype: u32, lba: u64, buf: *mut u8) -> u8 {
    unsafe {
        // Snapshot the used index BEFORE publishing anything: a single-sector
        // request can complete within microseconds of the doorbell, so reading
        // the baseline after the notify races with the device's own update and
        // the poll below would wait for a second advance that never comes.
        let used = v.vram.add(used_off(v.queue_num));
        let used_idx_ptr = used.add(2) as *mut u16;
        let start = used_idx_ptr.read_volatile();

        // Header: type + reserved + sector.
        let hdr = (&raw mut SCRATCH) as *mut u8;
        core::ptr::write_bytes(hdr, 0, 16);
        *(hdr as *mut u32) = rtype;
        *((hdr as *mut u32).add(1)) = 0;
        *((hdr as *mut u64).add(1)) = lba;
        let status_va = hdr.add(16) as u64;

        set_desc(v.vram, 0, phys(hdr as u64), 16, DESC_F_NEXT, 1);
        // The data buffer is device-writable only for a read (T_IN); for a
        // write (T_OUT) the device reads it.
        let data_flags = if rtype == VIRTIO_BLK_T_IN { DESC_F_NEXT | DESC_F_WRITE } else { DESC_F_NEXT };
        set_desc(v.vram, 1, phys(buf as u64), SECTOR as u32, data_flags, 2);
        set_desc(v.vram, 2, phys(status_va), 1, DESC_F_WRITE, 0);

        // Available ring: publish descriptor 0 at ring[idx % N], then idx++.
        // All ring access is volatile — the device shares this memory.
        let avail = v.vram.add(avail_off(v.queue_num));
        let idx_ptr = avail.add(2) as *mut u16;
        let idx = idx_ptr.read_volatile();
        let ring = avail.add(4) as *mut u16;
        ring.add((idx % v.queue_num) as usize).write_volatile(0);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        idx_ptr.write_volatile(idx.wrapping_add(1));
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        port::outw(v.io_base + REG_QUEUE_NOTIFY, 0);

        // Wait for the used ring to advance (bounded; the device answers a
        // single-sector request essentially immediately).
        let mut spins: u64 = 0;
        while used_idx_ptr.read_volatile() == start {
            spins += 1;
            if spins > 2_000_000_000 {
                return 0xFE; // device wedged
            }
            core::hint::spin_loop();
        }
        // Consume the interrupt-pending bit, if any.
        let _isr = port::inb(v.io_base + REG_ISR_STATUS);
        (&raw const SCRATCH).cast::<u8>().add(16).read()
    }
}

/// Read one 512-byte sector into `buf`. False when there is no device or
/// the device reports an error.
pub fn read_sector(lba: u64, buf: &mut [u8; SECTOR]) -> bool {
    let mut g = BLK.lock();
    let Some(v) = g.as_mut() else { return false };
    let st = request(v, VIRTIO_BLK_T_IN, lba, buf.as_mut_ptr());
    st == 0
}

/// Write one 512-byte sector from `buf`. False on no device/error.
pub fn write_sector(lba: u64, buf: &[u8; SECTOR]) -> bool {
    let mut g = BLK.lock();
    let Some(v) = g.as_mut() else { return false };
    let st = request(v, VIRTIO_BLK_T_OUT, lba, buf.as_ptr() as *mut u8);
    st == 0
}
