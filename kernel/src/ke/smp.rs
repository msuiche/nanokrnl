//! SMP — multiprocessor startup (`KeStartAllProcessors` in miniature).
//!
//! The BSP discovers the machine's processors from the MADT (see
//! [`crate::hal::acpi`]) and starts each application processor with the
//! classic INIT-SIPI-SIPI handshake: the AP wakes in 16-bit real mode at
//! the SIPI page frame, runs a tiny trampoline ([`TRAMPOLINE_PHYS`]) that
//! climbs through protected mode into long mode on a private transition
//! page table, lands in Rust, loads its own GDT/TSS/IDT/KPCR and local
//! APIC, and parks in a `cli; hlt` loop.
//!
//! This phase deliberately stops there: the scheduler, the clock, and user
//! mode remain BSP-only; the APs are online, addressable (IPIs), and hold
//! no kernel state. Per-CPU scheduling, the AP timer, and TLB shootdown
//! IPIs build on this.
//!
//! ## The transition page table
//!
//! The trampoline executes at its physical address, which the kernel's own
//! PML4 does not identity-map — so the `mov cr3` into the real tables must
//! happen from a *transition* PML4 that maps both: entry 0 identity-maps
//! the first 2 MiB (the trampoline), entries 256..512 are copies of the
//! kernel PML4 (the whole kernel high half, including the physical-memory
//! window every AP stack lives in). The AP switches to the real CR3 as its
//! first Rust-order business.

use super::{gdt, idt, pcr};
use super::traps::VECTOR_TLB_SHOOTDOWN;
use crate::hal::{acpi, apic};
use crate::mm::phys::{mm_allocate_contiguous_pages, TRAMPOLINE_PAGE};
use crate::mm::{phys_to_virt, PhysAddr, PAGE_SIZE};
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Physical address the trampoline is copied to; SIPI vector 0x08.
pub const TRAMPOLINE_PHYS: u64 = TRAMPOLINE_PAGE;
/// Byte offsets of the fields the BSP patches into the trampoline page.
const PATCH_CR3: usize = 0xF00;
const PATCH_STACK: usize = 0xF08;
const PATCH_ENTRY: usize = 0xF10;
/// Kernel stack per AP.
const AP_STACK_PAGES: usize = 16; // 64 KiB

/// Maximum processors tracked (matches the per-CPU table arrays).
pub const MAX_CPUS: usize = gdt::MAX_CPUS;

/// Per-processor startup record — BSP-published, AP-updated.
#[derive(Default)]
pub struct CpuSlot {
    /// The processor's local APIC ID (0 = slot empty).
    pub apic_id: AtomicU64,
    /// Set by the AP once its own GDT/IDT/KPCR/LAPIC are up.
    pub online: AtomicBool,
    /// The processor number this CPU read back *through its own GS*
    /// (proof the per-CPU KPCR is wired), stored +1 so 0 means "not seen".
    pub pcr_seen: AtomicU64,
    /// Physical base of the AP's kernel stack (diagnostics).
    pub stack_base: AtomicU64,
}

static CPU_SLOTS: [CpuSlot; MAX_CPUS] = [const { CpuSlot {
    apic_id: AtomicU64::new(0),
    online: AtomicBool::new(false),
    pcr_seen: AtomicU64::new(0),
    stack_base: AtomicU64::new(0),
} }; MAX_CPUS];

/// Processors listed by the MADT (1 when ACPI is absent — nanox).
static PROCESSOR_COUNT: AtomicUsize = AtomicUsize::new(1);
/// Processors that completed startup (BSP inclusive).
static ONLINE_COUNT: AtomicUsize = AtomicUsize::new(1);

/// The BSP's CR4, captured before AP startup so each AP can adopt the same
/// feature set (SMEP/SMAP/PGE/…).
/// The boot processor's CR4, captured before the first SIPI so each AP can
/// adopt the same feature set (SMEP/SMAP/…).
static BSP_CR4: AtomicU64 = AtomicU64::new(0);
/// The boot processor's CR0, captured likewise — the WP bit in particular
/// must hold on every CPU or read-only pages are writable on APs.
static BSP_CR0: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// TLB shootdown
// ---------------------------------------------------------------------------
//
// With threads of one address space able to run on several CPUs, an unmap
// or protection change must invalidate that page in *every* CPU's TLB, not
// just the local one (`KiIpiSendPacket`/`KeFlushTbSingle` in NT). Each CPU
// records the CR3 it is currently running; a shooter mails the VA to every
// online CPU running that CR3 and IPIs it; the target's handler (vector
// 0xE0, above the clock) does the `invlpg` and acks. The handler takes no
// locks, so a CPU can always ack mid-spin — cross-shootdowns cannot
// deadlock.

