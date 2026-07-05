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

/// `ProcessObject` from the NT `KOBJECTS` enum - the `_DISPATCHER_HEADER.Type`
/// value a process's `_KPROCESS` carries. `!process` validates this (its check is
/// separate from the object-header type `!object` uses), so it must sit at
/// `_EPROCESS + 0` (= `Pcb.Header.Type`).
const PROCESS_OBJECT_TYPE: u8 = 3;

/// `_EPROCESS` (the fields `!process` reads). Compact, but offset 0 must be the
/// embedded `_KPROCESS`'s dispatcher `Type` (not the PID) - `!process` reads
/// `Pcb.Header.Type` and rejects the object otherwise. `UniqueProcessId` moves to
/// 0x38 so it no longer overlaps that byte.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Eprocess {
    dispatcher_type: u8,                   // 0x00  Pcb.Header.Type = ProcessObject(3)
    _pad0: [u8; 7],                        // 0x01
    active_process_links: ListEntry,       // 0x08  links in PsActiveProcessHead
    directory_table_base: u64,             // 0x18  CR3 (also Pcb.DirectoryTableBase)
    peb: u64,                              // 0x20
    image_file_name: [u8; IMAGE_NAME_LEN], // 0x28  ASCII, NUL-padded
    unique_process_id: u64,                // 0x38  PID (as a HANDLE-shaped value)
}
impl Eprocess {
    const fn zero() -> Self {
        Eprocess {
            dispatcher_type: 0,
            _pad0: [0; 7],
            active_process_links: ListEntry::zero(),
            directory_table_base: 0,
            peb: 0,
            image_file_name: [0; IMAGE_NAME_LEN],
            unique_process_id: 0,
        }
    }
}

/// Size of an `_OBJECT_HEADER` from its start to the object body (`Body` @0x30).
const OBJECT_HEADER_SIZE: usize = 0x30;

/// `_OBJECT_HEADER` (amd64). `!process` validates that an address is really a
/// process object: it reads the header sitting `OBJECT_HEADER_SIZE` bytes before
/// the `_EPROCESS`, decodes `TypeIndex` (@0x18) and compares the resulting
/// `_OBJECT_TYPE*` against `nt!PsProcessType`. Without this it prints
/// "TYPE mismatch for process object". Only the fields the engine reads are named.
#[repr(C)]
#[derive(Clone, Copy)]
struct ObjectHeader {
    pointer_count: u64,       // 0x00
    handle_count: u64,        // 0x08
    lock: u64,                // 0x10
    type_index: u8,           // 0x18  obfuscated index into nt!ObTypeIndexTable
    trace_flags: u8,          // 0x19
    info_mask: u8,            // 0x1a
    flags: u8,                // 0x1b
    _pad0: u32,               // 0x1c
    quota_block: u64,         // 0x20
    security_descriptor: u64, // 0x28
    // Body (the _EPROCESS) begins at 0x30.
}
impl ObjectHeader {
    const fn zero() -> Self {
        ObjectHeader {
            pointer_count: 0,
            handle_count: 0,
            lock: 0,
            type_index: 0,
            trace_flags: 0,
            info_mask: 0,
            flags: 0,
            _pad0: 0,
            quota_block: 0,
            security_descriptor: 0,
        }
    }
}

/// A process object as it lives in kernel memory: an `_OBJECT_HEADER` immediately
/// followed by the `_EPROCESS` body. Every address we publish for a process (the
/// active-process links, `PsInitialSystemProcess`, `!process <addr>`) points at
/// `body`; the header sits at `&body - OBJECT_HEADER_SIZE` for the type check.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessObject {
    object_header: ObjectHeader, // 0x00
    body: Eprocess,              // 0x30
}
impl ProcessObject {
    const fn zero() -> Self {
        ProcessObject { object_header: ObjectHeader::zero(), body: Eprocess::zero() }
    }
}

/// Size of an amd64 `CONTEXT` record (what the debugger reads for register state).
pub const CONTEXT_SIZE: usize = 0x4d0;

