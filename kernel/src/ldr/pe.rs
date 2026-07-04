//! PE/COFF image loader — the heart of driver loading.
//!
//! Takes the raw bytes of a PE32+ image (a `.sys` is just a PE with subsystem
//! `native`) and turns them into a runnable in-memory image, performing the
//! three jobs a loader must do:
//!
//! 1. **Map sections by RVA.** Allocate `SizeOfImage` bytes, zero it (so
//!    uninitialized `.bss`/`.data` is zero, as the PE format promises), and
//!    copy each section's raw bytes to `base + VirtualAddress`.
//! 2. **Apply base relocations.** The image was linked for a preferred
//!    `ImageBase`; we loaded it elsewhere, so every absolute address baked
//!    into the code must be fixed by `delta = actual_base - preferred_base`.
//!    We handle `IMAGE_REL_BASED_DIR64` (the only kind 64-bit code emits)
//!    and `ABSOLUTE` padding.
//! 3. **Resolve imports.** Walk the import directory; for each name a driver
//!    imports from `ntoskrnl.exe`, look it up in the kernel export table
//!    ([`super::exports`]) and write the resolved address into the IAT.
//!
//! The result is an entry-point function pointer with the Microsoft x64 ABI.
//!
//! Parsing is offset-based against the documented PE layout (no `packed`
//! structs to mis-declare). The loader validates the load-bearing fields and
//! bails with an `NTSTATUS` on anything malformed; exhaustive hardening
//! against hostile images (overlapping sections, RVA overflow on every
//! field) is noted future work — this is a trusted-image loader today.

use crate::mm::pool::{pool_alloc_checked, pool_tag};
use crate::rtl::NtStatus;
use ntabi::DriverInitialize;

const TAG_IMAGE: u32 = pool_tag(b"MmLd");

// --- Little-endian field readers over the raw image ----------------------
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn u64le(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|s| {
        u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    })
}

// --- PE constants --------------------------------------------------------
const IMAGE_DOS_SIGNATURE: u16 = 0x5A4D; // "MZ"
const IMAGE_NT_SIGNATURE: u32 = 0x0000_4550; // "PE\0\0"
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_NT_OPTIONAL_HDR64_MAGIC: u16 = 0x20B;
const DIR_IMPORT: usize = 1;
const DIR_BASERELOC: usize = 5;
const DIR_TLS: usize = 9;
const DIR_LOAD_CONFIG: usize = 10;
const IMAGE_REL_BASED_ABSOLUTE: u16 = 0;
const IMAGE_REL_BASED_DIR64: u16 = 10;
const IMAGE_ORDINAL_FLAG64: u64 = 1 << 63;

/// A loaded, ready-to-run driver image.
pub struct LoadedImage {
    /// Base address of the mapped image in pool.
    pub base: *mut u8,
    /// Total mapped size (`SizeOfImage`).
    pub size: usize,
    /// Entry point as a kernel-ABI driver initializer (valid for kernel
    /// images loaded via [`load`]).
    pub entry: DriverInitialize,
    /// Entry point as a raw virtual address (used for user images, whose
    /// entry runs in ring 3 rather than as a kernel callback).
    pub entry_va: u64,
}

/// A freshly mapped + relocated image, before subsystem-specific finishing
/// (import binding for kernel, page-protection choice for either).
struct Mapped {
    base: *mut u8,
    size: usize,
    entry_rva: usize,
    import_rva: usize,
}

/// Load a PE32+ **kernel** driver: map, relocate, bind imports to the kernel
/// export table, mark the image kernel-executable, and return a
/// kernel-ABI entry pointer.
pub fn load(data: &[u8]) -> Result<LoadedImage, NtStatus> {
    let m = map_and_relocate(data)?;
    let img = unsafe { core::slice::from_raw_parts_mut(m.base, m.size) };
    if m.import_rva != 0 {
        resolve_imports(img, m.import_rva, super::exports::resolve)?;
    }
    // Pool is mapped NX by the bootloader; make the code executable.
    unsafe { crate::mm::virt::mm_set_executable(m.base as u64, m.size) };
    // Seed the /GS security cookie, as the Windows loader does before calling
    // DriverEntry (a real driver's GsDriverEntry __fastfail()s if the cookie is
    // still the on-disk default).
    unsafe { seed_security_cookie(m.base, m.size) };
    let entry_va = m.base as u64 + m.entry_rva as u64;
    Ok(LoadedImage {
        base: m.base,
        size: m.size,
        // SAFETY: entry is a DriverEntry with the Microsoft x64 ABI by contract.
        entry: unsafe { core::mem::transmute::<u64, DriverInitialize>(entry_va) },
        entry_va,
    })
}

/// Seed a mapped+relocated driver image's `/GS` security cookie, mirroring what
/// the Windows loader (`MmLoadSystemImage`) does before invoking DriverEntry.
/// The compiler's `GsDriverEntry` only *validates* `__security_cookie` (it
/// `__fastfail`s if the value is still 0 or the on-disk default); the OS is
/// responsible for replacing the default with a per-load value. We locate the
/// cookie through the Load Config data directory's `SecurityCookie` field
/// (offset 0x58 in `IMAGE_LOAD_CONFIG_DIRECTORY64`, already relocated to this
/// image's base) and write a value that is neither 0 nor the default.
///
/// # Safety
/// `base` must point at a fully mapped, relocated image of `size` bytes.
unsafe fn seed_security_cookie(base: *mut u8, size: usize) {
    /// The default `__security_cookie` MSVC bakes into x64 images.
    const DEFAULT_COOKIE: u64 = 0x0000_2B99_2DDF_A232;
    let hdr = unsafe { core::slice::from_raw_parts(base, size.min(0x1000)) };
    let Some(e_lfanew) = u32le(hdr, 0x3C) else {
        return;
    };
    let opt = e_lfanew as usize + 4 + 20;
    let Some(num_dirs) = u32le(hdr, opt + 108) else {
        return;
    };
    if DIR_LOAD_CONFIG >= num_dirs as usize {
        return;
    }
    let lc_rva = u32le(hdr, opt + 112 + DIR_LOAD_CONFIG * 8).unwrap_or(0) as usize;
    let lc_size = u32le(hdr, opt + 112 + DIR_LOAD_CONFIG * 8 + 4).unwrap_or(0) as usize;
    // Need the SecurityCookie field at load-config offset 0x58.
    if lc_rva == 0 || lc_size < 0x60 || lc_rva + 0x60 > size {
        return;
    }
    let cookie_va = unsafe { *((base as u64 + lc_rva as u64 + 0x58) as *const u64) };
    // The cookie must live inside this (kernel-mapped) image; ignore anything
    // pointing elsewhere rather than scribbling over unrelated memory.
    let lo = base as u64;
    let hi = lo + size as u64;
    if cookie_va < lo || cookie_va + 16 > hi {
        return;
    }
    // A per-load value from the timestamp counter, kept to 48 bits like a real
    // cookie and forced away from the two values the check rejects.
    let tsc: u64;
    unsafe {
        let (lo32, hi32): (u32, u32);
        core::arch::asm!("rdtsc", out("eax") lo32, out("edx") hi32, options(nomem, nostack));
        tsc = ((hi32 as u64) << 32) | lo32 as u64;
    }
    let mut cookie = (tsc ^ lo).rotate_left(17) & 0x0000_FFFF_FFFF_FFFF;
    if cookie == 0 || cookie == DEFAULT_COOKIE {
        cookie = DEFAULT_COOKIE ^ 0x1357;
    }
    unsafe {
        *(cookie_va as *mut u64) = cookie; // __security_cookie
        *((cookie_va + 8) as *mut u64) = !cookie; // __security_cookie_complement
    }
}

