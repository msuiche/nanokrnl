//! Windows-kernel-debugger data: a real `KDDEBUGGER_DATA64` block plus NT-shaped
//! `PsLoadedModuleList` and `PsActiveProcessHead`, so a Windows debugger can walk
//! nanokrnl the way it walks a real kernel: `lm` lists the loaded modules and
//! `!process 0 0` lists the processes.
//!
//! # How a debugger finds this
//!
//! The engine resolves the symbol `KdDebuggerDataBlock` (biased by the kernel
//! module's load base), reads the [`KddebuggerData64`] there, and follows its
//! `PsLoadedModuleList` / `PsActiveProcessHead` pointers into two circular
//! doubly-linked lists: one of `KLDR_DATA_TABLE_ENTRY` (modules), one of
//! `EPROCESS` (processes). We build both here from the live module table and
//! process table, refreshed just before a crash dump so the core contains a
//! coherent snapshot.
//!
//! # Addresses
//!
//! The kernel is linked at 0 but the loader maps it at [`KERNEL_VIRT_BASE`], so a
//! symbol's runtime address is `KERNEL_VIRT_BASE + its ELF vaddr`. Everything we
//! emit is therefore a higher-half address inside the captured dump range, and a
//! debugger that loads `kernel.bin`'s DWARF at module base `KERNEL_VIRT_BASE`
//! resolves these symbols to exactly those addresses.
//!
//! All the fields a debugger reads (list links, `DllBase`, `SizeOfImage`, the
//! `UNICODE_STRING` names, `EPROCESS.ImageFileName`, `UniqueProcessId`) sit at
//! their genuine NT offsets, so an off-the-shelf engine follows them without
//! custom scripting.

#![allow(non_upper_case_globals)]

use crate::ke::spinlock::SpinLock;

/// The virtual base the loader maps the kernel image at (it is linked at 0). A
/// symbol's runtime VA is this plus its link offset. Confirmed by the crash
/// dump's kernel `PT_LOAD` (`vaddr 0xffff800000000000`).
pub const KERNEL_VIRT_BASE: u64 = 0xffff_8000_0000_0000;

/// `_LIST_ENTRY` — a circular doubly-linked list node (Flink/Blink).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ListEntry {
    flink: u64,
    blink: u64,
}
impl ListEntry {
    const fn zero() -> Self {
        ListEntry { flink: 0, blink: 0 }
    }
}

/// `_UNICODE_STRING` — Length/MaximumLength in bytes, plus a pointer to UTF-16.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UnicodeString {
    length: u16,
    maximum_length: u16,
    _pad: u32,
    buffer: u64,
}
impl UnicodeString {
    const fn zero() -> Self {
        UnicodeString { length: 0, maximum_length: 0, _pad: 0, buffer: 0 }
    }
}

const NAME_CHARS: usize = 32;

/// `_KLDR_DATA_TABLE_ENTRY` (the fields `lm` reads), with the UTF-16 name buffer
/// carried inline so the whole entry lives in one place in the dump.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KldrEntry {
    in_load_order_links: ListEntry,        // 0x00
    in_memory_order_links: ListEntry,      // 0x10
    in_init_order_links: ListEntry,        // 0x20
    dll_base: u64,                         // 0x30
    entry_point: u64,                      // 0x38
    size_of_image: u32,                    // 0x40
    _pad0: u32,                            // 0x44
    full_dll_name: UnicodeString,          // 0x48
    base_dll_name: UnicodeString,          // 0x58
    flags: u32,                            // 0x68
    _pad1: u32,                            // 0x6c
    // Inline name storage the UNICODE_STRINGs point at (UTF-16).
    name_buf: [u16; NAME_CHARS],           // 0x70
}
impl KldrEntry {
    const fn zero() -> Self {
        KldrEntry {
            in_load_order_links: ListEntry::zero(),
            in_memory_order_links: ListEntry::zero(),
            in_init_order_links: ListEntry::zero(),
            dll_base: 0,
            entry_point: 0,
            size_of_image: 0,
            _pad0: 0,
            full_dll_name: UnicodeString::zero(),
            base_dll_name: UnicodeString::zero(),
            flags: 0,
            _pad1: 0,
            name_buf: [0; NAME_CHARS],
        }
    }
}

const IMAGE_NAME_LEN: usize = 16;