/// `_KDESCRIPTOR` — GDTR/IDTR as stored in `KSPECIAL_REGISTERS`: six pad bytes,
/// then the 16-bit limit and 64-bit base.
#[repr(C)]
#[derive(Clone, Copy)]
struct Kdescriptor {
    pad: [u16; 3],
    limit: u16,
    base: u64,
}
impl Kdescriptor {
    const fn zero() -> Self {
        Kdescriptor { pad: [0; 3], limit: 0, base: 0 }
    }
}

/// `_KSPECIAL_REGISTERS` (amd64) — the control/descriptor registers WinDbg needs
/// to establish machine context (`GetContextState`) and resolve the CS
/// descriptor (via `Gdtr`). Laid out at its genuine NT offsets; fields the engine
/// does not read for a dump are left zero.
#[repr(C)]
#[derive(Clone, Copy)]
struct KspecialRegisters {
    cr0: u64,             // 0x00
    cr2: u64,             // 0x08
    cr3: u64,             // 0x10
    cr4: u64,             // 0x18
    kernel_dr: [u64; 6],  // 0x20  Dr0,Dr1,Dr2,Dr3,Dr6,Dr7
    gdtr: Kdescriptor,    // 0x50
    idtr: Kdescriptor,    // 0x60
    tr: u16,              // 0x70
    ldtr: u16,            // 0x72
    mxcsr: u32,           // 0x74
    debug_control: u64,   // 0x78
    last_branch_to: u64,  // 0x80
    last_branch_from: u64, // 0x88
    last_exception_to: u64, // 0x90
    last_exception_from: u64, // 0x98
    cr8: u64,             // 0xa0
    msr: [u64; 6],        // 0xa8  GsBase,GsSwap,Star,LStar,CStar,SyscallMask (ends 0xd8)
}
impl KspecialRegisters {
    const fn zero() -> Self {
        KspecialRegisters {
            cr0: 0, cr2: 0, cr3: 0, cr4: 0,
            kernel_dr: [0; 6],
            gdtr: Kdescriptor::zero(),
            idtr: Kdescriptor::zero(),
            tr: 0, ldtr: 0, mxcsr: 0,
            debug_control: 0, last_branch_to: 0, last_branch_from: 0,
            last_exception_to: 0, last_exception_from: 0,
            cr8: 0,
            msr: [0; 6],
        }
    }
}

/// Layout of our synthetic `KPRCB`. Real PRCBs never place `CurrentThread` or
/// `ProcessorState` at offset 0, and the engine treats `PRCB+0` reads (from a
/// zero `OffsetPrcb*`) as garbage - so keep both at realistic nonzero offsets.
const PRCB_CURRENT_THREAD: usize = 0x08;
const PRCB_PROCESSOR_STATE: usize = 0x180;

/// A synthetic `KPRCB` carrying just what the debugger reads: a `CurrentThread`
/// pointer and the `ProcessorState` (`KPROCESSOR_STATE` = `SpecialRegisters` +
/// `ContextFrame`). WinDbg finds each via the `OffsetPrcb*` fields we publish in
/// [`KddebuggerData64`]. `context` (align 1) sits right after `special`.
#[repr(C)]
struct Kprcb {
    head: [u8; PRCB_PROCESSOR_STATE], // CurrentThread pointer lives at PRCB_CURRENT_THREAD
    special: KspecialRegisters,       // ProcessorState.SpecialRegisters @ PRCB_PROCESSOR_STATE
    context: [u8; CONTEXT_SIZE],      // ProcessorState.ContextFrame
}

/// Offset of the (synthetic) `ApcState.Process` pointer inside our KTHREAD,
/// published as `OffsetKThreadApcProcess` so the engine maps the current thread
/// to its `EPROCESS`. The rest of the KTHREAD is zero (readable, benign).
const KTHREAD_APC_PROCESS: usize = 0x98;
const KTHREAD_SIZE: usize = 0x300;

#[repr(C)]
struct Kthread {
    bytes: [u8; KTHREAD_SIZE],
}