/// Load a PE32+ **user-mode** image (a ring-3 executable): map, relocate,
/// and mark it user-accessible + executable. Imports are *not* bound — a
/// user EXE that talks to this kernel issues `syscall`s directly and carries
/// no import table (one that imports kernel32/ntdll is rejected, since no
/// user-mode support DLLs exist yet).
pub fn load_user(data: &[u8]) -> Result<LoadedImage, NtStatus> {
    let m = map_and_relocate(data)?;
    if m.import_rva != 0 {
        // Bind imports against the user-mode modules: the ntdll syscall
        // trampoline plus any loaded support DLL (kernel32). Lets a console
        // app linked against ntdll.lib/kernel32.lib run unmodified.
        let img = unsafe { core::slice::from_raw_parts_mut(m.base, m.size) };
        resolve_imports(img, m.import_rva, super::loaded::resolve)?;
    }
    unsafe { crate::mm::virt::mm_set_user_executable(m.base as u64, m.size) };
    let entry_va = m.base as u64 + m.entry_rva as u64;
    Ok(LoadedImage {
        base: m.base,
        size: m.size,
        entry: unsafe { core::mem::transmute::<u64, DriverInitialize>(entry_va) },
        entry_va,
    })
}

/// A user image loaded into its **own address space**.
pub struct LoadedProcess {
    /// The process address space (PML4 physical base) to load into CR3.
    pub cr3: crate::mm::PhysAddr,
    /// Entry point virtual address (a low-half user address).
    pub entry_va: u64,
    /// Initial user stack pointer (low-half, ABI-aligned).
    pub user_rsp: u64,
    /// Thread Environment Block VA — the user-mode GS base. A real Windows
    /// binary reads `gs:[0x30]` (self), `gs:[0x60]` (PEB), etc.
    pub teb: u64,
    /// Image base (HINSTANCE) — the module's load address, which
    /// `GetModuleHandle(NULL)` returns and a `.mui` registration keys on.
    pub image_base: u64,
    /// Mapped image size (for the debugger's module map).
    pub image_size: u64,
    /// Physical base of the TEB/PEB/parameters control block. Lets the kernel
    /// patch process-parameter fields (e.g. the real command line) after load,
    /// via the physical window, without switching into the process address space.
    pub block_phys: crate::mm::PhysAddr,
    /// The process's distinct standard-stream console handles (stdin, stdout,
    /// stderr). The spawner installs these as the thread's std handles unless the
    /// parent staged a redirection (a pipe/file) for a given stream.
    pub std_console: [u64; 3],
}

/// Top of the per-process user stack (low canonical half).
const USER_STACK_TOP: u64 = 0x0000_7FFF_FFF0_0000;
/// User stack size in pages. Sized for a real MSVC CRT startup (security
/// init, locale, buffered I/O), not just our minimal apps.
const USER_STACK_PAGES: usize = 64; // 256 KiB
/// Per-process control-block region, just below the user stack. A real binary
/// reaches these through GS (the TEB) and the PEB pointer chained from it. We
/// lay them out at fixed offsets from [`TEB_BASE`]:
///   TEB (2 pages — `TlsSlots` lives at 0x1480), PEB, the process parameters
///   (with its UNICODE_STRING buffers), the loader module list, a TLS array,
///   and an environment block.
const TEB_BASE: u64 = 0x0000_7FFF_FFE0_0000;
const PEB_BASE: u64 = TEB_BASE + 0x2000;
const PARAMS_BASE: u64 = TEB_BASE + 0x3000;
const LDR_BASE: u64 = TEB_BASE + 0x4000;
const TLS_BASE: u64 = TEB_BASE + 0x5000;
const ENV_BASE: u64 = TEB_BASE + 0x6000;
/// VA of the optional per-process DLL-init trampoline (see
/// [`build_ulib_dllmain_trampoline`]), one page below the control-block region.
const ULIB_TRAMPOLINE_VA: u64 = TEB_BASE - 0x10000;
/// Pages backing the whole TEB…environment region above.
const USER_BLOCK_PAGES: usize = 8;
/// `GetProcessHeap` returns this; mirror it in `PEB.ProcessHeap` so a binary
/// that reads the field directly and a binary that calls the API agree.
const PROCESS_HEAP: u64 = 1;

