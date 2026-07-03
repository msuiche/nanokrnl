//! A transport-agnostic GDB Remote Serial Protocol (RSP) stub for nanox.
//!
//! It is pure bytes-in / bytes-out: [`GdbStub::on_input`] consumes bytes from a
//! debugger (over TCP natively, or a WebSocket bridge in the browser), mutates
//! the [`Machine`], and queues reply bytes in [`GdbStub::out`]. The driver loop
//! runs the machine while [`GdbStub::running`] is set and calls
//! [`GdbStub::report_stop`] whenever [`Machine::run`] returns, so the debugger
//! sees a stop reply on a breakpoint, a step, or a fault.
//!
//! Scope: the core protocol a real client needs to be useful — read/write
//! registers and memory, software breakpoints (emulator-side, no INT3 patching),
//! single-step, continue, and interrupt (Ctrl-C). Registers and memory are the
//! genuine guest state; memory is translated through the guest page tables, so
//! `x/i $rip` and `db <kernel va>` work. lldb is supported via a `target.xml`
//! served over `qXfer:features:read`.

extern crate alloc;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::machine::Machine;
use crate::mmu::{self, Access};

/// GDB `g`-packet general-purpose register order mapped to nanox `regs[]`
/// indices. GDB order is rax,rbx,rcx,rdx,rsi,rdi,rbp,rsp,r8..r15; nanox stores
/// them in x86 encoding order (rax,rcx,rdx,rbx,rsp,rbp,rsi,rdi,r8..r15).
const GDB_GP: [usize; 16] = [0, 3, 1, 2, 6, 7, 5, 4, 8, 9, 10, 11, 12, 13, 14, 15];

/// The x86-64 core register description served to lldb/gdb over qXfer. Order and
/// sizes must match the `g`/`G` packet layout below (16 GP + rip + eflags + 6
/// segment selectors = 164 bytes).
const TARGET_XML: &str = concat!(
    "<?xml version=\"1.0\"?>",
    "<!DOCTYPE target SYSTEM \"gdb-target.dtd\">",
    "<target version=\"1.0\">",
    "<architecture>i386:x86-64</architecture>",
    "<feature name=\"org.gnu.gdb.i386.core\">",
    "<reg name=\"rax\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"rbx\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"rcx\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"rdx\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"rsi\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"rdi\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"rbp\" bitsize=\"64\" type=\"data_ptr\"/>",
    "<reg name=\"rsp\" bitsize=\"64\" type=\"data_ptr\"/>",
    "<reg name=\"r8\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"r9\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"r10\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"r11\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"r12\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"r13\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"r14\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"r15\" bitsize=\"64\" type=\"int64\"/>",
    "<reg name=\"rip\" bitsize=\"64\" type=\"code_ptr\"/>",
    "<reg name=\"eflags\" bitsize=\"32\" type=\"int32\"/>",
    "<reg name=\"cs\" bitsize=\"32\" type=\"int32\"/>",
    "<reg name=\"ss\" bitsize=\"32\" type=\"int32\"/>",
    "<reg name=\"ds\" bitsize=\"32\" type=\"int32\"/>",
    "<reg name=\"es\" bitsize=\"32\" type=\"int32\"/>",
    "<reg name=\"fs\" bitsize=\"32\" type=\"int32\"/>",
    "<reg name=\"gs\" bitsize=\"32\" type=\"int32\"/>",
    "</feature></target>",
);

pub struct GdbStub {
    inbuf: Vec<u8>,
    /// Bytes to transmit back to the debugger.
    pub out: VecDeque<u8>,
    /// Whether the target should be executing. The driver runs the machine only
    /// while this is set, and clears it (via `report_stop`) on any stop.
    pub running: bool,
    /// After `QStartNoAckMode`, packets are not `+`/`-` acknowledged.
    no_ack: bool,
}

impl Default for GdbStub {
    fn default() -> Self {
        Self::new()
    }
}

impl GdbStub {
    pub fn new() -> Self {
        GdbStub { inbuf: Vec::new(), out: VecDeque::new(), running: false, no_ack: false }
    }

    /// Feed bytes received from the debugger; dispatch every complete packet.
    pub fn on_input(&mut self, m: &mut Machine, bytes: &[u8]) {
        self.inbuf.extend_from_slice(bytes);
        self.drain_packets(m);
    }