/// The address space each CPU is currently running (0 = slot empty).
static ACTIVE_CR3: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Shootdown mailbox, per CPU: the VA to invalidate…
static SHOOT_VA: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// …the request sequence number…
static SHOOT_SEQ: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// …and the acknowledged sequence number.
static SHOOT_ACK: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Record the CR3 this CPU is running (called from the context switch and
/// at CPU bring-up).
pub fn set_active_cr3(cr3: u64) {
    let cpu = pcr::ke_get_prcb().number as usize;
    if cpu < MAX_CPUS {
        ACTIVE_CR3[cpu].store(cr3, Ordering::Release);
    }
}

/// Serialize shooters: the per-CPU mailbox holds one VA, so two CPUs
/// shooting the same target concurrently would clobber it. NT serializes
/// its IPI packet similarly; the wait is short and targets never take
/// this lock, so spinning here cannot deadlock.
static SHOOT_LOCK: crate::ke::spinlock::SpinLock<()> = crate::ke::spinlock::SpinLock::new(());

/// Invalidate `va` in the TLB of every *other* online CPU currently running
/// address space `cr3`. The caller handles its own TLB. Returns how many
/// remote CPUs acked (self-test/diagnostic surface).
pub fn tlb_shootdown(cr3: u64, va: u64) -> usize {
    let me = pcr::ke_get_prcb().number as usize;
    let _serialize = SHOOT_LOCK.lock();
    let mut acked = 0;
    for cpu in 0..MAX_CPUS {
        if cpu == me || !CPU_SLOTS[cpu].online.load(Ordering::Acquire) {
            continue;
        }
        if ACTIVE_CR3[cpu].load(Ordering::Acquire) != cr3 {
            continue;
        }
        SHOOT_VA[cpu].store(va, Ordering::Relaxed);
        let seq = SHOOT_SEQ[cpu].fetch_add(1, Ordering::AcqRel) + 1;
        apic::send_ipi(CPU_SLOTS[cpu].apic_id.load(Ordering::Acquire) as u8, 0, VECTOR_TLB_SHOOTDOWN);
        // Bounded wait: a stuck CPU must not wedge the shooter (the page
        // is freed regardless; a missed shootdown is a correctness bug we
        // surface via the return count rather than a hang).
        let start = rdtsc();
        while SHOOT_ACK[cpu].load(Ordering::Acquire) < seq {
            if rdtsc() - start > 2_000_000_000 {
                break;
            }
            core::hint::spin_loop();
        }
        if SHOOT_ACK[cpu].load(Ordering::Acquire) >= seq {
            acked += 1;
        }
    }
    acked
}

/// The target half of the shootdown, in the IPI (above CLOCK_LEVEL, so it
/// preempts any shooter spinning at DISPATCH or below). No locks, no
/// allocation — invalidate, ack, done.
pub fn on_tlb_shootdown() {
    let cpu = pcr::ke_get_prcb().number as usize;
    if cpu >= MAX_CPUS {
        return;
    }
    let va = SHOOT_VA[cpu].load(Ordering::Relaxed);
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) va, options(nostack));
    }
    SHOOT_ACK[cpu].store(SHOOT_SEQ[cpu].load(Ordering::Acquire), Ordering::Release);
}

extern "C" {
    static ap_tramp_start: u8;
    static ap_tramp_end: u8;
}

