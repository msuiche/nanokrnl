// sehtest.c — frame-based SEH end-to-end test, through the real CRT handler.
//
// Exercises the full chain: kernel exception delivery -> the kernel32 shim's
// RtlLookupFunctionEntry + RtlVirtualUnwind + frame-handler dispatch -> the
// msvcrt shim's `__C_specific_handler` (the scope-table walk) -> __except
// blocks and a __finally funclet emitted by clang's WinEH.
//
// Every filter is a real function (opaque to the folder) and every flag is
// set inside the block it proves ran — so a green line means the machinery
// truly dispatched into that block, not just that the code exists.
//
// Built with clang -fasync-exceptions (required: without it clang treats
// hardware faults as outside the SEH model and emits no handler metadata).
// See scripts/build-sehtest.sh.

typedef unsigned long long u64;
typedef unsigned int u32;
typedef int i32;

#define EXCEPTION_EXECUTE_HANDLER 1
#define EXCEPTION_CONTINUE_SEARCH 0

// msvcrt shim import: the frame handler named in our .xdata.
__declspec(dllimport) int __C_specific_handler(void*, u64, void*, void*);

// msvcrt shim imports: setjmp/longjmp over the unwind machinery.
__declspec(dllimport) int __intrinsic_setjmp(u64*);
__declspec(dllimport) void __stdcall longjmp(u64*, int);

// kernel32 shim imports
__declspec(dllimport) u64 __stdcall GetStdHandle(u32);
__declspec(dllimport) i32 __stdcall WriteConsoleA(u64, const void*, u32, u32*, u64);
__declspec(dllimport) void __stdcall ExitProcess(u32);

// The SEH filter-context intrinsic (excpt.h, declared manually: freestanding).
unsigned long _exception_code(void);

static volatile u32 g_seen_code;
static volatile int g_finally_ran;
static volatile int g_inner_ran;
static volatile int g_rec[5];

// Real, non-foldable filters: one executes, one passes.
static int filt_exec(u32 code) {
    g_seen_code = code;
    return EXCEPTION_EXECUTE_HANDLER;
}
static int filt_pass(u32 code) {
    g_seen_code = code;
    return EXCEPTION_CONTINUE_SEARCH;
}

static void print(const char* s) {
    u32 n = 0;
    while (s[n]) n++;
    u32 wrote = 0;
    WriteConsoleA(GetStdHandle((u32)-11), s, n, &wrote, 0);
}

// Case 3's helper: faults with no handler metadata of its own (a leaf
// function — the dispatch must apply the leaf rule to reach the caller).
static __declspec(noinline) void fault_in_callee(void) {
    *(volatile int*)0x1234 = 42;
}

// Case 5's helpers: a function whose __finally must run when the longjmp
// unwinds through it, and the longjmp target buffer.
static volatile int g_finally_lj;
static __declspec(noinline) void fault_with_finally(void) {
    __try {
        *(volatile int*)0x1234 = 42;
    } __finally {
        g_finally_lj = 1;
    }
}
static u64 g_jb[10];
static volatile int g_lj_state;

void mainCRTStartup(void) {
    // Case 1: fault in __try, except block runs.
    __try {
        *(volatile int*)0x1234 = 42;
        g_rec[1] = -1; // unreached
    } __except (filt_exec(_exception_code())) {
        g_rec[1] = 1;
    }
    if (g_rec[1] == 1 && g_seen_code == 0xC0000005)
        print("SEH: __except block ran, filter saw the AV code\n");
    else
        print("SEH: FAIL case1\n");

    // Case 2: the __finally funclet must run as the unwind passes its frame.
    g_finally_ran = 0;
    __try {
        __try {
            *(volatile int*)0x1234 = 42;
        } __finally {
            g_finally_ran = 1;
        }
    } __except (filt_exec(_exception_code())) {
        g_rec[2] = 1;
    }
    if (g_rec[2] == 1 && g_finally_ran)
        print("SEH: __finally ran during the unwind\n");
    else
        print("SEH: FAIL case2\n");

    // Case 3: fault one leaf frame down, recovered by the caller's except.
    g_rec[3] = 0;
    __try {
        fault_in_callee();
    } __except (filt_exec(_exception_code())) {
        g_rec[3] = 1;
    }
    if (g_rec[3] == 1)
        print("SEH: caller's __except caught the callee fault (virtual unwind)\n");
    else
        print("SEH: FAIL case3\n");

    // Case 4: an inner filter passing must let the outer handler fire.
    g_rec[4] = 0;
    g_inner_ran = 0;
    __try {
        __try {
            *(volatile int*)0x1234 = 42;
        } __except (filt_pass(_exception_code())) {
            g_inner_ran = 1; // must NOT run
        }
    } __except (filt_exec(_exception_code())) {
        g_rec[4] = 1;
    }
    if (g_rec[4] == 1 && !g_inner_ran)
        print("SEH: nested filter passed, outer __except caught it\n");
    else
        print("SEH: FAIL case4\n");

    // Case 5: a fault's except block longjmps back to setjmp — the unwind
    // must run the intervening __finally and land at setjmp with the value.
    g_finally_lj = 0;
    g_lj_state = 0;
    if (__intrinsic_setjmp(g_jb) == 0) {
        g_lj_state = 1;
        __try {
            fault_with_finally(); // faults; its handler passes us onward
        } __except (filt_exec(_exception_code())) {
            longjmp(g_jb, 42); // unwind to the setjmp
            g_lj_state = -1;   // unreached
        }
    }
    // Control resumes here: setjmp "returned" 42 via longjmp.
    if (g_lj_state == 1 && g_finally_lj)
        print("SEH: longjmp ran the intervening __finally and returned 42\n");
    else
        print("SEH: FAIL case5\n");

    print("SEH: reached the end (survived all faults)\n");
    // Exit-code bitmask for the boot self-test: bit i = case i+1 behaved.
    u32 mask = 0;
    if (g_rec[1] == 1) mask |= 1;
    if (g_rec[2] == 1 && g_finally_ran) mask |= 2;
    if (g_rec[3] == 1) mask |= 4;
    if (g_rec[4] == 1 && !g_inner_ran) mask |= 8;
    if (g_lj_state == 1 && g_finally_lj) mask |= 16;
    ExitProcess(mask);
}