    /// Report that the machine has stopped (breakpoint, step, fault, or an
    /// interrupt), sending the debugger a stop reply and halting execution.
    pub fn report_stop(&mut self, signal: u8) {
        self.running = false;
        let mut s = String::new();
        push_hex_byte(&mut s, signal);
        self.send(&alloc::format!("T{s}thread:01;"));
    }

    fn drain_packets(&mut self, m: &mut Machine) {
        loop {
            // Skip acks and handle a raw Ctrl-C (interrupt request).
            while let Some(&b) = self.inbuf.first() {
                match b {
                    b'+' | b'-' => {
                        self.inbuf.remove(0);
                    }
                    0x03 => {
                        self.inbuf.remove(0);
                        self.running = false;
                        self.report_stop(2); // SIGINT
                    }
                    b'$' => break,
                    _ => {
                        self.inbuf.remove(0); // stray byte
                    }
                }
            }
            // Need "$<body>#XX".
            let Some(start) = self.inbuf.iter().position(|&b| b == b'$') else { return };
            let Some(hash) = self.inbuf[start..].iter().position(|&b| b == b'#') else { return };
            let hash = start + hash;
            if self.inbuf.len() < hash + 3 {
                return; // checksum digits not here yet
            }
            let body: Vec<u8> = self.inbuf[start + 1..hash].to_vec();
            // (Checksum bytes are inbuf[hash+1..hash+3]; we trust the transport.)
            self.inbuf.drain(0..hash + 3);
            if !self.no_ack {
                self.out.push_back(b'+');
            }
            self.dispatch(m, &body);
        }
    }

    fn dispatch(&mut self, m: &mut Machine, body: &[u8]) {
        let s = body;
        match s.first().copied() {
            Some(b'?') => self.send("T05thread:01;"),
            Some(b'g') => self.read_regs(m),
            Some(b'G') => self.write_regs(m, &s[1..]),
            Some(b'p') => self.read_one_reg(m, &s[1..]),
            Some(b'P') => self.write_one_reg(m, &s[1..]),
            Some(b'm') => self.read_mem(m, &s[1..]),
            Some(b'M') => self.write_mem(m, &s[1..]),
            Some(b'Z') => self.set_break(m, &s[1..], true),
            Some(b'z') => self.set_break(m, &s[1..], false),
            Some(b'c') => {
                // Resume; step off any breakpoint we are parked on.
                m.bp_skip_once = true;
                self.running = true;
            }
            Some(b's') => {
                // Single step: execute exactly one instruction, then report.
                m.bp_skip_once = true;
                let _ = m.run(1);
                self.report_stop(5);
            }
            Some(b'H') => self.send("OK"),      // set thread (ignored, one thread)
            Some(b'D') => {
                self.running = true; // detach: let it run free
                self.send("OK");
            }
            Some(b'k') => self.send("OK"),      // kill (ignored)
            Some(b'v') => self.handle_v(m, s),
            Some(b'q') | Some(b'Q') => self.handle_query(s),
            _ => self.send(""), // unsupported -> empty
        }
    }

    fn handle_v(&mut self, m: &mut Machine, s: &[u8]) {
        if s.starts_with(b"vCont?") {
            self.send("vCont;c;s;C;S");
        } else if s.starts_with(b"vCont;s") {
            m.bp_skip_once = true;
            let _ = m.run(1);
            self.report_stop(5);
        } else if s.starts_with(b"vCont;c") || s.starts_with(b"vCont;C") {
            m.bp_skip_once = true;
            self.running = true;
        } else {
            self.send("");
        }
    }

    fn handle_query(&mut self, s: &[u8]) {
        if s.starts_with(b"qSupported") {
            self.send("PacketSize=4000;QStartNoAckMode+;qXfer:features:read+");
        } else if s.starts_with(b"QStartNoAckMode") {
            self.no_ack = true;
            self.send("OK");
        } else if s.starts_with(b"qXfer:features:read:target.xml:") {
            self.serve_xfer(&s[b"qXfer:features:read:target.xml:".len()..]);
        } else if s.starts_with(b"qAttached") {
            self.send("1");
        } else if s.starts_with(b"qC") {
            self.send("QC01");
        } else if s.starts_with(b"qfThreadInfo") {
            self.send("m01");
        } else if s.starts_with(b"qsThreadInfo") {
            self.send("l");
        } else if s.starts_with(b"qHostInfo") {
            self.send("triple:7838365f36342d2d2d;ptrsize:8;endian:little;");
        } else if s.starts_with(b"qProcessInfo") {
            self.send("pid:1;ptrsize:8;endian:little;");
        } else {
            self.send(""); // unsupported query
        }
    }