/// Load a PE32+ user image into a **fresh per-process address space**, mapped
/// at its preferred ImageBase in the low (user) half. The image and a user
/// stack get their own physical pages mapped only in this address space;
/// imports bind to the (shared, high-half) kernel/`ntdll`/`kernel32` stubs.
///
/// Because we map at the preferred base, the relocation delta is zero — no
/// fixups are needed. Returns the CR3, entry, and initial user RSP.
pub fn load_user_process(data: &[u8]) -> Result<LoadedProcess, NtStatus> {
    // ---- Headers (same layout as map_and_relocate) ---------------------
    if u16le(data, 0) != Some(IMAGE_DOS_SIGNATURE) {
        return Err(bad());
    }
    let e_lfanew = u32le(data, 0x3C).ok_or(bad())? as usize;
    if u32le(data, e_lfanew) != Some(IMAGE_NT_SIGNATURE) {
        return Err(bad());
    }
    let coff = e_lfanew + 4;
    if u16le(data, coff) != Some(IMAGE_FILE_MACHINE_AMD64) {
        return Err(bad());
    }
    let num_sections = u16le(data, coff + 2).ok_or(bad())? as usize;
    let opt_size = u16le(data, coff + 16).ok_or(bad())? as usize;
    let opt = coff + 20;
    if u16le(data, opt) != Some(IMAGE_NT_OPTIONAL_HDR64_MAGIC) {
        return Err(bad());
    }
    let entry_rva = u32le(data, opt + 16).ok_or(bad())? as usize;
    let preferred_base = u64le(data, opt + 24).ok_or(bad())?;
    let size_of_image = u32le(data, opt + 56).ok_or(bad())? as usize;
    let size_of_headers = u32le(data, opt + 60).ok_or(bad())? as usize;
    let num_dirs = u32le(data, opt + 108).ok_or(bad())? as usize;
    let import_rva = if DIR_IMPORT < num_dirs {
        u32le(data, opt + 112 + DIR_IMPORT * 8).unwrap_or(0) as usize
    } else {
        0
    };
    let tls_rva = if DIR_TLS < num_dirs {
        u32le(data, opt + 112 + DIR_TLS * 8).unwrap_or(0) as usize
    } else {
        0
    };
    if size_of_image == 0 || size_of_image > 64 * 1024 * 1024 {
        return Err(bad());
    }
    // The image must want to live in the low (user) half.
    if preferred_base == 0 || preferred_base >= 0x0000_8000_0000_0000 {
        return Err(bad());
    }
    if entry_rva >= size_of_image {
        return Err(bad());
    }

    // ---- Allocate physical pages and lay the image out via the window --
    let pages = size_of_image.div_ceil(0x1000);
    let img_phys = crate::mm::phys::mm_allocate_contiguous_pages(pages)
        .ok_or(NtStatus::INSUFFICIENT_RESOURCES)?;
    let win = crate::mm::phys_to_virt(img_phys); // high-half view for setup
    unsafe { core::ptr::write_bytes(win, 0, size_of_image) };
    unsafe {
        let hdr_len = size_of_headers.min(data.len());
        core::ptr::copy_nonoverlapping(data.as_ptr(), win, hdr_len);
    }
    let sec_table = opt + opt_size;
    for s in 0..num_sections {
        let sh = sec_table + s * 40;
        let virt_addr = u32le(data, sh + 12).ok_or(bad())? as usize;
        let raw_size = u32le(data, sh + 16).ok_or(bad())? as usize;
        let raw_ptr = u32le(data, sh + 20).ok_or(bad())? as usize;
        if raw_size == 0 {
            continue;
        }
        let src = data.get(raw_ptr..raw_ptr + raw_size).ok_or(bad())?;
        if virt_addr + raw_size > size_of_image {
            return Err(bad());
        }
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), win.add(virt_addr), raw_size) };
    }

    // Imports bind to the shared high-half stubs (delta == 0, no relocs).
    let img = unsafe { core::slice::from_raw_parts_mut(win, size_of_image) };
    let uses_ulib = import_rva != 0 && imports_dll_prefix(img, import_rva, "ulib");
    if import_rva != 0 {
        resolve_imports(img, import_rva, super::loaded::resolve)?;
    }

    // ---- Build the address space and map image + stack into the low half
    let cr3 = unsafe { crate::mm::virt::mm_create_address_space() };
    unsafe { crate::mm::virt::mm_map_user_range(cr3, preferred_base, img_phys, pages, true, true) };

    let stk_phys = crate::mm::phys::mm_allocate_contiguous_pages(USER_STACK_PAGES)
        .ok_or(NtStatus::INSUFFICIENT_RESOURCES)?;
    let stk_base = USER_STACK_TOP - (USER_STACK_PAGES as u64 * 0x1000);
    unsafe { crate::mm::virt::mm_map_user_range(cr3, stk_base, stk_phys, USER_STACK_PAGES, true, false) };

    // Set up the initial frame as the Microsoft x64 ABI requires of a *called*
    // function: a return address at [RSP] plus 32 bytes of shadow space above
    // it (which a real CRT entry's prologue spills register args into). The
    // return address is the ntdll `NtTerminateThread` stub, so if the entry
    // ever returns the thread terminates cleanly. RSP ≡ 8 (mod 16) — the
    // post-`call` state. (Our own no-CRT apps don't need the shadow space but
    // are unaffected by its presence.)
    let user_rsp = USER_STACK_TOP - 0x28; // 8 (ret addr) + 0x20 (shadow space)
    unsafe {
        let win = crate::mm::phys_to_virt(stk_phys);
        let slot = win.add((user_rsp - stk_base) as usize) as *mut u64;
        *slot = super::ntdll::trampoline_base(); // NtTerminateThread stub (svc 0)
    }

    // ---- TEB / PEB / process parameters / loader list / TLS / environment.
    // A real binary reaches the thread block through GS and the process block
    // (PEB) chained from it; the CRT and many APIs read standard handles, the
    // current directory, the loaded-module list and thread-local storage from
    // these. Lay them all out below the stack.
    let (block_phys, std_console) = setup_user_blocks(SetupBlocks {
        cr3,
        preferred_base,
        stk_base,
        entry_rva,
        size_of_image,
        img_phys,
        tls_rva,
    })?;

    // A ulib-based tool needs ulib's DllMain to run (standard-stream / heap
    // init) before its own entry; route the entry through a trampoline that
    // does so. Non-ulib images run their entry directly.
    let real_entry = preferred_base + entry_rva as u64;
    let entry_va = if uses_ulib {
        build_ulib_dllmain_trampoline(cr3, real_entry)
    } else {
        real_entry
    };

    Ok(LoadedProcess {
        cr3,
        entry_va,
        user_rsp,
        teb: TEB_BASE,
        image_base: preferred_base,
        image_size: size_of_image as u64,
        block_phys,
        std_console,
    })
}

/// Inputs for [`setup_user_blocks`] (grouped to keep the argument list sane).
struct SetupBlocks {
    cr3: crate::mm::PhysAddr,
    preferred_base: u64,
    stk_base: u64,
    entry_rva: usize,
    size_of_image: usize,
    img_phys: crate::mm::PhysAddr,
    tls_rva: usize,
}

