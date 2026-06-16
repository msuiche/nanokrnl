// Headless host for the WASM kernel — the same contract as index.html, but in
// Node so the boot can be smoke-tested without a browser.
//   node wasm/web/run-node.mjs
import { readFileSync } from "node:fs";

const bytes = readFileSync(new URL("./ntoskrnl_wasm.wasm", import.meta.url));
let memory;
const imports = {
  env: {
    host_write(ptr, len) {
      const b = new Uint8Array(memory.buffer, ptr, len);
      process.stdout.write(Buffer.from(b).toString("utf8"));
    },
  },
};
const { instance } = await WebAssembly.instantiate(bytes, imports);
memory = instance.exports.memory;
const code = instance.exports.kernel_main();
process.stdout.write(`\n[host] kernel_main() returned ${code}\n`);
process.exit(code);