// The AP startup trampoline: real mode -> protected -> long mode.
//
// Runs at its physical address (0x8000) with all internal references
// absolute literals against that base. The layout is pinned with `.org`
// (entry 0x000, 32-bit entry 0x100, 64-bit entry 0x180, GDTR 0x200, GDT
// 0x208) because the assembler won't fold label arithmetic inside memory
// operands; the two far jumps are hand-encoded (`.byte 0xEA`) for the same
// reason. The patch fields at 0xF00/0xF08 are written by the BSP before
// each SIPI.
core::arch::global_asm!(
    r#"
.section .text.aptramp, "ax"
.globl ap_tramp_start, ap_tramp_end
ap_tramp_start:
.code16
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7000
    lgdt [0x8000 + 0x200]            # GDTR (below)
    mov eax, cr0
    or  eax, 1                       # CR0.PE
    mov cr0, eax
    .byte 0x66, 0xEA                 # ljmp ptr16:32 -> protected mode
    .long 0x8000 + 0x100
    .word 0x08

.org 0x100
.code32
tramp_pm:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x7000
    mov eax, [0x8000 + 0xF00]        # PATCH_CR3 (transition PML4)
    mov cr3, eax
    mov eax, cr4
    or  eax, 0x20                    # CR4.PAE
    mov cr4, eax
    mov ecx, 0xC0000080              # IA32_EFER
    rdmsr
    or  eax, 0x901                   # SCE | LME | NXE
    wrmsr
    mov eax, cr0
    or  eax, 0x80000000              # CR0.PG
    mov cr0, eax
    .byte 0xEA                       # ljmp ptr16:32 -> long mode
    .long 0x8000 + 0x180
    .word 0x18

.org 0x180
.code64
tramp_lm:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov rax, 0x8000 + 0xF08          # PATCH_STACK — via a base register:
    mov rsp, [rax]                   # [disp32] would be RIP-relative
    mov rax, 0x8000 + 0xF10          # PATCH_ENTRY (runtime VA of
    jmp [rax]                        # ap_rust_entry; the kernel is PIE,
                                     # so no absolute reloc exists)

.org 0x200
tramp_gdtr:
    .word 4 * 8 - 1
    .long 0x8000 + 0x208             # GDT (below)
.org 0x208
tramp_gdt:
    .quad 0x0000000000000000         # null
    .quad 0x00CF9A000000FFFF         # 0x08: flat 32-bit code
    .quad 0x00CF92000000FFFF         # 0x10: flat 32-bit data
    .quad 0x00AF9A000000FFFF         # 0x18: 64-bit code
ap_tramp_end:
"#
);

/// Read the time-stamp counter (for the SIPI handshake delays — the APIC
/// clock isn't a wall clock and interrupts are off here anyway).
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

/// Busy-wait ~`ms` milliseconds, assuming a >= 2 GHz TSC (QEMU's is;
/// a slower TSC just waits proportionally longer, which is safe here).
fn delay_ms(ms: u64) {
    let start = rdtsc();
    while rdtsc() - start < ms * 2_000_000 {
        core::hint::spin_loop();
    }
}

/// Processors the MADT lists (1 without ACPI).
pub fn processor_count() -> usize {
    PROCESSOR_COUNT.load(Ordering::Acquire)
}

/// Processors that completed startup, BSP inclusive.
pub fn online_count() -> usize {
    ONLINE_COUNT.load(Ordering::Acquire)
}

/// Startup record of processor `n` — self-test/diagnostic surface.
pub fn slot(n: usize) -> Option<(u64, bool, u64)> {
    let s = CPU_SLOTS.get(n)?;
    let id = s.apic_id.load(Ordering::Acquire);
    if id == 0 && n != 0 {
        return None;
    }
    Some((id, s.online.load(Ordering::Acquire), s.pcr_seen.load(Ordering::Acquire)))
}

// ---------------------------------------------------------------------------
// Idle-aware IPI targeting
// ---------------------------------------------------------------------------

/// Per-CPU "running the idle thread" flag — the CPUs worth nudging when new
/// work appears (a busy CPU finds it at its next dispatch point anyway).
static HALTING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Idle-targeted dispatch IPIs sent so far (self-test surface).
static IDLE_IPIS: AtomicU64 = AtomicU64::new(0);

/// Record whether `cpu` is parked on its idle thread (called from the
/// context switch with the chosen thread).
pub fn set_halting(cpu: usize, halting: bool) {
    if cpu < MAX_CPUS {
        HALTING[cpu].store(halting, Ordering::Relaxed);
    }
}

/// Send a fixed IPI to every *idle* processor except the caller — NT's
/// idle-processor nudge instead of a broadcast storm. Returns how many were
/// targeted.
pub fn ipi_idle_cpus(vector: u8) -> usize {
    let me = pcr::ke_get_prcb().number as usize;
    let mut sent = 0;
    for cpu in 0..MAX_CPUS {
        if cpu == me || !HALTING[cpu].load(Ordering::Relaxed) {
            continue;
        }
        if !CPU_SLOTS[cpu].online.load(Ordering::Acquire) {
            continue;
        }
        apic::send_ipi(CPU_SLOTS[cpu].apic_id.load(Ordering::Acquire) as u8, 0, vector);
        sent += 1;
    }
    IDLE_IPIS.fetch_add(sent as u64, Ordering::Relaxed);
    sent
}