/// `_EPROCESS` (the fields `!process` reads). Compact: a debugger takes these
/// offsets from the type, and everything it needs to list a process is here.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Eprocess {
    unique_process_id: u64,             // 0x00  PID (as a HANDLE-shaped value)
    active_process_links: ListEntry,    // 0x08  links in PsActiveProcessHead
    directory_table_base: u64,          // 0x18  CR3
    peb: u64,                           // 0x20
    image_file_name: [u8; IMAGE_NAME_LEN], // 0x28  ASCII, NUL-padded
}
impl Eprocess {
    const fn zero() -> Self {
        Eprocess {
            unique_process_id: 0,
            active_process_links: ListEntry::zero(),
            directory_table_base: 0,
            peb: 0,
            image_file_name: [0; IMAGE_NAME_LEN],
        }
    }
}

/// `_DBGKD_DEBUG_DATA_HEADER64` — the head of [`KddebuggerData64`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DebugDataHeader64 {
    list: ListEntry,
    owner_tag: u32, // 'KDBG'
    size: u32,
}

/// `_KDDEBUGGER_DATA64`. Only the fields a debugger needs to bootstrap the module
/// and process lists are named; the remainder (breakpoint helpers, pool tags,
/// the long tail of `Offset*`/`Size*` fields) is left zero, which is valid - a
/// real block also zeroes fields the target build does not use.
#[repr(C)]
pub struct KddebuggerData64 {
    header: DebugDataHeader64,     // 0x00 (24 bytes)
    kern_base: u64,                // 0x18
    breakpoint_with_status: u64,   // 0x20
    saved_context: u64,            // 0x28
    th_callback_stack: u16,        // 0x30
    next_callback: u16,            // 0x32
    frame_pointer: u16,            // 0x34
    pae_enabled: u16,              // 0x36
    ki_call_user_mode: u64,        // 0x38
    ke_user_callback_dispatcher: u64, // 0x40
    ps_loaded_module_list: u64,    // 0x48
    ps_active_process_head: u64,   // 0x50
    // Pad to the full KDDEBUGGER_DATA64 size (0x340). The remaining fields are
    // not needed to walk the module/process lists.
    _rest: [u8; 0x340 - 0x58],
}

const KDBG_OWNER_TAG: u32 = u32::from_le_bytes(*b"KDBG");
const KDBG_SIZE: u32 = 0x340;

/// Symbols a debugger looks up by name. `#[no_mangle]` keeps the exact names in
/// the symbol table / DWARF so the engine (and our validator) can find them.
///
/// SAFETY: these are only mutated under [`KD_LOCK`] from [`refresh`], which runs
/// single-threaded on the crash path; a debugger only ever reads them.
#[no_mangle]
pub static mut KdDebuggerDataBlock: KddebuggerData64 = KddebuggerData64 {
    header: DebugDataHeader64 { list: ListEntry::zero(), owner_tag: KDBG_OWNER_TAG, size: KDBG_SIZE },
    kern_base: 0,
    breakpoint_with_status: 0,
    saved_context: 0,
    th_callback_stack: 0,
    next_callback: 0,
    frame_pointer: 0,
    pae_enabled: 0,
    ki_call_user_mode: 0,
    ke_user_callback_dispatcher: 0,
    ps_loaded_module_list: 0,
    ps_active_process_head: 0,
    _rest: [0; 0x340 - 0x58],
};

/// Head of the loaded-module list (`lm` walks this via `InLoadOrderLinks`).
#[no_mangle]
pub static mut PsLoadedModuleList: ListEntry = ListEntry::zero();

/// Head of the active-process list (`!process 0 0` walks this).
#[no_mangle]
pub static mut PsActiveProcessHead: ListEntry = ListEntry::zero();

const MAX_MODULES: usize = 16;
const MAX_PROCS: usize = 16;

#[no_mangle]
static mut KdModuleEntries: [KldrEntry; MAX_MODULES] = [KldrEntry::zero(); MAX_MODULES];
#[no_mangle]
static mut KdProcessEntries: [Eprocess; MAX_PROCS] = [Eprocess::zero(); MAX_PROCS];

static KD_LOCK: SpinLock<()> = SpinLock::new(());
static mut N_MODULES: usize = 0;
static mut N_PROCS: usize = 0;
/// `KernBase` for the debugger data block: the base of the first module pushed
/// (which the caller makes the kernel itself).
static mut KERN_BASE: u64 = KERNEL_VIRT_BASE;

/// The runtime VA of a `&raw` reference to one of our statics is already
/// `KERNEL_VIRT_BASE + link offset`, so addresses are used directly.
#[inline]
fn addr<T>(p: *const T) -> u64 {
    p as u64
}

/// Start rebuilding the module/process lists. Call [`push_module`] (kernel
/// first) then [`push_process`], then [`commit`]. Single-threaded on the crash
/// path; a debugger only ever reads the results.
pub fn begin() {
    let _g = KD_LOCK.lock();
    unsafe {
        N_MODULES = 0;
        N_PROCS = 0;
        KERN_BASE = KERNEL_VIRT_BASE;
    }
}