/// Open a fresh handle to `\Device\Console` in the **new process's** handle
/// table (keyed by its `cr3`), for its standard input/output/error. This runs in
/// the spawning thread's context, so it must target `cr3` explicitly rather than
/// the caller's table. 0 if the device is absent.
fn open_console_handle(cr3: u64) -> u64 {
    let name = crate::io::AbiUnicodeString::from_units(crate::w!("\\Device\\Console"));
    match crate::io::namespace::lookup_device(&name) {
        Ok(dev) => crate::ob::handle::ob_create_handle_in(cr3, dev as *mut u8, 0),
        Err(_) => 0,
    }
}

/// Per-load (process, thread) id pair for `TEB.ClientId`. The real scheduler ids
/// aren't known until the thread is created; these are unique, non-zero values
/// so code that only checks "is this a valid id" is satisfied.
fn next_client_id() -> (u64, u64) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x100);
    let n = NEXT.fetch_add(8, Ordering::Relaxed);
    (n, n + 4)
}

/// Populate the TEB, PEB, `RTL_USER_PROCESS_PARAMETERS`, `PEB_LDR_DATA`, the
/// static-TLS array, and an environment block for a freshly loaded image.
/// Offsets follow the x64 NT layout. Best-effort: a field we can't fill yet
/// (e.g. the real command line, set later per-thread) gets a sane placeholder.
fn setup_user_blocks(s: SetupBlocks) -> Result<(crate::mm::PhysAddr, [u64; 3]), NtStatus> {
    let blk = crate::mm::phys::mm_allocate_contiguous_pages(USER_BLOCK_PAGES)
        .ok_or(NtStatus::INSUFFICIENT_RESOURCES)?;
    unsafe {
        crate::mm::virt::mm_map_user_range(s.cr3, TEB_BASE, blk, USER_BLOCK_PAGES, true, false)
    };
    // Distinct console handles for stdin/stdout/stderr (as on Windows), so a
    // program that closes one standard stream - cmd closes its stdout handle
    // when tearing down `dir > file`, sort closes stdin after reading - does not
    // invalidate the others. A single shared handle made cmd's stdout-close kill
    // its stdin, so the shell hit EOF and exited after every redirect.
    let std_console =
        [open_console_handle(s.cr3.0), open_console_handle(s.cr3.0), open_console_handle(s.cr3.0)];
    let console = std_console[0];
    let (pid, tid) = next_client_id();

    // Region offsets within the mapped block (VA == TEB_BASE + offset).
    const PEB_OFF: usize = (PEB_BASE - TEB_BASE) as usize;
    const PARAMS_OFF: usize = (PARAMS_BASE - TEB_BASE) as usize;
    const LDR_OFF: usize = (LDR_BASE - TEB_BASE) as usize;
    const TLS_OFF: usize = (TLS_BASE - TEB_BASE) as usize;
    const ENV_OFF: usize = (ENV_BASE - TEB_BASE) as usize;

    unsafe {
        let b = crate::mm::phys_to_virt(blk);
        core::ptr::write_bytes(b, 0, USER_BLOCK_PAGES * 0x1000);
        let w64 = |off: usize, v: u64| *(b.add(off) as *mut u64) = v;
        let w32 = |off: usize, v: u32| *(b.add(off) as *mut u32) = v;
        let w16 = |off: usize, v: u16| *(b.add(off) as *mut u16) = v;
        let w8 = |off: usize, v: u8| *(b.add(off) as *mut u8) = v;
        // Write a NUL-terminated UTF-16 string (from ASCII) at `off`; returns
        // its length in characters (excluding the NUL).
        let wbuf = |off: usize, str: &[u8]| -> usize {
            for (i, &c) in str.iter().enumerate() {
                *(b.add(off + i * 2) as *mut u16) = c as u16;
            }
            *(b.add(off + str.len() * 2) as *mut u16) = 0;
            str.len()
        };
        // Fill a UNICODE_STRING header at `hdr` for a string at VA `buf_va`,
        // `nchars` long (Length excludes the NUL, MaximumLength includes it).
        let wstr = |hdr: usize, buf_va: u64, nchars: usize| {
            w16(hdr, (nchars * 2) as u16);
            w16(hdr + 2, ((nchars + 1) * 2) as u16);
            w64(hdr + 8, buf_va);
        };

        // ---- TEB (NT_TIB at offset 0) ----
        w64(0x08, USER_STACK_TOP); // NtTib.StackBase
        w64(0x10, s.stk_base); // NtTib.StackLimit
        w64(0x30, TEB_BASE); // NtTib.Self
        w64(0x40, pid); // ClientId.UniqueProcess
        w64(0x48, tid); // ClientId.UniqueThread
        w64(0x58, TLS_BASE); // ThreadLocalStoragePointer (the TLS array)
        w64(0x60, PEB_BASE); // ProcessEnvironmentBlock
        w32(0x68, 0); // LastErrorValue
        // (TlsSlots[64] live at 0x1480 inside the 2-page TEB, zeroed.)

        // ---- PEB ----
        w8(PEB_OFF + 0x02, 0); // BeingDebugged
        w64(PEB_OFF + 0x10, s.preferred_base); // ImageBaseAddress
        w64(PEB_OFF + 0x18, LDR_BASE); // Ldr (PEB_LDR_DATA)
        w64(PEB_OFF + 0x20, PARAMS_BASE); // ProcessParameters
        w64(PEB_OFF + 0x30, PROCESS_HEAP); // ProcessHeap
        w32(PEB_OFF + 0xBC, 0); // NtGlobalFlag
        w32(PEB_OFF + 0x118, 1); // OSMajorVersion
        w32(PEB_OFF + 0x11C, 0); // OSMinorVersion
        w16(PEB_OFF + 0x120, 1); // OSBuildNumber
        w32(PEB_OFF + 0x124, 2); // OSPlatformId = VER_PLATFORM_WIN32_NT

        // ---- RTL_USER_PROCESS_PARAMETERS ----
        // String buffers sit after the struct body (well under one page).
        let cd_off = PARAMS_OFF + 0x400;
        let cd_n = wbuf(cd_off, b"C:\\");
        let ip_off = cd_off + (cd_n + 1) * 2;
        let ip_n = wbuf(ip_off, b"C:\\program.exe");
        w32(PARAMS_OFF, (USER_BLOCK_PAGES * 0x1000) as u32); // MaximumLength
        w32(PARAMS_OFF + 0x04, (USER_BLOCK_PAGES * 0x1000) as u32); // Length
        w64(PARAMS_OFF + 0x10, console); // ConsoleHandle
        w64(PARAMS_OFF + 0x20, std_console[0]); // StandardInput
        w64(PARAMS_OFF + 0x28, std_console[1]); // StandardOutput
        w64(PARAMS_OFF + 0x30, std_console[2]); // StandardError
        // CurrentDirectory: CURDIR { UNICODE_STRING DosPath @0x38; HANDLE @0x48 }
        wstr(PARAMS_OFF + 0x38, TEB_BASE + cd_off as u64, cd_n);
        wstr(PARAMS_OFF + 0x60, TEB_BASE + (ip_off) as u64, ip_n); // ImagePathName
        // CommandLine lives in a dedicated buffer (CMDLINE_BUF_OFF) so the real
        // invocation can be patched in after load via `set_command_line` — a
        // binary that reads `PEB.ProcessParameters.CommandLine` directly (e.g.
        // ulib-based tools like more.com) needs the actual arguments, not the
        // image path. Seed it with the placeholder for callers that never patch.
        let cl_off = PARAMS_OFF + CMDLINE_BUF_OFF;
        let cl_n = wbuf(cl_off, b"C:\\program.exe");
        wstr(PARAMS_OFF + 0x70, TEB_BASE + cl_off as u64, cl_n); // CommandLine
        w64(PARAMS_OFF + 0x80, ENV_BASE); // Environment

        // ---- PEB_LDR_DATA + one LDR_DATA_TABLE_ENTRY for the image ----
        // The three module lists are circular; with a single entry each list
        // head and the entry's links point at each other.
        let entry = LDR_OFF + 0x100;
        let entry_va = LDR_BASE + 0x100;
        w32(LDR_OFF, 0x58); // Length
        w8(LDR_OFF + 0x04, 1); // Initialized
        // InLoadOrder (head @ +0x10 ↔ entry @ +0x00)
        w64(LDR_OFF + 0x10, entry_va);
        w64(LDR_OFF + 0x18, entry_va);
        w64(entry, LDR_BASE + 0x10);
        w64(entry + 0x08, LDR_BASE + 0x10);
        // InMemoryOrder (head @ +0x20 ↔ entry @ +0x10)
        w64(LDR_OFF + 0x20, entry_va + 0x10);
        w64(LDR_OFF + 0x28, entry_va + 0x10);
        w64(entry + 0x10, LDR_BASE + 0x20);
        w64(entry + 0x18, LDR_BASE + 0x20);
        // InInitializationOrder (head @ +0x30 ↔ entry @ +0x20)
        w64(LDR_OFF + 0x30, entry_va + 0x20);
        w64(LDR_OFF + 0x38, entry_va + 0x20);
        w64(entry + 0x20, LDR_BASE + 0x30);
        w64(entry + 0x28, LDR_BASE + 0x30);
        // Entry body
        w64(entry + 0x30, s.preferred_base); // DllBase
        w64(entry + 0x38, s.preferred_base + s.entry_rva as u64); // EntryPoint
        w32(entry + 0x40, s.size_of_image as u32); // SizeOfImage
        let nm_off = LDR_OFF + 0x200;
        let nm_n = wbuf(nm_off, b"program.exe");
        wstr(entry + 0x48, LDR_BASE + 0x200, nm_n); // FullDllName
        wstr(entry + 0x58, LDR_BASE + 0x200, nm_n); // BaseDllName

        // ---- Static TLS: array slot 0 → the image's TLS template copy ----
        // The array itself is at TLS_BASE (TEB.ThreadLocalStoragePointer). When
        // the image declares a TLS directory, copy its template into the block
        // after the array and set the module's `_tls_index` to 0.
        if s.tls_rva != 0 && s.tls_rva + 0x28 <= s.size_of_image {
            let iw = crate::mm::phys_to_virt(s.img_phys);
            let rd = |o: usize| *(iw.add(s.tls_rva + o) as *const u64);
            let start = rd(0x00);
            let end = rd(0x08);
            let idx_va = rd(0x10);
            let zero_fill = *(iw.add(s.tls_rva + 0x20) as *const u32) as usize;
            let in_image = |va: u64| {
                va >= s.preferred_base && va < s.preferred_base + s.size_of_image as u64
            };
            let raw = end.saturating_sub(start) as usize;
            let total = raw + zero_fill;
            let block_off = TLS_OFF + 0x80;
            if end >= start && in_image(start) && total <= 0x1000 - 0x80 {
                let src = iw.add((start - s.preferred_base) as usize);
                core::ptr::copy_nonoverlapping(src, b.add(block_off), raw);
                w64(TLS_OFF, TLS_BASE + 0x80); // tls array[0] -> block
                if in_image(idx_va) {
                    *(iw.add((idx_va - s.preferred_base) as usize) as *mut u32) = 0;
                }
            }
        }

        // ---- Environment block (NUL-separated UTF-16, double-NUL at end) ----
        // Mirror the kernel32 shim's defaults so a binary that reads the block
        // from the PEB (rather than via GetEnvironmentStrings) sees the same
        // variables — notably PATHEXT, which `where` needs to resolve a bare
        // command name to its `.exe`.
        let env_vars: &[&[u8]] = &[
            b"COMSPEC=C:\\cmd.exe",
            b"OS=nanokrnl",
            b"PATH=C:\\",
            b"PATHEXT=.COM;.EXE;.BAT;.CMD",
            b"PROMPT=$P$G",
            b"SystemRoot=C:\\NanoKrnl",
        ];
        let mut eo = ENV_OFF;
        for v in env_vars {
            let n = wbuf(eo, v);
            eo += (n + 1) * 2;
        }
        *(b.add(eo) as *mut u16) = 0; // block terminator (extra NUL)
    }
    Ok((blk, std_console))
}