/// Idle-targeted dispatch IPIs sent so far (self-test surface).
pub fn idle_ipis_sent() -> u64 {
    IDLE_IPIS.load(Ordering::Relaxed)
}

/// Whether `cpu` is parked on its idle thread.
pub fn is_halting(cpu: usize) -> bool {
    cpu < MAX_CPUS && HALTING[cpu].load(Ordering::Relaxed)
}

/// Build the transition page table set (PML4 + PDPT + PD): identity map of
/// the first 2 MiB plus a copy of the kernel high half. Returns the
/// transition PML4's physical address.
fn build_transition_tables() -> PhysAddr {
    let pml4 = mm_allocate_contiguous_pages(1).expect("transition PML4");
    let pdpt = mm_allocate_contiguous_pages(1).expect("transition PDPT");
    let pd = mm_allocate_contiguous_pages(1).expect("transition PD");
    const P: u64 = 1;
    const RW: u64 = 1 << 1;
    const PS: u64 = 1 << 7; // large page
    unsafe {
        let t = phys_to_virt(pml4) as *mut u64;
        *t = pdpt.0 | P | RW;
        let kern = phys_to_virt(crate::mm::virt::mm_kernel_address_space()) as *const u64;
        for i in 256..512 {
            *t.add(i) = *kern.add(i);
        }
        *(phys_to_virt(pdpt) as *mut u64) = pd.0 | P | RW;
        *(phys_to_virt(pd) as *mut u64) = 0 | P | RW | PS; // 0..2 MiB identity
    }
    pml4
}

/// Start every application processor the MADT lists. Runs on the BSP at the
/// end of phase 0 (interrupts off, LAPIC online, allocator up). With no
/// ACPI or a single CPU this is a no-op and the kernel stays uniprocessor.
pub fn init() {
    let Some(madt) = acpi::processors() else {
        crate::kd_println!("SMP: no ACPI tables — uniprocessor");
        return;
    };
    PROCESSOR_COUNT.store(madt.count.max(1), Ordering::Release);
    let bsp_id = apic::lapic_id();
    CPU_SLOTS[0].apic_id.store(bsp_id as u64, Ordering::Release);
    CPU_SLOTS[0].online.store(true, Ordering::Release);
    CPU_SLOTS[0].pcr_seen.store(1, Ordering::Release);
    // The BSP's boot context doubles as its idle thread.
    HALTING[0].store(true, Ordering::Release);
    ACTIVE_CR3[0].store(
        crate::mm::virt::mm_kernel_address_space().0,
        Ordering::Release,
    );
    if madt.count <= 1 {
        crate::kd_println!("SMP: uniprocessor (APIC id {})", bsp_id);
        return;
    }

    // Adopt the BSP's CR4 for every AP (SMEP/SMAP/…), build the transition
    // tables, and install the trampoline at its SIPI page.
    let cr4: u64;
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack)) };
    BSP_CR4.store(cr4, Ordering::Release);
    let cr0: u64;
    unsafe { asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack)) };
    BSP_CR0.store(cr0, Ordering::Release);
    let trans_cr3 = build_transition_tables();
    let blob = unsafe {
        core::slice::from_raw_parts(
            &raw const ap_tramp_start,
            &raw const ap_tramp_end as usize - &raw const ap_tramp_start as usize,
        )
    };
    assert!(blob.len() < PATCH_CR3, "trampoline overlaps its patch area");
    let page_va = phys_to_virt(PhysAddr(TRAMPOLINE_PHYS));
    unsafe {
        core::ptr::copy_nonoverlapping(blob.as_ptr(), page_va, blob.len());
        (page_va.add(PATCH_CR3) as *mut u64).write(trans_cr3.0);
        (page_va.add(PATCH_ENTRY) as *mut u64).write(ap_rust_entry as usize as u64);
    }

    // Start the APs one at a time: patch the stack field, INIT-SIPI-SIPI,
    // wait for the AP to report online before addressing the next.
    let mut next = 1usize;
    for i in 0..madt.count {
        let id = madt.cpus[i].apic_id;
        if id == bsp_id || next >= MAX_CPUS {
            continue;
        }
        let cpu = next;
        next += 1;
        let stack_pa = match mm_allocate_contiguous_pages(AP_STACK_PAGES) {
            Some(pa) => pa,
            None => {
                crate::kd_println!("SMP: no stack for AP (APIC id {}) — skipping", id);
                continue;
            }
        };
        let stack_top = phys_to_virt(stack_pa) as u64 + (AP_STACK_PAGES * PAGE_SIZE) as u64;
        let slot = &CPU_SLOTS[cpu];
        slot.apic_id.store(id as u64, Ordering::Release);
        slot.stack_base.store(stack_pa.0, Ordering::Release);
        slot.online.store(false, Ordering::Release);
        unsafe { (page_va.add(PATCH_STACK) as *mut u64).write(stack_top) };

        apic::send_ipi(id, 5, 0); // INIT
        delay_ms(10);
        apic::send_ipi(id, 6, (TRAMPOLINE_PHYS >> 12) as u8); // SIPI
        delay_ms(1);
        apic::send_ipi(id, 6, (TRAMPOLINE_PHYS >> 12) as u8); // SIPI (again)

        let start = rdtsc();
        while !slot.online.load(Ordering::Acquire) {
            if rdtsc() - start > 2_000_000_000 {
                crate::kd_println!("SMP: AP (APIC id {}) did not start — skipping", id);
                break;
            }
            core::hint::spin_loop();
        }
    }
    crate::kd_println!(
        "SMP: {} of {} processors online",
        online_count(),
        processor_count()
    );
}

