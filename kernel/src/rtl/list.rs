//! `LIST_ENTRY` — the intrusive circular doubly-linked list.
//!
//! This is *the* load-bearing data structure of the NT kernel: threads sit
//! on ready queues, DPCs on per-processor queues, wait blocks on dispatcher
//! object wait lists, IRPs on device queues — all through embedded
//! `LIST_ENTRY` fields, never through heap-allocated nodes.
//!
//! ## Shape
//!
//! A list head and every entry are the same two-pointer structure; an empty
//! list is a head whose `flink`/`blink` point at itself:
//!
//! ```text
//!        head                 head ──flink──▶ A ──flink──▶ B ─┐
//!       ┌─────┐                ▲                              │
//!  ┌───▶│ f,b │───┐            └────────────blink─────────────┘   (circular)
//!  └────┴─────┴◀──┘
//! ```
//!
//! ## Safety model
//!
//! Intrusive lists are inherently about raw aliasing, so the API is `unsafe`
//! and the invariants are stated explicitly (callers are the kernel's own
//! subsystems, which uphold them under the appropriate lock — exactly the
//! discipline the C kernel uses, but here the contracts are written down):
//!
//! 1. A `ListEntry` must be pinned in memory while linked (entries live
//!    inside pool allocations or static structures; they never move).
//! 2. Linking/unlinking on a given list must be externally synchronized
//!    (spinlock or IRQL), as in NT.
//! 3. `container_of!` recovers the owning structure from an embedded entry,
//!    like `CONTAINING_RECORD`.

use core::ptr;

/// Bit-compatible with the Windows `LIST_ENTRY` structure.
#[repr(C)]
#[derive(Debug)]
pub struct ListEntry {
    /// Forward link (`Flink`) — next entry toward the tail.
    pub flink: *mut ListEntry,
    /// Backward link (`Blink`) — previous entry toward the head.
    pub blink: *mut ListEntry,
}

// ListEntry is only ever manipulated under a lock by the owning subsystem;
// the raw pointers themselves are safe to send across CPUs.
unsafe impl Send for ListEntry {}
unsafe impl Sync for ListEntry {}

impl ListEntry {
    /// A not-yet-initialized entry. Call [`init`](Self::init) (or link it)
    /// before use; the null pointers make accidental use loud.
    pub const fn new() -> Self {
        ListEntry {
            flink: ptr::null_mut(),
            blink: ptr::null_mut(),
        }
    }

    /// `InitializeListHead` — make this entry an empty list head
    /// (both links pointing at itself).
    ///
    /// # Safety
    /// `self` must be pinned at a stable address from now until the list is
    /// no longer used (invariant 1 above).
    pub unsafe fn init(&mut self) {
        let me = self as *mut ListEntry;
        self.flink = me;
        self.blink = me;
    }

    /// `IsListEmpty` — true when the head points at itself.
    ///
    /// # Safety
    /// `self` must have been initialized with [`init`](Self::init).
    pub unsafe fn is_empty(&self) -> bool {
        self.flink as *const ListEntry == self as *const ListEntry
    }

    /// `InsertTailList` — link `entry` immediately before the head, i.e.
    /// at the tail of the list. FIFO producers use this.
    ///
    /// # Safety
    /// Head initialized, `entry` pinned and not currently linked anywhere,
    /// external synchronization held (invariants 1–2).
    pub unsafe fn insert_tail(&mut self, entry: *mut ListEntry) {
        unsafe {
            let head = self as *mut ListEntry;
            let old_tail = self.blink;
            (*entry).flink = head;
            (*entry).blink = old_tail;
            (*old_tail).flink = entry;
            self.blink = entry;
        }
    }

    /// `InsertHeadList` — link `entry` immediately after the head. LIFO /
    /// priority-boost producers use this.
    ///
    /// # Safety
    /// Same contract as [`insert_tail`](Self::insert_tail).
    pub unsafe fn insert_head(&mut self, entry: *mut ListEntry) {
        unsafe {
            let head = self as *mut ListEntry;
            let old_first = self.flink;
            (*entry).flink = old_first;
            (*entry).blink = head;
            (*old_first).blink = entry;
            self.flink = entry;
        }
    }