/// Offset of the command-line buffer within the process-parameters region. It
/// sits past the current-directory / image-path strings (which end well before
/// +0x800) and leaves the rest of the page for the command line itself.
const CMDLINE_BUF_OFF: usize = 0x800;
/// Largest command line (in UTF-16 chars) the reserved buffer holds: the params
/// region runs to the next page (LDR_BASE), so 0x800..0x1000 = 0x800 bytes.
const CMDLINE_MAX_CHARS: usize = 0x800 / 2 - 1;

/// Patch a freshly loaded process's `PEB.ProcessParameters.CommandLine` with its
/// real invocation (ASCII `cmdline`, widened to UTF-16). Must be called before
/// the process first runs. Tools that read the command line straight from the
/// PEB — e.g. ulib-based binaries (more.com) — rely on this; the kernel32
/// `GetCommandLine` syscall path is fed separately (per-thread `cmdline_ptr`).
pub fn set_command_line(proc: &LoadedProcess, cmdline: &[u8]) {
    const PARAMS_OFF: usize = (PARAMS_BASE - TEB_BASE) as usize;
    let n = cmdline.len().min(CMDLINE_MAX_CHARS);
    unsafe {
        let b = crate::mm::phys_to_virt(proc.block_phys);
        let buf = b.add(PARAMS_OFF + CMDLINE_BUF_OFF);
        for i in 0..n {
            *(buf.add(i * 2) as *mut u16) = cmdline[i] as u16;
        }
        *(buf.add(n * 2) as *mut u16) = 0;
        // Update the CommandLine UNICODE_STRING header (ProcessParameters+0x70).
        let hdr = b.add(PARAMS_OFF + 0x70);
        *(hdr as *mut u16) = (n * 2) as u16; // Length (excl. NUL)
        *(hdr.add(2) as *mut u16) = ((n + 1) * 2) as u16; // MaximumLength
        *(hdr.add(8) as *mut u64) = PARAMS_BASE + CMDLINE_BUF_OFF as u64; // Buffer
    }
}

