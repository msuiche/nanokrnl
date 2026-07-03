// A tiny 9P2000.L server that runs in the browser and backs nanokrnl's "H:"
// drive. The kernel's 9P client (kernel/src/io/p9.rs) writes size-prefixed
// T-messages to a port-mapped transport; the emulator buffers them in
// `p9.tx`. Between CPU run-slices the page calls `pump()`, which drains those
// request bytes through `nanox_p9_read`, serves complete messages from an
// in-memory file tree, and pushes the R-message bytes back with
// `nanox_p9_write` into `p9.rx`, which the guest reads once STATUS goes ready.
//
// Only the handful of messages a file read needs are implemented (version,
// attach, walk, lopen, read, clunk); anything else gets an Rlerror. See
// docs/9p-over-nanox.md for the wire format and the transport design.

// 9P2000.L message types.
const T = {
  VERSION: 100, RVERSION: 101,
  ATTACH: 104, RATTACH: 105,
  WALK: 110, RWALK: 111,
  LOPEN: 12, RLOPEN: 13,
  READ: 116, RREAD: 117,
  CLUNK: 120, RCLUNK: 121,
  LERROR: 7,
};

export class P9Server {
  // `files` is a plain object mapping a forward-slash path ("readme.txt",
  // "docs/notes.txt") to a string or Uint8Array.
  constructor(files) {
    this.files = {};
    for (const [k, v] of Object.entries(files || {})) {
      this.files[k] = typeof v === "string" ? new TextEncoder().encode(v) : v;
    }
    this.inbuf = [];               // request bytes not yet framed into a message
    this.fids = new Map();         // fid -> Uint8Array (file) or null (directory)
    this.served = 0;
  }

  // Drain pending request bytes from the guest, serve any complete messages,
  // and push the replies back. Call once per run-slice.
  pump(e) {
    let b;
    while ((b = e.nanox_p9_read()) >= 0) this.inbuf.push(b);
    for (;;) {
      if (this.inbuf.length < 7) return;
      const size = this.inbuf[0] | (this.inbuf[1] << 8) | (this.inbuf[2] << 16) | (this.inbuf[3] << 24);
      if (this.inbuf.length < size) return;
      const msg = this.inbuf.splice(0, size);
      const reply = this.serve(msg);
      for (const byte of reply) e.nanox_p9_write(byte);
      this.served++;
    }
  }

  serve(msg) {
    const typ = msg[4];
    const tag = msg[5] | (msg[6] << 8);
    const body = msg.slice(7);
    const u32 = (o) => body[o] | (body[o + 1] << 8) | (body[o + 2] << 16) | (body[o + 3] << 24);
    switch (typ) {
      case T.VERSION: {                       // Tversion msize version
        const msize = u32(0);
        const r = new Reply(T.RVERSION, tag);
        r.u32(msize); r.str("9P2000.L");
        return r.done();
      }
      case T.ATTACH: {                        // Tattach fid afid uname aname n_uname
        this.fids.set(u32(0), null);          // root directory
        const r = new Reply(T.RATTACH, tag); r.qid(0); return r.done();
      }
      case T.WALK: {                          // Twalk fid newfid nwname name*
        const newfid = u32(4);
        const nw = body[8] | (body[9] << 8);
        let off = 10, parts = [];
        for (let i = 0; i < nw; i++) {
          const l = body[off] | (body[off + 1] << 8); off += 2;
          parts.push(new TextDecoder().decode(new Uint8Array(body.slice(off, off + l)))); off += l;
        }
        const name = parts.join("/");
        const data = this.files[name];
        if (data === undefined) return rlerror(tag, 2); // ENOENT
        this.fids.set(newfid, data);
        const r = new Reply(T.RWALK, tag);
        r.u16(nw);
        for (let i = 0; i < nw; i++) r.qid(0);
        return r.done();
      }
      case T.LOPEN: {                         // Tlopen fid flags
        const r = new Reply(T.RLOPEN, tag); r.qid(0); r.u32(0); return r.done();
      }
      case T.READ: {                          // Tread fid offset count
        const fid = u32(0);
        const offset = u32(4) + u32(8) * 0x100000000; // 64-bit LE offset
        const count = u32(12);
        const data = this.fids.get(fid);
        const r = new Reply(T.RREAD, tag);
        if (data && offset < data.length) {
          const end = Math.min(offset + count, data.length);
          const slice = data.subarray(offset, end);
          r.u32(slice.length); r.bytes(slice);
        } else {
          r.u32(0);
        }
        return r.done();
      }
      case T.CLUNK:
        return new Reply(T.RCLUNK, tag).done();
      default:
        return rlerror(tag, 22); // EINVAL
    }
  }
}

function rlerror(tag, ecode) {
  const r = new Reply(T.LERROR, tag); r.u32(ecode); return r.done();
}

// Little-endian 9P message builder. The 4-byte size prefix is backpatched by
// done().
class Reply {
  constructor(typ, tag) {
    this.b = [0, 0, 0, 0, typ, tag & 0xff, (tag >> 8) & 0xff];
  }
  u16(v) { this.b.push(v & 0xff, (v >> 8) & 0xff); }
  u32(v) { this.b.push(v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, (v >> 24) & 0xff); }
  bytes(a) { for (const x of a) this.b.push(x); }
  str(s) { const e = new TextEncoder().encode(s); this.u16(e.length); this.bytes(e); }
  qid(type) { this.b.push(type); this.u32(0); this.bytes([0, 0, 0, 0, 0, 0, 0, 0]); } // type, version, path
  done() {
    const n = this.b.length;
    this.b[0] = n & 0xff; this.b[1] = (n >> 8) & 0xff;
    this.b[2] = (n >> 16) & 0xff; this.b[3] = (n >> 24) & 0xff;
    return this.b;
  }
}
