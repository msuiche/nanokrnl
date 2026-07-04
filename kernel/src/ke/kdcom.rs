//! KDCOM packet layer - the wire framing of the Windows kernel-debugger (KD)
//! protocol, the transport a live WinDbg session speaks to the target.
//!
//! This module is the framing foundation only: encode/decode of `KD_PACKET`
//! (leader, type, byte-count, id, checksum, trailer) plus the control packets
//! (ACK/RESEND/RESET) and the break-in byte. The state machine (wait-state-change
//! on break-in, then `KD_STATE_MANIPULATE` for read/write memory, get/set context,
//! breakpoints) and the byte transport (over the UART, bridged to WinDbg's
//! `com:pipe`) build on top. Kept dependency-free so it unit-tests on the host.
//!
//! Layout of `KD_PACKET` (16-byte header, little-endian), then `byte_count`
//! payload bytes, then a single [`PACKET_TRAILER`] byte:
//! ```text
//!   +0x00 u32 PacketLeader   (data: 0x30303030, control: 0x69696969)
//!   +0x04 u16 PacketType
//!   +0x06 u16 ByteCount      (payload length)
//!   +0x08 u32 PacketId
//!   +0x0c u32 Checksum       (sum of payload bytes)
//! ```

#![allow(dead_code)]

use alloc::vec::Vec;

/// Leader of a normal (data-bearing) packet: four `'0'` bytes.
pub const PACKET_LEADER: u32 = 0x3030_3030;
/// Leader of a control packet (ACK/RESEND/RESET): four `'i'` bytes.
pub const CONTROL_PACKET_LEADER: u32 = 0x6969_6969;
/// The single byte that terminates a packet on the wire.
pub const PACKET_TRAILER: u8 = 0xAA;
/// The byte WinDbg sends to force a break-in.
pub const BREAKIN_BYTE: u8 = 0x62; // 'b'

/// 16-byte `KD_PACKET` header size.
pub const HEADER_LEN: usize = 16;

/// KD packet types (`PACKET_TYPE_KD_*`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum PacketType {
    Unused = 0,
    StateChange32 = 1,
    StateManipulate = 2,
    DebugIo = 3,
    Acknowledge = 4,
    Resend = 5,
    Reset = 6,
    StateChange64 = 7,
}
impl PacketType {
    fn from_u16(v: u16) -> Option<PacketType> {
        Some(match v {
            0 => PacketType::Unused,
            1 => PacketType::StateChange32,
            2 => PacketType::StateManipulate,
            3 => PacketType::DebugIo,
            4 => PacketType::Acknowledge,
            5 => PacketType::Resend,
            6 => PacketType::Reset,
            7 => PacketType::StateChange64,
            _ => return None,
        })
    }
}

/// Initial data-packet id; the two endpoints toggle the low bit each packet.
pub const INITIAL_PACKET_ID: u32 = 0x8080_0000;
/// The id used for a sync/reset exchange.
pub const SYNC_PACKET_ID: u32 = 0x0000_0800;

/// Sum-of-bytes checksum over a payload (`KdCalculateChecksum`).
pub fn checksum(data: &[u8]) -> u32 {
    let mut sum: u32 = 0;
    for &b in data {
        sum = sum.wrapping_add(b as u32);
    }
    sum
}

/// Encode a full data packet: header + payload + trailer.
pub fn encode(kind: PacketType, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(HEADER_LEN + payload.len() + 1);
    p.extend_from_slice(&PACKET_LEADER.to_le_bytes());
    p.extend_from_slice(&(kind as u16).to_le_bytes());
    p.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    p.extend_from_slice(&id.to_le_bytes());
    p.extend_from_slice(&checksum(payload).to_le_bytes());
    p.extend_from_slice(payload);
    p.push(PACKET_TRAILER);
    p
}

/// Encode a control packet (ACK/RESEND/RESET): a header only, control leader,
/// no payload and no trailer, checksum 0.
pub fn encode_control(kind: PacketType, id: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(HEADER_LEN);
    p.extend_from_slice(&CONTROL_PACKET_LEADER.to_le_bytes());
    p.extend_from_slice(&(kind as u16).to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes()); // ByteCount
    p.extend_from_slice(&id.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes()); // Checksum
    p
}

/// A decoded packet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Packet {
    pub kind: PacketType,
    pub id: u32,
    pub control: bool,
    pub payload: Vec<u8>,
}