/// Patch a freshly loaded process's PEB standard handles (`StandardInput` /
/// `StandardOutput` / `StandardError`) with the handles the parent inherited to
/// it - a pipe end or a redirected file. cmd reads these straight from
/// `PEB.ProcessParameters` (not the `GetStdHandle` syscall), so `dir | sort` and
/// `> file` depend on them being set here. A zero entry keeps the console
/// default the block was seeded with. Must be called before the process runs.
pub fn set_std_handles(proc: &LoadedProcess, std_handles: [u64; 3]) {
    const PARAMS_OFF: usize = (PARAMS_BASE - TEB_BASE) as usize;
    // StandardInput @ +0x20, StandardOutput @ +0x28, StandardError @ +0x30.
    const STD_OFF: [usize; 3] = [0x20, 0x28, 0x30];
    unsafe {
        let b = crate::mm::phys_to_virt(proc.block_phys);
        for (i, &off) in STD_OFF.iter().enumerate() {
            if std_handles[i] != 0 {
                *(b.add(PARAMS_OFF + off) as *mut u64) = std_handles[i];
            }
        }
    }
}

/// Parse, allocate, copy sections, and apply base relocations. Shared by
/// [`load`] and [`load_user`].
fn map_and_relocate(data: &[u8]) -> Result<Mapped, NtStatus> {
    // ---- Headers -------------------------------------------------------
    if u16le(data, 0) != Some(IMAGE_DOS_SIGNATURE) {
        return Err(NtStatus(0xC000_0193)); // STATUS_INVALID_IMAGE_FORMAT
    }
    let e_lfanew = u32le(data, 0x3C).ok_or(bad())? as usize;
    if u32le(data, e_lfanew) != Some(IMAGE_NT_SIGNATURE) {
        return Err(bad());
    }
    let coff = e_lfanew + 4;
    if u16le(data, coff) != Some(IMAGE_FILE_MACHINE_AMD64) {
        return Err(bad());
    }
    let num_sections = u16le(data, coff + 2).ok_or(bad())? as usize;
    let opt_size = u16le(data, coff + 16).ok_or(bad())? as usize;
    let opt = coff + 20;
    if u16le(data, opt) != Some(IMAGE_NT_OPTIONAL_HDR64_MAGIC) {
        return Err(bad());
    }

    let entry_rva = u32le(data, opt + 16).ok_or(bad())? as usize;
    let preferred_base = u64le(data, opt + 24).ok_or(bad())?;
    let size_of_image = u32le(data, opt + 56).ok_or(bad())? as usize;
    let size_of_headers = u32le(data, opt + 60).ok_or(bad())? as usize;
    let num_dirs = u32le(data, opt + 108).ok_or(bad())? as usize;
    let dir = |i: usize| -> (usize, usize) {
        if i >= num_dirs {
            return (0, 0);
        }
        let base = opt + 112 + i * 8;
        (
            u32le(data, base).unwrap_or(0) as usize,
            u32le(data, base + 4).unwrap_or(0) as usize,
        )
    };
    let (import_rva, _import_size) = dir(DIR_IMPORT);
    let (reloc_rva, reloc_size) = dir(DIR_BASERELOC);

    if size_of_image == 0 || size_of_image > 64 * 1024 * 1024 {
        return Err(bad());
    }

    // ---- Allocate and map ---------------------------------------------
    let base = pool_alloc_checked(size_of_image, TAG_IMAGE)?;
    // Zero first: PE guarantees section bytes beyond SizeOfRawData (BSS) read
    // as zero, and the list-path pool does not zero on its own.
    unsafe { core::ptr::write_bytes(base, 0, size_of_image) };

    // Copy headers, then each section to its RVA.
    unsafe {
        let hdr_len = size_of_headers.min(data.len());
        core::ptr::copy_nonoverlapping(data.as_ptr(), base, hdr_len);
    }
    let sec_table = opt + opt_size;
    for s in 0..num_sections {
        let sh = sec_table + s * 40;
        let virt_addr = u32le(data, sh + 12).ok_or(bad())? as usize;
        let raw_size = u32le(data, sh + 16).ok_or(bad())? as usize;
        let raw_ptr = u32le(data, sh + 20).ok_or(bad())? as usize;
        if raw_size == 0 {
            continue; // pure BSS section: already zeroed
        }
        let src = data.get(raw_ptr..raw_ptr + raw_size).ok_or(bad())?;
        if virt_addr + raw_size > size_of_image {
            return Err(bad());
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), base.add(virt_addr), raw_size);
        }
    }

    // View of the freshly mapped image for in-place fixups.
    let img = unsafe { core::slice::from_raw_parts_mut(base, size_of_image) };

    // ---- Base relocations ---------------------------------------------
    let delta = (base as u64).wrapping_sub(preferred_base);
    if delta != 0 && reloc_size != 0 {
        apply_relocations(img, reloc_rva, reloc_size, delta)?;
    }

    if entry_rva >= size_of_image {
        return Err(bad());
    }

    Ok(Mapped {
        base,
        size: size_of_image,
        entry_rva,
        import_rva,
    })
}