/// The AP's first Rust code, entered from the trampoline on the transition
/// page tables. Brings up this CPU's own tables, adopts this context as its
/// idle thread, and joins the scheduler.
#[no_mangle]
pub extern "C" fn ap_rust_entry() -> ! {
    // Switch to the real kernel page tables (the transition PML4 only
    // existed to survive the mode climb).
    unsafe {
        asm!(
            "mov cr3, {}",
            in(reg) crate::mm::virt::mm_kernel_address_space().0,
            options(nostack),
        );
        // Adopt the BSP's CR4 feature set.
        asm!("mov cr4, {}", in(reg) BSP_CR4.load(Ordering::Acquire), options(nostack));
        // …and its CR0 (WP included: read-only pages are read-only to the
        // supervisor on every CPU, or the probe/recovery contract is
        // CPU-dependent). Paging is already on here, so the PG/PE bits the
        // snapshot carries are the ones we are already running with.
        asm!("mov cr0, {}", in(reg) BSP_CR0.load(Ordering::Acquire), options(nostack));
    }
    // Who am I? Match the LAPIC ID against the BSP-published slots.
    let id = apic::lapic_id() as u64;
    let cpu = (1..MAX_CPUS)
        .find(|&i| CPU_SLOTS[i].apic_id.load(Ordering::Acquire) == id)
        .unwrap_or_else(|| unsafe { crate::ke::bugcheck::ke_bug_check_ex(0x5C, id, 0, 0, 0) });
    let slot = &CPU_SLOTS[cpu];
    let stack_top = phys_to_virt(PhysAddr(slot.stack_base.load(Ordering::Acquire))) as u64
        + (AP_STACK_PAGES * PAGE_SIZE) as u64;

    gdt::init_ap(cpu, stack_top);
    idt::load();
    pcr::init_ap(cpu);
    apic::init_ap();
    // STAR/LSTAR/FMASK/SCE are per-CPU MSRs — without this a `syscall` on
    // this processor vectors to address 0.
    super::syscall::init();
    HALTING[cpu].store(true, Ordering::Release);
    ACTIVE_CR3[cpu].store(
        crate::mm::virt::mm_kernel_address_space().0,
        Ordering::Release,
    );

    // Prove the per-CPU KPCR works by reading our own number through GS.
    let seen = pcr::ke_get_prcb().number as u64 + 1;
    slot.pcr_seen.store(seen, Ordering::Release);
    crate::kd_println!("SMP: processor {} online (APIC id {})", cpu, id);

    // Adopt this very context as the processor's idle thread — the same
    // trick the BSP pulls in phase 1 (we are already running on the stack).
    // The global dispatcher state is initialized by the BSP; only the
    // per-CPU half is ours. From here on this CPU takes the clock,
    // preempts, and picks up ready threads off the global queues.
    let idle = crate::ex::ex_allocate_object(
        crate::ke::thread::Kthread::new(cpu as u64, 0, 0, 0),
        crate::mm::pool::pool_tag(b"Idle"),
    )
    .expect("AP idle thread allocation");
    unsafe { crate::ke::scheduler::ki_initialize_ap(idle) };
    slot.online.store(true, Ordering::Release);
    ONLINE_COUNT.fetch_add(1, Ordering::AcqRel);
    crate::ke::scheduler::ap_idle_loop();
}
