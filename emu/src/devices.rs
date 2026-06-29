//! The minimal device set ntoskrnl-rs touches when booted directly in long
//! mode: a 16550 UART (COM1, the console), a Local APIC with its timer (the
//! scheduler tick, vector 0xD1), and a PS/2 keyboard (interactive input).
//!
//! qemu-wasm/v86 emulate a whole PC chipset; we emulate only these three. The
//! UART and PS/2 are reached through port I/O (`in`/`out`); the Local APIC is
//! memory-mapped at [`APIC_BASE`]. The host (JS shim) drains `uart.tx` to the
//! terminal and pushes keystrokes into `ps2`/`uart` input queues.

extern crate alloc;
use alloc::collections::VecDeque;

/// COM1 base port. The 8 registers live at 0x3F8..=0x3FF.
pub const COM1_BASE: u16 = 0x3F8;
/// Local APIC MMIO base (physical). The kernel maps this and pokes the timer.
pub const APIC_BASE: u64 = 0xFEE0_0000;
pub const APIC_SIZE: u64 = 0x1000;

/// 16550 UART. We model just enough for a console: a transmit byte sink and a
/// receive queue, with a Line Status Register that always reports "ready to
/// send" and reflects whether a byte is waiting to be read.
#[derive(Default, Clone)]
pub struct Uart {
    /// Bytes the guest wrote to the transmit holding register — the host drains
    /// this to the terminal.
    pub tx: VecDeque<u8>,
    /// Bytes the host queued from the terminal for the guest to read.
    pub rx: VecDeque<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
}

// 16550 register offsets from the base port.
const UART_THR_RBR: u16 = 0; // write: transmit; read: receive
const UART_IER: u16 = 1;
const UART_IIR_FCR: u16 = 2;
const UART_LCR: u16 = 3;
const UART_MCR: u16 = 4;
const UART_LSR: u16 = 5;
const UART_MSR: u16 = 6;

const LSR_DR: u8 = 1 << 0; // data ready (a received byte is available)
const LSR_THRE: u8 = 1 << 5; // transmit holding register empty
const LSR_TEMT: u8 = 1 << 6; // transmitter empty

