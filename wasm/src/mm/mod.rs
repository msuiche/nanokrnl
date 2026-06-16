//! `mm` — memory management for the WASM build. Phase 1 provides the pool API
//! over a static arena (see [`pool`]); page tables / `mm::virt` have no WASM
//! analogue (no MMU) and are out of scope until a later phase.
pub mod pool;