/// STATUS_INVALID_IMAGE_FORMAT — the catch-all for a malformed image.
fn bad() -> NtStatus {
    NtStatus(0xC000_0193)
}

/// Fill `out` with the `(rva, virtual_size)` of each **writable** section of a
/// PE image (`.data`/`.bss`/`.didat`), returning how many were written. The
/// loader uses these to snapshot and privatize a shim DLL's mutable data (the
/// C-runtime's fd table, init flags, cached standard handles): the shim is
/// shared code in the high half, but that state must be per-process.
pub fn writable_sections(data: &[u8], out: &mut [(u32, u32)]) -> usize {
    const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;
    let mut parse = || -> Option<usize> {
        if u16le(data, 0)? != IMAGE_DOS_SIGNATURE {
            return None;
        }
        let e = u32le(data, 0x3C)? as usize;
        if u32le(data, e)? != IMAGE_NT_SIGNATURE {
            return None;
        }
        let coff = e + 4;
        let num_sections = u16le(data, coff + 2)? as usize;
        let opt_size = u16le(data, coff + 16)? as usize;
        let sec_table = coff + 20 + opt_size;
        let mut n = 0;
        for s in 0..num_sections {
            if n >= out.len() {
                break;
            }
            let sh = sec_table + s * 40;
            let vsize = u32le(data, sh + 8)?;
            let vaddr = u32le(data, sh + 12)?;
            let chars = u32le(data, sh + 36)?;
            if chars & IMAGE_SCN_MEM_WRITE != 0 && vsize > 0 {
                out[n] = (vaddr, vsize);
                n += 1;
            }
        }
        Some(n)
    };
    parse().unwrap_or(0)
}

/// Walk the `.reloc` directory and apply DIR64 fixups in place.
fn apply_relocations(img: &mut [u8], reloc_rva: usize, reloc_size: usize, delta: u64) -> Result<(), NtStatus> {
    let mut off = reloc_rva;
    let end = reloc_rva + reloc_size;
    while off + 8 <= end {
        let page_rva = u32le(img, off).ok_or(bad())? as usize;
        let block_size = u32le(img, off + 4).ok_or(bad())? as usize;
        if block_size < 8 {
            break; // malformed / terminator
        }
        let entries = (block_size - 8) / 2;
        for i in 0..entries {
            let e = u16le(img, off + 8 + i * 2).ok_or(bad())?;
            let typ = e >> 12;
            let fixup = (e & 0xFFF) as usize;
            match typ {
                IMAGE_REL_BASED_ABSOLUTE => {} // padding, ignore
                IMAGE_REL_BASED_DIR64 => {
                    let target = page_rva + fixup;
                    let cur = u64le(img, target).ok_or(bad())?;
                    let patched = cur.wrapping_add(delta);
                    img.get_mut(target..target + 8)
                        .ok_or(bad())?
                        .copy_from_slice(&patched.to_le_bytes());
                }
                _ => return Err(bad()), // unexpected reloc type for x64
            }
        }
        off += block_size;
    }
    Ok(())
}

/// Walk the import directory, binding each imported name via `resolver`.
/// Kernel images bind against the kernel export table; user images bind
/// against the user-mode ntdll trampoline. Fails with
/// STATUS_PROCEDURE_NOT_FOUND if a name can't be resolved (better a clean
/// load failure than a null-call crash later).
fn resolve_imports(
    img: &mut [u8],
    import_rva: usize,
    resolver: fn(&str) -> Option<usize>,
) -> Result<(), NtStatus> {
    let mut desc = import_rva;
    loop {
        // IMAGE_IMPORT_DESCRIPTOR: OFT@0, Name@12, FirstThunk@16; 20 bytes.
        let oft = u32le(img, desc).ok_or(bad())? as usize;
        let name_rva = u32le(img, desc + 12).ok_or(bad())? as usize;
        let iat = u32le(img, desc + 16).ok_or(bad())? as usize;
        if name_rva == 0 && iat == 0 && oft == 0 {
            break; // null terminator descriptor
        }
        // The DLL name (e.g. "ntoskrnl.exe") — accepted but not matched
        // against; all our exports live in a single namespace.
        let _dll = read_cstr(img, name_rva);

        // Read names from the ILT (OFT) when present, else from the IAT.
        let names_thunk = if oft != 0 { oft } else { iat };
        let mut i = 0usize;
        loop {
            let thunk = u64le(img, names_thunk + i * 8).ok_or(bad())?;
            if thunk == 0 {
                break; // end of this DLL's imports
            }
            let addr = if thunk & IMAGE_ORDINAL_FLAG64 != 0 {
                // By-ordinal import: we don't keep ordinal tables, so bind it
                // to a generic return-0 stub. Lets a binary that links an
                // ordinal it doesn't truly depend on (e.g. a WS2_32 ordinal)
                // load and run.
                let ord = thunk & 0xFFFF;
                let stub = super::loaded::ordinal_stub().ok_or(NtStatus(0xC000_007A))?;
                crate::kd_println!("LDR: import by ordinal #{} -> stub", ord);
                stub
            } else {
                // Hint/Name table entry: u16 hint, then ASCIIZ name.
                let by_name = thunk as usize;
                let name = read_cstr(img, by_name + 2).ok_or(bad())?;
                match resolver(name) {
                    Some(a) => a,
                    None => {
                        // Unimplemented import: bind to a distinct per-name
                        // return-0 stub (logs name->address), so the image
                        // loads and the API tracer can identify exactly which
                        // missing import a binary calls. Returns 0 like the
                        // shared stub; a binary that needs it may fault later,
                        // which the user-fault handler turns into a clean exit.
                        super::loaded::unresolved_stub(name).ok_or(NtStatus(0xC000_007A))?
                    }
                }
            };
            // Write the resolved address into the IAT slot.
            let slot = iat + i * 8;
            img.get_mut(slot..slot + 8)
                .ok_or(bad())?
                .copy_from_slice(&(addr as u64).to_le_bytes());
            i += 1;
        }
        desc += 20;
    }
    Ok(())
}