impl Uart {
    pub fn new() -> Self {
        Uart::default()
    }
    pub fn read(&mut self, off: u16) -> u8 {
        match off {
            UART_THR_RBR => self.rx.pop_front().unwrap_or(0),
            UART_IER => self.ier,
            UART_IIR_FCR => 0x01, // no interrupt pending
            UART_LCR => self.lcr,
            UART_MCR => self.mcr,
            UART_LSR => {
                let dr = if self.rx.is_empty() { 0 } else { LSR_DR };
                LSR_THRE | LSR_TEMT | dr // always ready to transmit
            }
            UART_MSR => 0xB0, // DCD|DSR|CTS asserted
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u16, val: u8) {
        match off {
            UART_THR_RBR => self.tx.push_back(val),
            UART_IER => self.ier = val,
            UART_LCR => self.lcr = val,
            UART_MCR => self.mcr = val,
            _ => {}
        }
    }
    /// Host pushes a received byte for the guest.
    pub fn push_rx(&mut self, b: u8) {
        self.rx.push_back(b);
    }
}

/// Local APIC. A small register file plus a countdown timer. The kernel programs
/// the timer (initial count + divide + an LVT entry naming the vector) and we
/// raise that vector when the count reaches zero (reloading if periodic).
#[derive(Default, Clone)]
pub struct Apic {
    pub id: u32,
    pub svr: u32,        // spurious vector register (bit8 = APIC enable)
    pub tpr: u32,        // task priority
    pub lvt_timer: u32,  // bits 0..7 vector, bit16 mask, bit17 periodic
    pub initial_count: u32,
    pub current_count: u32,
    pub divide_config: u32,
    /// A pending vector raised by the timer, awaiting injection by the machine.
    pub pending_vector: Option<u8>,
}

// Local APIC register offsets.
const APIC_ID: u64 = 0x20;
const APIC_VERSION: u64 = 0x30;
const APIC_TPR: u64 = 0x80;
const APIC_EOI: u64 = 0xB0;
const APIC_SVR: u64 = 0xF0;
const APIC_LVT_TIMER: u64 = 0x320;
const APIC_TIMER_INIT: u64 = 0x380;
const APIC_TIMER_CUR: u64 = 0x390;
const APIC_TIMER_DIV: u64 = 0x3E0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_PERIODIC: u32 = 1 << 17;

impl Apic {
    pub fn new() -> Self {
        Apic::default()
    }
    /// Divider encoded in bits {0,1,3} of the divide-config register (the usual
    /// APIC encoding); returns the divisor (1,2,4,...,128).
    fn divisor(&self) -> u32 {
        let e = ((self.divide_config & 0b11) | ((self.divide_config & 0b1000) >> 1)) as u32;
        match e {
            0b000 => 2,
            0b001 => 4,
            0b010 => 8,
            0b011 => 16,
            0b100 => 32,
            0b101 => 64,
            0b110 => 128,
            _ => 1, // 0b111 = divide by 1
        }
    }
    pub fn read(&self, off: u64) -> u32 {
        match off {
            APIC_ID => self.id << 24,
            APIC_VERSION => 0x0001_0014, // version 0x14, max LVT 1
            APIC_TPR => self.tpr,
            APIC_SVR => self.svr,
            APIC_LVT_TIMER => self.lvt_timer,
            APIC_TIMER_INIT => self.initial_count,
            APIC_TIMER_CUR => self.current_count,
            APIC_TIMER_DIV => self.divide_config,
            _ => 0,
        }
    }
    pub fn write(&mut self, off: u64, val: u32) {
        match off {
            APIC_ID => self.id = val >> 24,
            APIC_TPR => self.tpr = val,
            APIC_EOI => {} // end-of-interrupt: acknowledge (no nested state modeled)
            APIC_SVR => self.svr = val,
            APIC_LVT_TIMER => self.lvt_timer = val,
            APIC_TIMER_INIT => {
                self.initial_count = val;
                self.current_count = val; // writing the initial count (re)starts the timer
            }
            APIC_TIMER_DIV => self.divide_config = val,
            _ => {}
        }
    }
    /// Advance the timer by `cycles` retired instructions. Returns the vector to
    /// inject if the count crossed zero and the LVT is unmasked.
    pub fn tick(&mut self, cycles: u32) -> Option<u8> {
        if self.lvt_timer & LVT_MASKED != 0 || self.initial_count == 0 {
            return None;
        }
        let dec = cycles / self.divisor().max(1);
        if dec == 0 {
            return None;
        }
        if self.current_count > dec {
            self.current_count -= dec;
            return None;
        }
        // Reached zero.
        let vector = (self.lvt_timer & 0xFF) as u8;
        if self.lvt_timer & LVT_PERIODIC != 0 {
            self.current_count = self.initial_count;
        } else {
            self.current_count = 0;
            self.initial_count = 0; // one-shot: stop
        }
        self.pending_vector = Some(vector);
        Some(vector)
    }

    /// Force an armed timer to expire now (used to wake a `hlt` waiting on the
    /// next tick). Returns the vector if a timer was armed and unmasked.
    pub fn expire(&mut self) -> Option<u8> {
        if self.lvt_timer & LVT_MASKED != 0 || self.initial_count == 0 {
            return None;
        }
        let vector = (self.lvt_timer & 0xFF) as u8;
        if self.lvt_timer & LVT_PERIODIC != 0 {
            self.current_count = self.initial_count;
        } else {
            self.current_count = 0;
            self.initial_count = 0;
        }
        self.pending_vector = Some(vector);
        Some(vector)
    }
}

/// PS/2 keyboard controller (8042), data port 0x60 / status+command port 0x64.
#[derive(Default, Clone)]
pub struct Ps2 {
    queue: VecDeque<u8>,
}
const PS2_STATUS_OBF: u8 = 1 << 0; // output buffer full (a byte is readable at 0x60)

impl Ps2 {
    pub fn new() -> Self {
        Ps2::default()
    }
    pub fn read_data(&mut self) -> u8 {
        self.queue.pop_front().unwrap_or(0)
    }
    pub fn read_status(&self) -> u8 {
        if self.queue.is_empty() { 0 } else { PS2_STATUS_OBF }
    }
    /// Host pushes a scancode.
    pub fn push_scancode(&mut self, code: u8) {
        self.queue.push_back(code);
    }
}

/// The whole device set, owned by the machine.
#[derive(Clone)]
pub struct Devices {
    pub uart: Uart,
    pub apic: Apic,
    pub ps2: Ps2,
}

impl Default for Devices {
    fn default() -> Self {
        Devices { uart: Uart::new(), apic: Apic::new(), ps2: Ps2::new() }
    }
}

impl Devices {
    pub fn new() -> Self {
        Devices::default()
    }

