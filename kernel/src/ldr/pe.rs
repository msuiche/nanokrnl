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
    let entry_va = m.base as u64 + m.entry_rva as u64;
    Ok(LoadedImage {
        base: m.base,
        size: m.size,
        // SAFETY: entry is a DriverEntry with the Microsoft x64 ABI by contract.
        entry: unsafe { core::mem::transmute::<u64, DriverInitialize>(entry_va) },
        entry_va,
    })
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
}

/// Top of the per-process user stack (low canonical half).
const USER_STACK_TOP: u64 = 0x0000_7FFF_FFF0_0000;
/// User stack size in pages. Sized for a real MSVC CRT startup (security
/// init, locale, buffered I/O), not just our minimal apps.
const USER_STACK_PAGES: usize = 64; // 256 KiB
/// TEB / PEB virtual bases, two pages just below the user stack region.
const TEB_BASE: u64 = 0x0000_7FFF_FFE0_0000;
const PEB_BASE: u64 = TEB_BASE + 0x1000;

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

    // ---- TEB + PEB: a real binary reaches the thread/process blocks via GS.
    // Two pages just below the stack region; the user-mode GS base points at
    // the TEB. We fill only the fields an early CRT reads (stack bounds, the
    // TEB self-pointer, the PEB pointer, the image base); the rest is zero.
    let tp = crate::mm::phys::mm_allocate_contiguous_pages(2)
        .ok_or(NtStatus::INSUFFICIENT_RESOURCES)?;
    unsafe { crate::mm::virt::mm_map_user_range(cr3, TEB_BASE, tp, 2, true, false) };
    unsafe {
        let teb = crate::mm::phys_to_virt(tp);
        core::ptr::write_bytes(teb, 0, 2 * 0x1000);
        let w = |off: usize, v: u64| *(teb.add(off) as *mut u64) = v;
        // NT_TIB / TEB
        w(0x08, USER_STACK_TOP); // NtTib.StackBase
        w(0x10, stk_base); // NtTib.StackLimit
        w(0x30, TEB_BASE); // NtTib.Self
        w(0x60, PEB_BASE); // ProcessEnvironmentBlock
        // PEB (second page)
        w(0x1000 + 0x10, preferred_base); // PEB.ImageBaseAddress
        // OS version fields — a placeholder version a binary can report (e.g.
        // cmd.exe's banner reads major.minor.build from here). 1.0.1.
        let w32 = |off: usize, v: u32| *(teb.add(off) as *mut u32) = v;
        let w16 = |off: usize, v: u16| *(teb.add(off) as *mut u16) = v;
        w32(0x1000 + 0x118, 1); // PEB.OSMajorVersion
        w32(0x1000 + 0x11C, 0); // PEB.OSMinorVersion
        w16(0x1000 + 0x120, 1); // PEB.OSBuildNumber
        w32(0x1000 + 0x124, 2); // PEB.OSPlatformId = VER_PLATFORM_WIN32_NT
    }

    Ok(LoadedProcess {
        cr3,
        entry_va: preferred_base + entry_rva as u64,
        user_rsp,
        teb: TEB_BASE,
        image_base: preferred_base,
        image_size: size_of_image as u64,
    })
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