    /// `RemoveHeadList` — unlink and return the first entry, or `None` if
    /// the list is empty (the C macro returns the head itself in that case;
    /// an `Option` is strictly safer and costs nothing).
    ///
    /// # Safety
    /// Head initialized, external synchronization held.
    pub unsafe fn remove_head(&mut self) -> Option<*mut ListEntry> {
        unsafe {
            if self.is_empty() {
                return None;
            }
            let entry = self.flink;
            ListEntry::remove(entry);
            Some(entry)
        }
    }

    /// `RemoveEntryList` — unlink `entry` from whatever list it is on.
    /// The entry's own links are poisoned to null afterwards so a double
    /// removal faults immediately instead of corrupting a list.
    ///
    /// # Safety
    /// `entry` must currently be linked on a list whose synchronization the
    /// caller holds.
    pub unsafe fn remove(entry: *mut ListEntry) {
        unsafe {
            let flink = (*entry).flink;
            let blink = (*entry).blink;
            (*blink).flink = flink;
            (*flink).blink = blink;
            // Poison (NT's checked builds do the same with 0xBAADF00D-style
            // values) so use-after-unlink is a null deref, not silent damage.
            (*entry).flink = ptr::null_mut();
            (*entry).blink = ptr::null_mut();
        }
    }

    /// Iterate the list calling `f` on every entry (head excluded). The
    /// callback must not unlink entries; use `remove_head` loops for that.
    ///
    /// # Safety
    /// Head initialized, external synchronization held for the whole walk.
    pub unsafe fn for_each(&self, mut f: impl FnMut(*mut ListEntry)) {
        unsafe {
            let head = self as *const ListEntry as *mut ListEntry;
            let mut cur = self.flink;
            while cur != head {
                let next = (*cur).flink; // read before f in case f relinks
                f(cur);
                cur = next;
            }
        }
    }
}

/// `CONTAINING_RECORD` — recover a pointer to the structure that embeds
/// `$field` from a pointer to that field.
///
/// ```ignore
/// let thread: *mut Kthread = container_of!(entry, Kthread, wait_list_entry);
/// ```
///
/// Safety: `$ptr` must really point at the `$field` member of a live
/// `$ty`; the macro is mechanical pointer arithmetic, identical to the
/// C macro.
#[macro_export]
macro_rules! container_of {
    ($ptr:expr, $ty:ty, $field:ident) => {{
        let __p = $ptr as *mut $crate::rtl::list::ListEntry;
        (__p as *mut u8).sub(core::mem::offset_of!($ty, $field)) as *mut $ty
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy "DPC-like" record with an embedded list entry, exercising the
    /// container_of pattern exactly the way ke/io use it.
    #[repr(C)]
    struct Record {
        value: u32,
        entry: ListEntry,
    }

    #[test]
    fn fifo_insert_tail_remove_head() {
        unsafe {
            let mut head = ListEntry::new();
            head.init();
            assert!(head.is_empty());

            let mut a = Record { value: 1, entry: ListEntry::new() };
            let mut b = Record { value: 2, entry: ListEntry::new() };
            let mut c = Record { value: 3, entry: ListEntry::new() };
            head.insert_tail(&mut a.entry);
            head.insert_tail(&mut b.entry);
            head.insert_tail(&mut c.entry);
            assert!(!head.is_empty());

            // FIFO order out.
            for expected in [1u32, 2, 3] {
                let e = head.remove_head().unwrap();
                let r = container_of!(e, Record, entry);
                assert_eq!((*r).value, expected);
            }
            assert!(head.is_empty());
            assert!(head.remove_head().is_none());
        }
    }

    #[test]
    fn lifo_insert_head_and_middle_removal() {
        unsafe {
            let mut head = ListEntry::new();
            head.init();

            let mut a = Record { value: 1, entry: ListEntry::new() };
            let mut b = Record { value: 2, entry: ListEntry::new() };
            let mut c = Record { value: 3, entry: ListEntry::new() };
            head.insert_head(&mut a.entry);
            head.insert_head(&mut b.entry);
            head.insert_head(&mut c.entry); // order is now c, b, a

            // Remove the middle element directly, as a wait-satisfy would.
            ListEntry::remove(&mut b.entry);
            assert!(b.entry.flink.is_null()); // poisoned after unlink

            let mut seen = alloc::vec::Vec::new();
            head.for_each(|e| {
                let r = container_of!(e, Record, entry);
                seen.push((*r).value);
            });
            assert_eq!(seen, [3, 1]);
        }
    }
}