    /// Port-input (`in`) dispatch. Unknown ports read as all-ones (the bus
    /// floats high) which is what real hardware-probing code expects.
    pub fn port_in(&mut self, port: u16, size: u8) -> u64 {
        match port {
            0x3F8..=0x3FF => self.uart.read(port - COM1_BASE) as u64,
            0x60 => self.ps2.read_data() as u64,
            0x64 => self.ps2.read_status() as u64,
            _ => {
                let bits = size as u64 * 8;
                if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
            }
        }
    }
    /// Port-output (`out`) dispatch.
    pub fn port_out(&mut self, port: u16, val: u64, _size: u8) {
        match port {
            0x3F8..=0x3FF => self.uart.write(port - COM1_BASE, val as u8),
            // 0x60/0x64 keyboard writes (commands/LED): accept and ignore.
            _ => {}
        }
    }

    /// `true` if a physical address falls in the Local APIC MMIO window.
    pub fn is_apic_mmio(&self, phys: u64) -> bool {
        (APIC_BASE..APIC_BASE + APIC_SIZE).contains(&phys)
    }
    pub fn apic_read(&self, phys: u64) -> u64 {
        self.apic.read(phys - APIC_BASE) as u64
    }
    pub fn apic_write(&mut self, phys: u64, val: u64) {
        self.apic.write(phys - APIC_BASE, val as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uart_tx_collects_output() {
        let mut d = Devices::new();
        for &b in b"OK" {
            d.port_out(COM1_BASE, b as u64, 1);
        }
        assert_eq!(d.uart.tx.iter().copied().collect::<alloc::vec::Vec<_>>(), b"OK");
    }

    #[test]
    fn uart_lsr_reports_ready_and_data() {
        let mut d = Devices::new();
        // Nothing queued: THRE set, DR clear.
        assert_eq!(d.port_in(COM1_BASE + 5, 1) as u8 & LSR_DR, 0);
        d.uart.push_rx(b'x');
        assert_eq!(d.port_in(COM1_BASE + 5, 1) as u8 & LSR_DR, LSR_DR);
        assert_eq!(d.port_in(COM1_BASE, 1) as u8, b'x'); // read it back
        assert_eq!(d.port_in(COM1_BASE + 5, 1) as u8 & LSR_DR, 0); // drained
    }

    #[test]
    fn apic_timer_one_shot_fires_vector() {
        let mut d = Devices::new();
        d.apic_write(APIC_BASE + APIC_TIMER_DIV, 0xB); // divide by 1
        d.apic_write(APIC_BASE + APIC_LVT_TIMER, 0xD1); // vector 0xD1, not masked, one-shot
        d.apic_write(APIC_BASE + APIC_TIMER_INIT, 100);
        assert_eq!(d.apic.tick(40), None);
        assert_eq!(d.apic.tick(40), None);
        assert_eq!(d.apic.tick(40), Some(0xD1)); // crossed zero
        assert_eq!(d.apic.tick(40), None); // one-shot stopped
    }

    #[test]
    fn apic_timer_periodic_reloads() {
        let mut d = Devices::new();
        d.apic_write(APIC_BASE + APIC_TIMER_DIV, 0xB);
        d.apic_write(APIC_BASE + APIC_LVT_TIMER, (0xD1u32 | LVT_PERIODIC) as u64);
        d.apic_write(APIC_BASE + APIC_TIMER_INIT, 10);
        assert_eq!(d.apic.tick(10), Some(0xD1));
        assert_eq!(d.apic.tick(10), Some(0xD1)); // reloaded and fired again
    }

    #[test]
    fn apic_mmio_window() {
        let d = Devices::new();
        assert!(d.is_apic_mmio(APIC_BASE));
        assert!(d.is_apic_mmio(APIC_BASE + 0x320));
        assert!(!d.is_apic_mmio(APIC_BASE + APIC_SIZE));
        assert!(!d.is_apic_mmio(0x1000));
    }

    #[test]
    fn ps2_scancode_queue() {
        let mut d = Devices::new();
        assert_eq!(d.port_in(0x64, 1) as u8 & PS2_STATUS_OBF, 0);
        d.ps2.push_scancode(0x1C);
        assert_eq!(d.port_in(0x64, 1) as u8 & PS2_STATUS_OBF, PS2_STATUS_OBF);
        assert_eq!(d.port_in(0x60, 1) as u8, 0x1C);
    }

    #[test]
    fn unknown_port_floats_high() {
        let mut d = Devices::new();
        assert_eq!(d.port_in(0xCFC, 4), 0xFFFF_FFFF);
    }
}
