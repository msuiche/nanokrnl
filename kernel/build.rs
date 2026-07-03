//! Build script: locate the embedded test-driver PE.
//!
//! The driver-load self test embeds a real `.sys` via `include_bytes!`. That
//! file is produced by `scripts/driver-test.sh` (a separate toolchain/target
//! build), so it may or may not exist when the kernel is compiled. To keep
//! the kernel buildable either way, we resolve the path here:
//!
//! * if `driver/testdriver.sys` exists, embed it;
//! * otherwise emit an empty placeholder and embed that.
//!
//! The self test treats an empty image as "no driver embedded" and reports
//! the load test as skipped rather than failing the build.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest.parent().unwrap();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // Resolve an embeddable image, or an empty placeholder if it hasn't been
    // built yet, so the kernel always compiles.
    let resolve = |built: PathBuf, placeholder: &str, env: &str| {
        // Always watch the expected path — even before it exists — so that
        // building the image later re-triggers this script and re-embeds it.
        println!("cargo:rerun-if-changed={}", built.display());
        let path = if built.exists() {
            built
        } else {
            let p = out_dir.join(placeholder);
            std::fs::write(&p, []).unwrap();
            p
        };
        println!("cargo:rustc-env={}={}", env, path.display());
    };

    resolve(
        workspace.join("driver").join("testdriver.sys"),
        "no_driver.bin",
        "NTOS_DRIVER_IMAGE",
    );
    // A real Microsoft kernel driver (the NULL device driver), dropped into
    // drivers/ by hand — exercises loading an unmodified .sys against our
    // ntoskrnl export table. Empty (skipped) when not present.
    resolve(
        workspace.join("drivers").join("null.sys"),
        "no_null_sys.bin",
        "NTOS_NULL_SYS_IMAGE",
    );
    resolve(
        workspace.join("userapp").join("userapp.exe"),
        "no_userapp.bin",
        "NTOS_USERAPP_IMAGE",
    );
    resolve(
        workspace.join("kernel32").join("kernel32.dll"),
        "no_kernel32.bin",
        "NTOS_KERNEL32_IMAGE",
    );
    resolve(
        workspace.join("userapp2").join("userapp2.exe"),
        "no_userapp2.bin",
        "NTOS_USERAPP2_IMAGE",
    );
    resolve(
        workspace.join("worker").join("worker.exe"),
        "no_worker.bin",
        "NTOS_WORKER_IMAGE",
    );
    resolve(
        workspace.join("crash").join("crash.exe"),
        "no_crash.bin",
        "NTOS_CRASH_IMAGE",
    );
    resolve(
        workspace.join("msvcrt").join("msvcrt.dll"),
        "no_msvcrt.bin",
        "NTOS_MSVCRT_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("sort.exe"),
        "no_sort.bin",
        "NTOS_SORT_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("choice.exe"),
        "no_choice.bin",
        "NTOS_CHOICE_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("choice.exe.mui"),
        "no_choice_mui.bin",
        "NTOS_CHOICE_MUI_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("where.exe"),
        "no_where.bin",
        "NTOS_WHERE_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("where.exe.mui"),
        "no_where_mui.bin",
        "NTOS_WHERE_MUI_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("cmd.exe"),
        "no_cmd.bin",
        "NTOS_CMD_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("more.com"),
        "no_more.bin",
        "NTOS_MORE_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("whoami.exe"),
        "no_whoami.bin",
        "NTOS_WHOAMI_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("whoami.exe.mui"),
        "no_whoami_mui.bin",
        "NTOS_WHOAMI_MUI_IMAGE",
    );
    // ulib.dll — the utility library more.com (and format/chkdsk/…) depend on.
    // A real DLL with its own imports; the dependent-DLL loader binds those
    // against the shims, then a console tool's ulib imports bind to it.
    resolve(
        workspace.join("winbin").join("ulib.dll"),
        "no_ulib.bin",
        "NTOS_ULIB_IMAGE",
    );
    resolve(
        workspace.join("winbin").join("cmd.exe.mui"),
        "no_cmd_mui.bin",
        "NTOS_CMD_MUI_IMAGE",
    );

    println!("cargo:rerun-if-changed=build.rs");
}