/// Does the image's import table name a DLL whose name starts with `prefix`
/// (case-insensitive)? Used to decide whether a process needs a dependent
/// DLL's `DllMain` run before its entry (e.g. `ulib`).
fn imports_dll_prefix(img: &[u8], import_rva: usize, prefix: &str) -> bool {
    let mut desc = import_rva;
    loop {
        let Some(name_rva) = u32le(img, desc + 12) else {
            return false;
        };
        let Some(iat) = u32le(img, desc + 16) else {
            return false;
        };
        let oft = u32le(img, desc).unwrap_or(0);
        if name_rva == 0 && iat == 0 && oft == 0 {
            return false; // null terminator
        }
        if let Some(name) = read_cstr(img, name_rva as usize) {
            let n = name.len().min(prefix.len());
            if name.len() >= prefix.len() && name[..n].eq_ignore_ascii_case(prefix) {
                return true;
            }
        }
        desc += 20;
    }
}

/// Build a one-page user trampoline that runs `ulib!DllMain(ulib_base,
/// DLL_PROCESS_ATTACH, NULL)` and then tail-jumps to the image's real entry,
/// preserving the entry's stack contract (RSP unchanged, `[RSP]` = the
/// NtTerminateThread return stub). Returns the trampoline VA to use as the
/// process entry, or the unchanged `real_entry` if anything is unavailable.
fn build_ulib_dllmain_trampoline(cr3: crate::mm::PhysAddr, real_entry: u64) -> u64 {
    let (ulib_base, ulib_entry) = super::loaded::ulib_base_and_entry();
    if ulib_base == 0 || ulib_entry == 0 {
        return real_entry;
    }
    let Some(pa) = crate::mm::phys::mm_allocate_page() else {
        return real_entry;
    };
    // The trampoline (RSP enters ≡ 8 mod 16, the post-call state the entry
    // wants):
    //   sub rsp,0x28 ; mov rcx,ulib_base ; mov edx,1 ; xor r8d,r8d
    //   mov rax,ulib_entry ; call rax ; add rsp,0x28
    //   mov rax,real_entry ; jmp rax
    fn put(code: &mut [u8], n: &mut usize, bytes: &[u8]) {
        code[*n..*n + bytes.len()].copy_from_slice(bytes);
        *n += bytes.len();
    }
    let mut code = [0u8; 64];
    let mut n = 0usize;
    put(&mut code, &mut n, &[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
    put(&mut code, &mut n, &[0x48, 0xB9]);
    put(&mut code, &mut n, &ulib_base.to_le_bytes()); // mov rcx, ulib_base
    put(&mut code, &mut n, &[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, 1
    put(&mut code, &mut n, &[0x45, 0x31, 0xC0]); // xor r8d, r8d
    put(&mut code, &mut n, &[0x48, 0xB8]);
    put(&mut code, &mut n, &ulib_entry.to_le_bytes()); // mov rax, ulib_entry
    put(&mut code, &mut n, &[0xFF, 0xD0]); // call rax
    put(&mut code, &mut n, &[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
    put(&mut code, &mut n, &[0x48, 0xB8]);
    put(&mut code, &mut n, &real_entry.to_le_bytes()); // mov rax, real_entry
    put(&mut code, &mut n, &[0xFF, 0xE0]); // jmp rax

    let va = ULIB_TRAMPOLINE_VA;
    unsafe {
        let w = crate::mm::phys_to_virt(pa);
        core::ptr::copy_nonoverlapping(code.as_ptr(), w, n);
        // Map user-accessible + executable (read-only — written above via the
        // physical window, so no user write access is needed).
        crate::mm::virt::mm_map_user_range(cr3, va, pa, 1, false, true);
    }
    va
}

/// Resolve an exported symbol `name` to its virtual address within an
/// already-mapped image at `base` (size `size`). Parses the PE export
/// directory (the EAT/ENPT/ordinal tables). This is the cross-module
/// dynamic-linking primitive: a DLL's exports, looked up by name.
///
/// # Safety
/// `base` must point to a fully mapped PE image of at least `size` bytes.
pub unsafe fn resolve_export(base: *const u8, size: usize, name: &str) -> Option<u64> {
    let img = unsafe { core::slice::from_raw_parts(base, size) };

    // Locate the export data directory (index 0) via the headers.
    if u16le(img, 0)? != IMAGE_DOS_SIGNATURE {
        return None;
    }
    let e_lfanew = u32le(img, 0x3C)? as usize;
    if u32le(img, e_lfanew)? != IMAGE_NT_SIGNATURE {
        return None;
    }
    let opt = e_lfanew + 24;
    let num_dirs = u32le(img, opt + 108)? as usize;
    if num_dirs == 0 {
        return None;
    }
    let export_rva = u32le(img, opt + 112)? as usize; // dir[0].VirtualAddress
    if export_rva == 0 {
        return None;
    }

    // IMAGE_EXPORT_DIRECTORY fields.
    let num_names = u32le(img, export_rva + 0x18)? as usize;
    let funcs_rva = u32le(img, export_rva + 0x1C)? as usize; // EAT (u32 RVAs)
    let names_rva = u32le(img, export_rva + 0x20)? as usize; // name-pointer table
    let ords_rva = u32le(img, export_rva + 0x24)? as usize; // ordinal table (u16)

    // Linear search the name table; on match, the parallel ordinal indexes
    // the export address table.
    for i in 0..num_names {
        let name_ptr_rva = u32le(img, names_rva + i * 4)? as usize;
        let exp_name = read_cstr(img, name_ptr_rva)?;
        if exp_name == name {
            let ordinal = u16le(img, ords_rva + i * 2)? as usize;
            let func_rva = u32le(img, funcs_rva + ordinal * 4)? as usize;
            return Some(base as u64 + func_rva as u64);
        }
    }
    None
}

/// Read an ASCIIZ string at `rva` within the image as `&str` (borrowed).
fn read_cstr(img: &[u8], rva: usize) -> Option<&str> {
    let bytes = img.get(rva..)?;
    let end = bytes.iter().position(|&b| b == 0)?;
    core::str::from_utf8(&bytes[..end]).ok()
}