/// Publish a loaded module in `PsLoadedModuleList`. The first one pushed sets
/// `KernBase`.
pub fn push_module(base: u64, size: u64, name: &[u8]) {
    let _g = KD_LOCK.lock();
    unsafe {
        let i = N_MODULES;
        if i >= MAX_MODULES {
            return;
        }
        if i == 0 {
            KERN_BASE = base;
        }
        let e = &mut (*(&raw mut KdModuleEntries))[i];
        *e = KldrEntry::zero();
        e.dll_base = base;
        e.size_of_image = size as u32;
        let n = name.len().min(NAME_CHARS - 1);
        for (j, &b) in name.iter().take(n).enumerate() {
            e.name_buf[j] = b as u16;
        }
        e.name_buf[n] = 0;
        let buf_va = addr(e.name_buf.as_ptr());
        let bytes = (n * 2) as u16;
        e.base_dll_name =
            UnicodeString { length: bytes, maximum_length: bytes + 2, _pad: 0, buffer: buf_va };
        e.full_dll_name = e.base_dll_name;
        N_MODULES = i + 1;
    }
}

/// Publish a process in `PsActiveProcessHead`.
pub fn push_process(pid: u64, cr3: u64, peb: u64, name: &[u8]) {
    let _g = KD_LOCK.lock();
    unsafe {
        let i = N_PROCS;
        if i >= MAX_PROCS {
            return;
        }
        let e = &mut (*(&raw mut KdProcessEntries))[i];
        *e = Eprocess::zero();
        e.unique_process_id = pid;
        e.directory_table_base = cr3;
        e.peb = peb;
        let n = name.len().min(IMAGE_NAME_LEN - 1);
        for (j, &b) in name.iter().take(n).enumerate() {
            e.image_file_name[j] = b;
        }
        N_PROCS = i + 1;
    }
}

/// Link the two list rings and fill in `KdDebuggerDataBlock`. Everything the
/// debugger reads is coherent after this returns.
pub fn commit() {
    let _g = KD_LOCK.lock();
    unsafe {
        // ---- Loaded-module ring (via InLoadOrderLinks) ----
        let nmods = N_MODULES;
        let head = addr(&raw const PsLoadedModuleList);
        for i in 0..nmods {
            let next = if i + 1 < nmods {
                addr(&raw const (*(&raw const KdModuleEntries))[i + 1].in_load_order_links)
            } else {
                head
            };
            let prev = if i == 0 {
                head
            } else {
                addr(&raw const (*(&raw const KdModuleEntries))[i - 1].in_load_order_links)
            };
            (*(&raw mut KdModuleEntries))[i].in_load_order_links =
                ListEntry { flink: next, blink: prev };
        }
        if nmods == 0 {
            PsLoadedModuleList = ListEntry { flink: head, blink: head };
        } else {
            let first = addr(&raw const (*(&raw const KdModuleEntries))[0].in_load_order_links);
            let last =
                addr(&raw const (*(&raw const KdModuleEntries))[nmods - 1].in_load_order_links);
            PsLoadedModuleList = ListEntry { flink: first, blink: last };
        }

        // ---- Active-process ring (via ActiveProcessLinks) ----
        let nprocs = N_PROCS;
        let phead = addr(&raw const PsActiveProcessHead);
        for i in 0..nprocs {
            let next = if i + 1 < nprocs {
                addr(&raw const (*(&raw const KdProcessEntries))[i + 1].active_process_links)
            } else {
                phead
            };
            let prev = if i == 0 {
                phead
            } else {
                addr(&raw const (*(&raw const KdProcessEntries))[i - 1].active_process_links)
            };
            (*(&raw mut KdProcessEntries))[i].active_process_links =
                ListEntry { flink: next, blink: prev };
        }
        if nprocs == 0 {
            PsActiveProcessHead = ListEntry { flink: phead, blink: phead };
        } else {
            let first = addr(&raw const (*(&raw const KdProcessEntries))[0].active_process_links);
            let last =
                addr(&raw const (*(&raw const KdProcessEntries))[nprocs - 1].active_process_links);
            PsActiveProcessHead = ListEntry { flink: first, blink: last };
        }

        // ---- Debugger data block ----
        let self_va = addr(&raw const KdDebuggerDataBlock.header.list);
        let d = &mut *(&raw mut KdDebuggerDataBlock);
        d.header.list = ListEntry { flink: self_va, blink: self_va };
        d.header.owner_tag = KDBG_OWNER_TAG;
        d.header.size = KDBG_SIZE;
        d.kern_base = KERN_BASE;
        d.ps_loaded_module_list = head;
        d.ps_active_process_head = phead;
    }
}
