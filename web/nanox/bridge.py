#!/usr/bin/env python3
"""Bridge a kernel debugger to nanokrnl running in a browser tab, then launch it.

lldb/gdb speak the GDB protocol over TCP; a browser tab can only expose it over a
WebSocket. This relays bytes between the two and starts lldb for you:

    lldb  <--TCP 3333-->  gdb-bridge.py  <--WS 3334-->  browser tab (nanox.wasm)

Usage: python3 gdb-bridge.py         (open the page, click Debug, then this runs lldb)
       python3 gdb-bridge.py --bridge-only     (relay only; attach your own debugger)
Stdlib only. No pip, no Node.
"""
import socket, threading, subprocess, base64, hashlib, sys, shutil

TCP_PORT, WS_PORT = 3333, 3334
peer = {"tcp": None, "ws": None}      # lldb socket, page socket
page_ready = threading.Event()

def ws_handshake(c):
    data = b""
    while b"\r\n\r\n" not in data:
        data += c.recv(1024)
    key = next(l.split(b":", 1)[1].strip().decode()
               for l in data.split(b"\r\n") if l.lower().startswith(b"sec-websocket-key:"))
    accept = base64.b64encode(hashlib.sha1(
        (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
    c.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
              b"Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept.encode() + b"\r\n\r\n")

def ws_frames(buf):                    # parse complete client frames (masked)
    out = []
    while len(buf) >= 2:
        ln, p = buf[1] & 0x7f, 2
        if ln == 126: ln, p = int.from_bytes(buf[2:4], "big"), 4
        elif ln == 127: ln, p = int.from_bytes(buf[2:10], "big"), 10
        masked = buf[1] & 0x80
        if masked:
            if len(buf) < p + 4: break
            mask, p = buf[p:p + 4], p + 4
        if len(buf) < p + ln: break
        pl = bytearray(buf[p:p + ln])
        for i in range(ln if masked else 0): pl[i] ^= mask[i & 3]
        op, buf = buf[0] & 0x0f, buf[p + ln:]
        if op == 0x8: out.append(None); break     # close
        if op in (0, 1, 2): out.append(bytes(pl))
    return out, buf

def ws_send(c, data):                  # server->client binary frame (unmasked)
    n = len(data)
    hdr = bytes([0x82, n]) if n < 126 else \
          bytes([0x82, 126]) + n.to_bytes(2, "big") if n < 65536 else \
          bytes([0x82, 127]) + n.to_bytes(8, "big")
    c.sendall(hdr + data)

def serve(port, kind, on_data):
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port)); s.listen(1)
    while True:
        c, _ = s.accept(); peer[kind] = c
        if kind == "ws": ws_handshake(c); page_ready.set()
        print(f"[bridge] {'page' if kind=='ws' else 'debugger'} connected")
        buf = b""
        try:
            while (d := c.recv(4096)):
                buf = on_data(buf + d)
        except OSError: pass
        peer[kind] = None; print(f"[bridge] {kind} disconnected")

def on_ws(buf):
    frames, buf = ws_frames(buf)
    for f in frames:
        if peer["tcp"] and f: peer["tcp"].sendall(f)
    return buf

def on_tcp(buf):
    if peer["ws"]: ws_send(peer["ws"], buf)
    return b""

threading.Thread(target=serve, args=(WS_PORT, "ws", on_ws), daemon=True).start()
threading.Thread(target=serve, args=(TCP_PORT, "tcp", on_tcp), daemon=True).start()
print(f"[bridge] TCP :{TCP_PORT} (debugger)  WS :{WS_PORT} (page)")

if "--bridge-only" in sys.argv:
    threading.Event().wait()           # relay forever; you attach your own debugger
print("[bridge] open nanokrnl in the browser and click Debug ...")
page_ready.wait()
dbg = shutil.which("lldb") or shutil.which("gdb")
if not dbg:
    print("[bridge] no lldb/gdb found; leaving the relay running"); threading.Event().wait()
print(f"[bridge] launching {dbg}")
subprocess.run([dbg, "-o", f"gdb-remote {TCP_PORT}"] + sys.argv[1:])
