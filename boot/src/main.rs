//! Host-side boot image builder and QEMU runner — the "winload.exe" stand-in.
//!
//! On real Windows the boot chain is:
//!
//! ```text
//!   UEFI firmware -> bootmgfw.efi -> winload.efi -> ntoskrnl.exe
//! ```
//!
//! Here the `bootloader` crate plays the role of bootmgr+winload: it sets up
//! long mode, builds the initial identity/higher-half page tables, gathers the
//! physical memory map, and jumps to the kernel entry point with a
//! `BootInfo` structure (the moral equivalent of NT's `LOADER_PARAMETER_BLOCK`).
//!
//! Usage:
//! ```text
//!   boot <path-to-kernel-elf> [--run] [--uefi]
//! ```
//! Without `--run` it only produces `target/disk-bios.img` /
//! `target/disk-uefi.img`; with `--run` it launches `qemu-system-x86_64`
//! with the serial port wired to stdio (our KdPrint goes to COM1).

use std::{env, path::PathBuf, process::Command};

fn main() {
    let mut args = env::args().skip(1);
    let kernel = PathBuf::from(
        args.next()
            .expect("usage: boot <kernel-elf> [--run] [--uefi]"),
    );
    let rest: Vec<String> = args.collect();
    let run = rest.iter().any(|a| a == "--run");
    let uefi = rest.iter().any(|a| a == "--uefi");

    let out_dir = kernel
        .parent()
        .expect("kernel path has no parent directory")
        .to_path_buf();

    let bios_path = out_dir.join("disk-bios.img");
    let uefi_path = out_dir.join("disk-uefi.img");

    // Build both flavors; they are cheap and it lets the user pick at run time.
    bootloader::BiosBoot::new(&kernel)
        .create_disk_image(&bios_path)
        .expect("failed to create BIOS disk image");
    bootloader::UefiBoot::new(&kernel)
        .create_disk_image(&uefi_path)
        .expect("failed to create UEFI disk image");

    println!("created {}", bios_path.display());
    println!("created {}", uefi_path.display());

    if run {
        let mut qemu = Command::new("qemu-system-x86_64");
        if uefi {
            // OVMF firmware path is distro-specific; allow override via env.
            let ovmf = env::var("OVMF_PATH")
                .unwrap_or_else(|_| "/opt/homebrew/share/qemu/edk2-x86_64-code.fd".into());
            qemu.arg("-bios").arg(ovmf);
            qemu.arg("-drive")
                .arg(format!("format=raw,file={}", uefi_path.display()));
        } else {
            qemu.arg("-drive")
                .arg(format!("format=raw,file={}", bios_path.display()));
        }
        // COM1 -> stdio so KdPrint output lands in the terminal; the ISA debug
        // exit device gives the kernel a way to report pass/fail exit codes
        // from its phase-1 self tests (exit code = (value << 1) | 1).
        // Expose SMEP + SMAP so the kernel can enable Supervisor Mode
        // Execution/Access Prevention. The kernel brackets its few user-buffer
        // accesses with stac/clac.
        qemu.arg("-cpu").arg("qemu64,+smep,+smap");
        qemu.arg("-serial").arg("stdio");
        qemu.arg("-device")
            .arg("isa-debug-exit,iobase=0xf4,iosize=0x04");
        qemu.arg("-display").arg("none");
        qemu.arg("-no-reboot");
        let status = qemu.status().expect("failed to launch qemu-system-x86_64");
        // 0x10 is the kernel's "all self tests passed" exit value: (0x10<<1)|1 = 33.
        std::process::exit(status.code().unwrap_or(-1));
    }
}
