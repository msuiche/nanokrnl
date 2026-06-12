//! Kernel binary entry point.
//!
//! This file is deliberately thin: all kernel code lives in the `kernel`
//! library crate (see `lib.rs`) so the architecture-independent subsystems
//! can be unit-tested on the host. The binary's only job is to register the
//! bootloader entry point and forward to `kernel::init::ki_system_startup`.

#![cfg_attr(target_arch = "x86_64", no_std)]
#![cfg_attr(target_arch = "x86_64", no_main)]

#[cfg(target_arch = "x86_64")]
mod entry {
    use bootloader_api::{config::Mapping, entry_point, BootInfo, BootloaderConfig};

    /// Boot configuration consumed by the bootloader at image-build time.
    ///
    /// * `physical_memory: Dynamic` — ask winload to map *all* physical
    ///   memory at a kernel-space virtual offset. NT maintains the same
    ///   kind of window for the memory manager; `mm` depends on it to reach
    ///   page tables and the PFN database without temporary mappings.
    /// * 256 KiB boot stack: phase-0/1 init runs deep call chains (pool +
    ///   object manager + self tests) before the first real thread exists.
    pub static BOOTLOADER_CONFIG: BootloaderConfig = {
        let mut config = BootloaderConfig::new_default();
        // Place all kernel mappings in the HIGH canonical half (like NT):
        // the kernel image, stack, and boot info go in the dynamic range
        // 0xFFFF_8000_.. , and the all-physical-memory window sits just above
        // it. This frees the entire LOW half for per-process user mappings —
        // the prerequisite for real per-process address spaces (and SMAP).
        // The kernel reads `physical_memory_offset` from BootInfo at runtime,
        // so moving the window is transparent to mm.
        config.mappings.dynamic_range_start = Some(0xFFFF_8000_0000_0000);
        config.mappings.dynamic_range_end = Some(0xFFFF_FEFF_FFFF_FFFF);
        config.mappings.physical_memory = Some(Mapping::FixedAddress(0xFFFF_FF00_0000_0000));
        config.kernel_stack_size = 256 * 1024;
        config
    };

    entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

    /// First Rust code to run after the bootloader — NT's `KiSystemStartup`.
    fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
        kernel::init::ki_system_startup(boot_info)
    }

    /// The library only provides the panic handler for its own freestanding
    /// build; re-exporting nothing here is intentional — the handler in
    /// `kernel` (lib.rs) covers this binary too since they link as one image.
    const _: () = ();
}

/// Building the binary for a non-kernel target (e.g. as part of `cargo test`
/// on the host) produces a stub that just explains itself.
#[cfg(not(target_arch = "x86_64"))]
fn main() {
    eprintln!("ntoskrnl-rs only runs on x86_64-unknown-none; use `cargo xbuild`/boot crate");
}