    /// Serve a slice of `target.xml` for an "off,len" qXfer request.
    fn serve_xfer(&mut self, args: &[u8]) {
        let a = core::str::from_utf8(args).unwrap_or("");
        let mut it = a.split(',');
        let off = it.next().and_then(|x| usize::from_str_radix(x, 16).ok()).unwrap_or(0);
        let len = it.next().and_then(|x| usize::from_str_radix(x, 16).ok()).unwrap_or(0);
        let xml = TARGET_XML.as_bytes();
        if off >= xml.len() {
            self.send("l");
            return;
        }
        let end = (off + len).min(xml.len());
        let mut resp = String::new();
        resp.push(if end == xml.len() { 'l' } else { 'm' });
        resp.push_str(core::str::from_utf8(&xml[off..end]).unwrap_or(""));
        self.send(&resp);
    }

    fn read_regs(&mut self, m: &Machine) {
        let mut s = String::new();
        for &gp in &GDB_GP {
            push_u64_le(&mut s, m.cpu.regs[gp]);
        }
        push_u64_le(&mut s, m.cpu.rip);
        push_u32_le(&mut s, m.cpu.rflags as u32);
        // Segment selectors: plausible values by privilege level.
        let (cs, ss) = if m.cpu.cpl == 0 { (0x10u32, 0x18u32) } else { (0x33, 0x2b) };
        for seg in [cs, ss, 0, 0, 0, 0] {
            push_u32_le(&mut s, seg);
        }
        self.send(&s);
    }

    fn write_regs(&mut self, m: &mut Machine, hex: &[u8]) {
        let bytes = decode_hex(hex);
        if bytes.len() < 16 * 8 + 8 + 4 {
            self.send("E01");
            return;
        }
        for (i, &gp) in GDB_GP.iter().enumerate() {
            m.cpu.regs[gp] = le_u64(&bytes[i * 8..]);
        }
        m.cpu.rip = le_u64(&bytes[16 * 8..]);
        m.cpu.rflags = le_u32(&bytes[16 * 8 + 8..]) as u64;
        self.send("OK");
    }

    /// `p N` — read register number N (GDB numbering: 0..15 GP, 16 rip, 17
    /// eflags, 18..23 segs).
    fn read_one_reg(&mut self, m: &Machine, hex: &[u8]) {
        let Some(n) = parse_hex(hex) else {
            self.send("E01");
            return;
        };
        let mut s = String::new();
        match n {
            0..=15 => push_u64_le(&mut s, m.cpu.regs[GDB_GP[n as usize]]),
            16 => push_u64_le(&mut s, m.cpu.rip),
            17 => push_u32_le(&mut s, m.cpu.rflags as u32),
            18..=23 => push_u32_le(&mut s, 0),
            _ => {
                self.send("E01");
                return;
            }
        }
        self.send(&s);
    }

    /// `P N=VALUE`.
    fn write_one_reg(&mut self, m: &mut Machine, arg: &[u8]) {
        let a = core::str::from_utf8(arg).unwrap_or("");
        let Some((ns, vs)) = a.split_once('=') else {
            self.send("E01");
            return;
        };
        let Ok(n) = u64::from_str_radix(ns, 16) else {
            self.send("E01");
            return;
        };
        let bytes = decode_hex(vs.as_bytes());
        match n {
            0..=15 if bytes.len() >= 8 => m.cpu.regs[GDB_GP[n as usize]] = le_u64(&bytes),
            16 if bytes.len() >= 8 => m.cpu.rip = le_u64(&bytes),
            17 if bytes.len() >= 4 => m.cpu.rflags = le_u32(&bytes) as u64,
            _ => {}
        }
        self.send("OK");
    }

    /// `m ADDR,LEN` — read guest memory, translating each byte's page.
    fn read_mem(&mut self, m: &Machine, arg: &[u8]) {
        let a = core::str::from_utf8(arg).unwrap_or("");
        let mut it = a.split(',');
        let addr = it.next().and_then(|x| u64::from_str_radix(x, 16).ok());
        let len = it.next().and_then(|x| usize::from_str_radix(x, 16).ok());
        let (Some(addr), Some(len)) = (addr, len) else {
            self.send("E01");
            return;
        };
        let len = len.min(2048);
        let mut s = String::new();
        for i in 0..len as u64 {
            match mmu::translate(&m.ram, &m.cpu.paging, addr + i, Access::Read, false) {
                Ok(pa) => push_hex_byte(&mut s, m.ram.get(pa as usize).copied().unwrap_or(0)),
                Err(_) => {
                    if i == 0 {
                        self.send("E14");
                        return;
                    }
                    break; // partial read up to the unmapped page
                }
            }
        }
        self.send(&s);
    }

