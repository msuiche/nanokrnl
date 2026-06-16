// Headless interactive host for the WASM kernel — same contract as index.html,
// in Node so the shell can be driven without a browser.
//   node wasm/web/run-node.mjs            (type commands; Ctrl-D to exit)
//   printf 'ver\nmkobj\nhandles\n' | node wasm/web/run-node.mjs   (scripted)
import { readFileSync } from "node:fs";
import { createInterface } from "node:readline";

const bytes = readFileSync(new URL("./ntoskrnl_wasm.wasm", import.meta.url));
let memory;
const imports = {
  env: {
    host_write(ptr, len) {
      process.stdout.write(Buffer.from(new Uint8Array(memory.buffer, ptr, len)));
    },
    host_clear() {
      process.stdout.write("\x1b[2J\x1b[H"); // ANSI clear + home
    },
  },
};
const { instance } = await WebAssembly.instantiate(bytes, imports);
memory = instance.exports.memory;

// Feed one command line into the kernel's fixed input buffer, then run it.
const enc = new TextEncoder();
function send(line) {
  const buf = enc.encode(line);
  const ptr = instance.exports.kernel_input_ptr();
  new Uint8Array(memory.buffer, ptr, buf.length).set(buf);
  instance.exports.kernel_input(buf.length);
}

instance.exports.kernel_main(); // boot + first prompt

const rl = createInterface({ input: process.stdin, terminal: false });
rl.on("line", (line) => send(line));
rl.on("close", () => process.exit(0));