/// Incremental packet decoder: feed received bytes, pull complete packets. A
/// real transport also watches for a lone [`BREAKIN_BYTE`] outside a packet.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Decoder { buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Try to pull one complete, checksum-valid packet from the front of the
    /// buffer. Returns `None` if more bytes are needed. On a bad leader it
    /// resynchronizes by dropping one byte at a time.
    pub fn next_packet(&mut self) -> Option<Packet> {
        loop {
            if self.buf.len() < HEADER_LEN {
                return None;
            }
            let leader = u32::from_le_bytes(self.buf[0..4].try_into().unwrap());
            let control = leader == CONTROL_PACKET_LEADER;
            if leader != PACKET_LEADER && !control {
                self.buf.remove(0); // resync
                continue;
            }
            let kind_raw = u16::from_le_bytes(self.buf[4..6].try_into().unwrap());
            let byte_count = u16::from_le_bytes(self.buf[6..8].try_into().unwrap()) as usize;
            let id = u32::from_le_bytes(self.buf[8..12].try_into().unwrap());
            let sum = u32::from_le_bytes(self.buf[12..16].try_into().unwrap());

            if control {
                // Control packets carry no payload/trailer.
                let kind = PacketType::from_u16(kind_raw)?;
                let pkt = Packet { kind, id, control: true, payload: Vec::new() };
                self.buf.drain(0..HEADER_LEN);
                return Some(pkt);
            }

            let total = HEADER_LEN + byte_count + 1; // + trailer
            if self.buf.len() < total {
                return None;
            }
            let payload = self.buf[HEADER_LEN..HEADER_LEN + byte_count].to_vec();
            let trailer = self.buf[HEADER_LEN + byte_count];
            let ok = trailer == PACKET_TRAILER && checksum(&payload) == sum;
            let kind = PacketType::from_u16(kind_raw);
            self.buf.drain(0..total);
            match (ok, kind) {
                (true, Some(kind)) => {
                    return Some(Packet { kind, id, control: false, payload })
                }
                // Malformed/unknown: skip it and keep scanning.
                _ => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_byte_sum() {
        assert_eq!(checksum(&[]), 0);
        assert_eq!(checksum(&[1, 2, 3]), 6);
        assert_eq!(checksum(&[0xff, 0xff]), 0x1fe);
    }

    #[test]
    fn data_packet_round_trips() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x7F];
        let wire = encode(PacketType::StateManipulate, INITIAL_PACKET_ID, &payload);
        assert_eq!(u32::from_le_bytes(wire[0..4].try_into().unwrap()), PACKET_LEADER);
        assert_eq!(*wire.last().unwrap(), PACKET_TRAILER);

        let mut d = Decoder::new();
        d.push(&wire);
        let pkt = d.next_packet().expect("one packet");
        assert_eq!(pkt.kind, PacketType::StateManipulate);
        assert_eq!(pkt.id, INITIAL_PACKET_ID);
        assert!(!pkt.control);
        assert_eq!(pkt.payload, payload);
        assert!(d.next_packet().is_none());
    }

    #[test]
    fn control_packet_round_trips() {
        let wire = encode_control(PacketType::Acknowledge, INITIAL_PACKET_ID);
        assert_eq!(wire.len(), HEADER_LEN);
        let mut d = Decoder::new();
        d.push(&wire);
        let pkt = d.next_packet().expect("ack");
        assert_eq!(pkt.kind, PacketType::Acknowledge);
        assert!(pkt.control);
        assert!(pkt.payload.is_empty());
    }

    #[test]
    fn split_delivery_reassembles() {
        let payload = [1u8, 2, 3, 4, 5];
        let wire = encode(PacketType::StateChange64, 0x8080_0001, &payload);
        let mut d = Decoder::new();
        // Feed one byte at a time; only the final byte completes the packet.
        for (i, &b) in wire.iter().enumerate() {
            d.push(&[b]);
            let got = d.next_packet();
            if i + 1 < wire.len() {
                assert!(got.is_none(), "packet completed early at byte {i}");
            } else {
                assert_eq!(got.unwrap().payload, payload);
            }
        }
    }

    #[test]
    fn bad_checksum_is_dropped() {
        let mut wire = encode(PacketType::DebugIo, 1, &[9, 9, 9]);
        // Corrupt a payload byte so the checksum no longer matches.
        wire[HEADER_LEN] ^= 0xFF;
        let mut d = Decoder::new();
        d.push(&wire);
        assert!(d.next_packet().is_none(), "corrupt packet must not decode");
    }

    #[test]
    fn resyncs_after_leading_garbage() {
        let payload = [0xAB, 0xCD];
        let mut wire = alloc::vec![0x11u8, 0x22, 0x33]; // junk before the leader
        wire.extend_from_slice(&encode(PacketType::Reset, 7, &payload));
        let mut d = Decoder::new();
        d.push(&wire);
        let pkt = d.next_packet().expect("resynced packet");
        assert_eq!(pkt.payload, payload);
    }
}