/// x64 `_KPCR`: `GdtBase`@0, `Self`@0x18, `CurrentPrcb`@0x20, `IdtBase`@0x38, and
/// the embedded `KPRCB` at 0x180. WinDbg resolves segment descriptors (the CS
/// lookup that yields the flat program counter) via `KPCR.GdtBase`, finding the
/// KPCR as `KiProcessorBlock[n] - OffsetPcrContainedPrcb` - so the PRCB must sit
/// inside a KPCR whose `GdtBase` points at the real GDT.
const KPCR_PRCB_OFFSET: usize = 0x180;

#[repr(C)]
struct Kpcr {
    gdt_base: u64,     // 0x00
    tss_base: u64,     // 0x08
    user_rsp: u64,     // 0x10
    self_ptr: u64,     // 0x18  Self
    current_prcb: u64, // 0x20  CurrentPrcb
    _pad0: [u8; 0x38 - 0x28],
    idt_base: u64,     // 0x38
    _pad1: [u8; KPCR_PRCB_OFFSET - 0x40],
    prcb: Kprcb,       // 0x180
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

/// `PsInitialSystemProcess` / `PsIdleProcess` — `PEPROCESS` pointers the engine
/// reads to anchor the System/Idle process. `!process` dereferences
/// `PsInitialSystemProcess` before walking the list; unresolved it reads as 0
/// ("Error reading _EPROCESS at 0"), so point both at our first process.
#[no_mangle]
pub static mut PsInitialSystemProcess: u64 = 0;
#[no_mangle]
pub static mut PsIdleProcess: u64 = 0;

/// Object-manager plumbing so `!process` accepts our processes as real process
/// objects. It reads the `_OBJECT_HEADER` before an `_EPROCESS`, decodes
///   index = TypeIndex ^ ((&header >> 8) & 0xff) ^ ObHeaderCookie
/// indexes `nt!ObTypeIndexTable[index]` and compares that `_OBJECT_TYPE*` with
/// `nt!PsProcessType`; a mismatch prints "TYPE mismatch for process object".
/// We put a single process `_OBJECT_TYPE` (`PsProcessTypeObject`) in the table at
/// `PROCESS_TYPE_INDEX`, point `PsProcessType` at it, keep the cookie 0, and set
/// each header's `TypeIndex` so the decode lands on that slot.
const PROCESS_TYPE_INDEX: u8 = 3;
#[no_mangle]
pub static mut ObHeaderCookie: u8 = 0;
/// `nt!ObTypeIndexTable[PROCESS_TYPE_INDEX]` points at the process `_OBJECT_TYPE`.
/// `!process` decodes the object header's `TypeIndex` to an index and checks the
/// type at that slot; it also cross-checks the decoded index against the type's
/// own `Index` field, so all three (the header decode, this slot, and
/// `PsProcessTypeObject.Index`) must agree on PROCESS_TYPE_INDEX.
#[no_mangle]
static mut ObTypeIndexTable: [u64; 16] = [0; 16];
/// The process `_OBJECT_TYPE`. `Name` (@0x10) and `Index` (@0x28) are filled in
/// [`commit`]; the rest is zero (benign - `!process` only reads those two).
#[no_mangle]
static mut PsProcessTypeObject: [u8; 0x100] = [0; 0x100];
/// UTF-16 `"Process"` for `PsProcessTypeObject.Name`.
static mut PsProcessTypeName: [u16; 8] = [b'P' as u16, b'r' as u16, b'o' as u16, b'c' as u16,
    b'e' as u16, b's' as u16, b's' as u16, 0];
#[no_mangle]
pub static mut PsProcessType: u64 = 0;

/// `nt!MmUserProbeAddress` - the user/kernel VA boundary (highest user address +
/// 1). `!process 0 0` reads this to decide whether its argument is a PID/`0`
/// (walk `PsActiveProcessHead`) or a literal `_EPROCESS` address; if it reads 0,
/// `0 < 0` is false and it tries to read an `_EPROCESS` at address 0 ("Error in
/// reading nt!_EPROCESS at 0"). The engine dereferences the symbol (and the KDBG
/// `MmUserProbeAddress` field points here), so it must hold the real value.
#[no_mangle]
pub static mut MmUserProbeAddress: u64 = 0x0000_7FFF_FFFF_0000;
/// Offset of `MmUserProbeAddress` in `_KDDEBUGGER_DATA64` (holds &MmUserProbeAddress).
const KDBG_OFF_MM_USER_PROBE: usize = 0x1d8;

/// Offsets into `_OBJECT_TYPE` (amd64). `TypeList` (@0) links all object types;
/// `!process <pid>` / object commands walk it from `nt!ObpTypeObjectType`.
const OBJECT_TYPE_TYPELIST_OFFSET: usize = 0x00;
const OBJECT_TYPE_NAME_OFFSET: usize = 0x10;
const OBJECT_TYPE_INDEX_OFFSET: usize = 0x28;

/// `nt!ObpTypeObjectType` - the "Type" object type (the type-of-types). To resolve
/// the process type by name (e.g. `!process 4 0`, object commands), the engine
/// reads `*ObpTypeObjectType` and walks the `_OBJECT_TYPE.TypeList` ring comparing
/// each type's `Name`; we link the process type in so "Process" is found.
#[no_mangle]
static mut ObpTypeObjectTypeObject: [u8; 0x100] = [0; 0x100];
/// UTF-16 `"Type"` for `ObpTypeObjectTypeObject.Name`.
static mut ObpTypeTypeName: [u16; 5] =
    [b'T' as u16, b'y' as u16, b'p' as u16, b'e' as u16, 0];
/// `POBJECT_TYPE` -> the "Type" object type; the engine dereferences this.
#[no_mangle]
pub static mut ObpTypeObjectType: u64 = 0;
const TYPE_OBJECT_TYPE_INDEX: u8 = 2;
/// Offset of `ObpTypeObjectType` in `_KDDEBUGGER_DATA64` (holds &ObpTypeObjectType).
const KDBG_OFF_OBP_TYPE_OBJECT_TYPE: usize = 0xa0;

/// Fill an `_OBJECT_TYPE` blob's `Name` (@0x10) + `Index` (@0x28).
unsafe fn fill_object_type(ty: &mut [u8], name_va: u64, name_chars: u16, index: u8) {
    let bytes = name_chars * 2;
    ty[OBJECT_TYPE_NAME_OFFSET..OBJECT_TYPE_NAME_OFFSET + 2].copy_from_slice(&bytes.to_le_bytes());
    ty[OBJECT_TYPE_NAME_OFFSET + 2..OBJECT_TYPE_NAME_OFFSET + 4]
        .copy_from_slice(&(bytes + 2).to_le_bytes());
    ty[OBJECT_TYPE_NAME_OFFSET + 8..OBJECT_TYPE_NAME_OFFSET + 16]
        .copy_from_slice(&name_va.to_le_bytes());
    ty[OBJECT_TYPE_INDEX_OFFSET] = index;
}

const MAX_MODULES: usize = 16;
const MAX_PROCS: usize = 16;

#[no_mangle]
static mut KdModuleEntries: [KldrEntry; MAX_MODULES] = [KldrEntry::zero(); MAX_MODULES];
#[no_mangle]
static mut KdProcessEntries: [ProcessObject; MAX_PROCS] = [ProcessObject::zero(); MAX_PROCS];

/// Synthetic processor 0 state (`KPROCESSOR_STATE`) the debugger reads for
/// register/CS-descriptor context, plus the one-entry `KiProcessorBlock` array
/// that points at it. Filled on the crash path by [`set_processor_state`].
#[no_mangle]
static mut KdKpcr: Kpcr = Kpcr {
    gdt_base: 0,
    tss_base: 0,
    user_rsp: 0,
    self_ptr: 0,
    current_prcb: 0,
    _pad0: [0; 0x38 - 0x28],
    idt_base: 0,
    _pad1: [0; KPCR_PRCB_OFFSET - 0x40],
    prcb: Kprcb {
        head: [0; PRCB_PROCESSOR_STATE],
        special: KspecialRegisters::zero(),
        context: [0; CONTEXT_SIZE],
    },
};
#[no_mangle]
static mut KiProcessorBlock: [u64; 1] = [0];
/// Synthetic current thread for processor 0 (readable KTHREAD whose
/// `ApcState.Process` points at the first process), so `!process`/`!thread` can
/// establish the current context instead of dereferencing a bogus pointer.
#[no_mangle]
static mut KdThread0: Kthread = Kthread { bytes: [0; KTHREAD_SIZE] };

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
        *e = ProcessObject::zero();
        e.body.dispatcher_type = PROCESS_OBJECT_TYPE;
        e.body.unique_process_id = pid;
        e.body.directory_table_base = cr3;
        e.body.peb = peb;
        let n = name.len().min(IMAGE_NAME_LEN - 1);
        for (j, &b) in name.iter().take(n).enumerate() {
            e.body.image_file_name[j] = b;
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

        // ---- Object-manager type wiring ----
        // A single process `_OBJECT_TYPE`, published in the type-index table at
        // PROCESS_TYPE_INDEX and pointed to by PsProcessType, is what every
        // process object's header must decode to for `!process` to accept it.
        let proc_type = addr(&raw const PsProcessTypeObject);
        PsProcessType = proc_type;
        ObTypeIndexTable[PROCESS_TYPE_INDEX as usize] = proc_type;
        // Fill the process + "Type" `_OBJECT_TYPE`s (Name cross-checked against the
        // object header's decoded index; the engine also resolves types by name).
        let type_type = addr(&raw const ObpTypeObjectTypeObject);
        ObpTypeObjectType = type_type;
        fill_object_type(
            &mut *(&raw mut PsProcessTypeObject),
            addr(PsProcessTypeName.as_ptr()),
            7, // "Process"
            PROCESS_TYPE_INDEX,
        );
        fill_object_type(
            &mut *(&raw mut ObpTypeObjectTypeObject),
            addr(ObpTypeTypeName.as_ptr()),
            4, // "Type"
            TYPE_OBJECT_TYPE_INDEX,
        );
        // Link the two `_OBJECT_TYPE`s into a `TypeList` ring (TypeList @ offset 0),
        // so walking from `ObpTypeObjectType` reaches the process type by name.
        let tl = OBJECT_TYPE_TYPELIST_OFFSET;
        {
            let t = &mut *(&raw mut ObpTypeObjectTypeObject);
            t[tl..tl + 8].copy_from_slice(&proc_type.to_le_bytes()); // Flink -> Process
            t[tl + 8..tl + 16].copy_from_slice(&proc_type.to_le_bytes()); // Blink
        }
        {
            let p = &mut *(&raw mut PsProcessTypeObject);
            p[tl..tl + 8].copy_from_slice(&type_type.to_le_bytes()); // Flink -> Type
            p[tl + 8..tl + 16].copy_from_slice(&type_type.to_le_bytes()); // Blink
        }
        let cookie = ObHeaderCookie;

        // ---- Active-process ring (via ActiveProcessLinks) ----
        // Links, PsInitialSystemProcess, and the addresses `!process` reports all
        // reference the `_EPROCESS` body; the `_OBJECT_HEADER` sits before it.
        let nprocs = N_PROCS;
        let phead = addr(&raw const PsActiveProcessHead);
        for i in 0..nprocs {
            let next = if i + 1 < nprocs {
                addr(&raw const (*(&raw const KdProcessEntries))[i + 1].body.active_process_links)
            } else {
                phead
            };
            let prev = if i == 0 {
                phead
            } else {
                addr(&raw const (*(&raw const KdProcessEntries))[i - 1].body.active_process_links)
            };
            let e = &mut (*(&raw mut KdProcessEntries))[i];
            e.body.active_process_links = ListEntry { flink: next, blink: prev };
            // Make the header decode to PROCESS_TYPE_INDEX:
            //   index = TypeIndex ^ ((&header >> 8) & 0xff) ^ ObHeaderCookie
            let hdr_va = addr(&raw const (*(&raw const KdProcessEntries))[i].object_header);
            e.object_header.pointer_count = 1;
            e.object_header.handle_count = 1;
            e.object_header.type_index =
                PROCESS_TYPE_INDEX ^ ((hdr_va >> 8) as u8) ^ cookie;
        }
        if nprocs == 0 {
            PsActiveProcessHead = ListEntry { flink: phead, blink: phead };
        } else {
            let first =
                addr(&raw const (*(&raw const KdProcessEntries))[0].body.active_process_links);
            let last = addr(
                &raw const (*(&raw const KdProcessEntries))[nprocs - 1].body.active_process_links,
            );
            PsActiveProcessHead = ListEntry { flink: first, blink: last };
            // Anchor the System/Idle process at our first EPROCESS so `!process`
            // (which reads PsInitialSystemProcess up front) doesn't hit a null.
            let p0 = addr(&raw const (*(&raw const KdProcessEntries))[0].body);
            PsInitialSystemProcess = p0;
            PsIdleProcess = p0;
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
        // KDBG.MmUserProbeAddress holds the *address* of the nt!MmUserProbeAddress
        // variable; `!process 0 0` dereferences it for the user/kernel boundary.
        put_rest_u64(d, KDBG_OFF_MM_USER_PROBE, addr(&raw const MmUserProbeAddress));
        // KDBG.ObpTypeObjectType holds the *address* of the nt!ObpTypeObjectType
        // pointer; object commands deref it to reach the "Type" object type.
        put_rest_u64(d, KDBG_OFF_OBP_TYPE_OBJECT_TYPE, addr(&raw const ObpTypeObjectType));
    }
}

/// Write a value into the zero-initialized `_rest` tail of `KdDebuggerDataBlock`
/// at its absolute field offset (the tail begins at 0x58).
fn put_rest_u64(d: &mut KddebuggerData64, abs_off: usize, v: u64) {
    d._rest[abs_off - 0x58..abs_off - 0x58 + 8].copy_from_slice(&v.to_le_bytes());
}
fn put_rest_u16(d: &mut KddebuggerData64, abs_off: usize, v: u16) {
    d._rest[abs_off - 0x58..abs_off - 0x58 + 2].copy_from_slice(&v.to_le_bytes());
}

/// Publish processor 0's `KPROCESSOR_STATE` so a Windows debugger can establish
/// machine context (`GetContextState`) and resolve the CS descriptor. Fills the
/// synthetic PRCB with the captured special registers + `CONTEXT`, points
/// `KiProcessorBlock[0]` at it, and writes the `KdDebuggerDataBlock` tail fields
/// the engine uses to find the state (`KiProcessorBlock` @0x218, `SizePrcb`
/// @0x2b0, `OffsetPrcbProcStateContext` @0x2bc, `OffsetPrcbProcStateSpecialReg`
/// @0x2ec). Call on the crash path after [`commit`] and before the memory
/// snapshot, so the wired block and PRCB land in the dump. `context` is a full
/// amd64 `CONTEXT` (the same bytes written to the dump header).
#[allow(clippy::too_many_arguments)]
pub fn set_processor_state(
    cr0: u64,
    cr2: u64,
    cr3: u64,
    cr4: u64,
    cr8: u64,
    gdt_base: u64,
    gdt_limit: u16,
    idt_base: u64,
    idt_limit: u16,
    tr: u16,
    ldtr: u16,
    context: &[u8; CONTEXT_SIZE],
) {
    let _g = KD_LOCK.lock();
    unsafe {
        let kpcr_va = addr(&raw const KdKpcr);
        let prcb_va = addr(&raw const KdKpcr.prcb);
        let thread_va = addr(&raw const KdThread0);
        let proc0 = addr(&raw const (*(&raw const KdProcessEntries))[0].body);

        // KPCR: GdtBase/IdtBase are what the engine reads to resolve segment
        // descriptors (the CS lookup that yields the flat PC); Self/CurrentPrcb
        // tie the KPCR to its embedded PRCB.
        let kpcr = &mut *(&raw mut KdKpcr);
        kpcr.gdt_base = gdt_base;
        kpcr.idt_base = idt_base;
        kpcr.self_ptr = kpcr_va;
        kpcr.current_prcb = prcb_va;

        let prcb = &mut kpcr.prcb;
        prcb.special = KspecialRegisters {
            cr0,
            cr2,
            cr3,
            cr4,
            kernel_dr: [0; 6],
            gdtr: Kdescriptor { pad: [0; 3], limit: gdt_limit, base: gdt_base },
            idtr: Kdescriptor { pad: [0; 3], limit: idt_limit, base: idt_base },
            tr,
            ldtr,
            mxcsr: 0x1f80,
            debug_control: 0,
            last_branch_to: 0,
            last_branch_from: 0,
            last_exception_to: 0,
            last_exception_from: 0,
            cr8,
            // MsrGsBase = the current (kernel) GS base = the KPCR. The engine
            // locates the running processor's KPCR (hence GdtBase, for the CS
            // descriptor) via this MSR, not via KiProcessorBlock. GsSwap holds
            // the user base (none here).
            msr: [kpcr_va, 0, 0, 0, 0, 0],
        };
        prcb.context.copy_from_slice(context);
        // Current thread: point the PRCB at a readable KTHREAD whose
        // ApcState.Process is the first process, so the engine can establish the
        // current context instead of reading a control register as a pointer.
        prcb.head[PRCB_CURRENT_THREAD..PRCB_CURRENT_THREAD + 8].copy_from_slice(&thread_va.to_le_bytes());

        let thread = &mut *(&raw mut KdThread0);
        thread.bytes[KTHREAD_APC_PROCESS..KTHREAD_APC_PROCESS + 8]
            .copy_from_slice(&proc0.to_le_bytes());

        (*(&raw mut KiProcessorBlock))[0] = prcb_va;

        let d = &mut *(&raw mut KdDebuggerDataBlock);
        // Processor block: where the state lives and how big the PRCB is.
        put_rest_u64(d, 0x218, addr(&raw const KiProcessorBlock)); // KiProcessorBlock
        put_rest_u16(d, 0x2b0, core::mem::size_of::<Kprcb>() as u16); // SizePrcb
        put_rest_u16(d, 0x2b4, PRCB_CURRENT_THREAD as u16); // OffsetPrcbCurrentThread
        put_rest_u16(d, 0x2bc, (PRCB_PROCESSOR_STATE + core::mem::size_of::<KspecialRegisters>()) as u16); // OffsetPrcbProcStateContext
        put_rest_u16(d, 0x2f2, PRCB_PROCESSOR_STATE as u16); // OffsetPrcbProcStateSpecialReg
        // KPCR shape so the engine locates the GDT/IDT for descriptor lookups
        // (KPCR = KiProcessorBlock[n] - OffsetPcrContainedPrcb). NOTE: an 8-byte
        // alignment pad after SizeEThread (0x2c0) shifts every field from here on
        // by +6 vs a naively packed layout - these are the true offsets.
        put_rest_u16(d, 0x2e0, core::mem::size_of::<Kpcr>() as u16); // SizePcr
        put_rest_u16(d, 0x2e2, 0x18); // OffsetPcrSelfPcr
        put_rest_u16(d, 0x2e4, 0x20); // OffsetPcrCurrentPrcb
        put_rest_u16(d, 0x2e6, KPCR_PRCB_OFFSET as u16); // OffsetPcrContainedPrcb
        // Thread/process shape so !process can walk from the current thread and
        // decode each EPROCESS (matches our compact _EPROCESS layout).
        put_rest_u16(d, 0x2a0, KTHREAD_APC_PROCESS as u16); // OffsetKThreadApcProcess
        put_rest_u16(d, 0x2a8, core::mem::size_of::<Eprocess>() as u16); // SizeEProcess
        put_rest_u16(d, 0x2aa, 0x20); // OffsetEprocessPeb
        put_rest_u16(d, 0x2ae, 0x18); // OffsetEprocessDirectoryTableBase
    }
}