    /// `M ADDR,LEN:HEX` — write guest memory.
    fn write_mem(&mut self, m: &mut Machine, arg: &[u8]) {
        let a = core::str::from_utf8(arg).unwrap_or("");
        let Some((head, data)) = a.split_once(':') else {
            self.send("E01");
            return;
        };
        let mut it = head.split(',');
        let addr = it.next().and_then(|x| u64::from_str_radix(x, 16).ok());
        let (Some(addr), Some(_len)) = (addr, it.next()) else {
            self.send("E01");
            return;
        };
        let bytes = decode_hex(data.as_bytes());
        for (i, &b) in bytes.iter().enumerate() {
            let va = addr + i as u64;
            let pa = mmu::translate(&m.ram, &m.cpu.paging, va, Access::Write, false)
                .or_else(|_| mmu::translate(&m.ram, &m.cpu.paging, va, Access::Read, false));
            match pa {
                Ok(pa) => {
                    if let Some(slot) = m.ram.get_mut(pa as usize) {
                        *slot = b;
                    }
                }
                Err(_) => {
                    self.send("E14");
                    return;
                }
            }
        }
        self.send("OK");
    }

    /// `Z0,ADDR,KIND` / `z0,...` — software (and hardware, treated the same)
    /// breakpoints. Watchpoints (kinds 2..4) are not supported (empty reply).
    fn set_break(&mut self, m: &mut Machine, arg: &[u8], add: bool) {
        let a = core::str::from_utf8(arg).unwrap_or("");
        let ty = a.as_bytes().first().copied();
        if !matches!(ty, Some(b'0') | Some(b'1')) {
            self.send(""); // only exec breakpoints
            return;
        }
        // Format is "<type>,<addr>,<kind>".
        let addr = a.split(',').nth(1).and_then(|x| u64::from_str_radix(x, 16).ok());
        let Some(addr) = addr else {
            self.send("E01");
            return;
        };
        if add {
            if !m.breakpoints.contains(&addr) {
                m.breakpoints.push(addr);
            }
        } else {
            m.breakpoints.retain(|&b| b != addr);
        }
        self.send("OK");
    }

    /// Frame `body` as `$body#XX` and queue it for transmission.
    fn send(&mut self, body: &str) {
        self.out.push_back(b'$');
        let mut sum: u8 = 0;
        for &b in body.as_bytes() {
            self.out.push_back(b);
            sum = sum.wrapping_add(b);
        }
        self.out.push_back(b'#');
        let mut ck = String::new();
        push_hex_byte(&mut ck, sum);
        for b in ck.bytes() {
            self.out.push_back(b);
        }
    }
}

// --- hex helpers ----------------------------------------------------------

fn push_hex_byte(s: &mut String, b: u8) {
    const H: &[u8; 16] = b"0123456789abcdef";
    s.push(H[(b >> 4) as usize] as char);
    s.push(H[(b & 0xf) as usize] as char);
}
fn push_u32_le(s: &mut String, v: u32) {
    for b in v.to_le_bytes() {
        push_hex_byte(s, b);
    }
}
fn push_u64_le(s: &mut String, v: u64) {
    for b in v.to_le_bytes() {
        push_hex_byte(s, b);
    }
}
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
/// Decode an ASCII-hex byte string (stops at the last complete byte pair).
fn decode_hex(h: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(h.len() / 2);
    let mut i = 0;
    while i + 1 < h.len() {
        if let (Some(hi), Some(lo)) = (hex_val(h[i]), hex_val(h[i + 1])) {
            out.push((hi << 4) | lo);
        } else {
            break;
        }
        i += 2;
    }
    out
}
/// Parse a hex integer (for register numbers).
fn parse_hex(h: &[u8]) -> Option<u64> {
    u64::from_str_radix(core::str::from_utf8(h).ok()?, 16).ok()
}
fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}
fn le_u32(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[..4]);
    u32::from_le_bytes(a)
}
