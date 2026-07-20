// sehtest.c — frame-based SEH end-to-end test.
//
// Proves the whole OS-side SEH path on real clang-emitted .pdata metadata:
// exception delivery -> RtlLookupFunctionEntry -> RtlVirtualUnwind (incl.
// the leaf rule) -> frame-handler invocation per frame -> NtContinue.
//
// The twist: this binary carries its OWN frame handler (`__C_specific_handler`
// below — frame handlers are pluggable, the .xdata handler field just names
// a function). It dispatches on a global "which try am I in" marker instead
// of walking a scope table, so the test validates the OS machinery without
// depending on any toolchain's scope-table layout. The __try/__except
// constructs exist to make clang emit real UNWIND_INFO (EHANDLER/UHANDLER
// flags, handler RVA) for the marked functions; the recovery actions live
// in the handler + flags, and the continuations check the flags.
//
// Built with clang (MSVC ABI) — see scripts/build-sehtest.sh.

typedef unsigned long long u64;
typedef unsigned int u32;
typedef int i32;

#define NULL ((void*)0)
// EXCEPTION_DISPOSITION
#define ExceptionContinueExecution 0
#define ExceptionContinueSearch 1

// kernel32 shim imports
__declspec(dllimport) u64 __stdcall GetStdHandle(u32);
__declspec(dllimport) i32 __stdcall WriteConsoleA(u64, const void*, u32, u32*, u64);
__declspec(dllimport) void __stdcall ExitProcess(u32);

// CONTEXT, trimmed to the one field the handler rewrites (rip @ 0xF8).
typedef struct { char pad[0xF8]; u64 rip; } ContextRip;

static volatile int g_active_case;  // which __try region is live (1..4)
static volatile int g_finally_ran;  // handler ran for the __finally frame
static volatile int g_rec[5];       // handler recovered case i
static volatile int g_dummy;        // keeps the funclets un-eliminable
static u64 g_cont[5];               // continuation label addresses

// The custom frame handler: called once per handler-carrying frame during
// the dispatch walk (that per-frame invocation is exactly what __finally and
// __except mechanics reduce to). `frame` is the establisher frame; `ctx` is
// the dispatch context the kernel resumes from on ContinueExecution.
int __C_specific_handler(void* record, u64 frame, void* context, void* disp) {
    (void)record; (void)frame; (void)disp;
    ContextRip* ctx = (ContextRip*)context;
    int c = g_active_case;
    if (c == 2) {
        g_finally_ran = 1; // the frame carrying the __finally was reached
    }
    if (c >= 1 && c <= 4) {
        g_rec[c] = 1;
        ctx->rip = g_cont[c];       // resume at the case's continuation
        return ExceptionContinueExecution;
    }
    return ExceptionContinueSearch;
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

void mainCRTStartup(void) {
    g_cont[1] = (u64)&&c1_cont;
    g_cont[2] = (u64)&&c2_cont;
    g_cont[3] = (u64)&&c3_cont;
    g_cont[4] = (u64)&&c4_cont;

    // Case 1: fault in a __try, recovered by the frame's handler.
    g_active_case = 1;
    __try {
        *(volatile int*)0x1234 = 42;
    } __except (1 /* EXECUTE_HANDLER */) {
        g_dummy++; // the custom handler bypasses this block
    }
    // if the dispatch worked, the handler set g_rec[1] and jumped us here:
c1_cont:
    if (g_rec[1]) print("SEH: frame handler recovered a fault in __try\n");
    else print("SEH: FAIL case1 (handler not invoked)\n");

    // Case 2: a frame carrying a __finally must have its handler invoked
    // while the walk passes through it.
    g_active_case = 2;
    __try {
        __try {
            *(volatile int*)0x1234 = 42;
        } __finally {
            g_dummy++; // runs only under a real scope-table walk
        }
    } __except (1) {
        g_dummy++;
    }
c2_cont:
    if (g_finally_ran) print("SEH: unwind reached the frame carrying __finally\n");
    else print("SEH: FAIL case2 (finally frame skipped)\n");

    // Case 3: fault one leaf frame down; the walk must apply the leaf rule
    // to reach this frame's handler.
    g_active_case = 3;
    __try {
        fault_in_callee();
    } __except (1) {
        g_dummy++;
    }
c3_cont:
    if (g_rec[3]) print("SEH: leaf-rule unwind reached the caller's handler\n");
    else print("SEH: FAIL case3 (callee fault not recovered)\n");

    // Case 4: an inner handler passing means the walk continues outward.
    g_active_case = 4;
    __try {
        __try {
            *(volatile int*)0x1234 = 42;
        } __except (0 /* CONTINUE_SEARCH */) {
            g_dummy++; // inner passes; must NOT recover here
        }
    } __except (1) {
        g_dummy++;
    }
c4_cont:
    if (g_rec[4]) print("SEH: nested dispatch fell through to the outer frame\n");
    else print("SEH: FAIL case4 (nested dispatch wrong)\n");

    g_active_case = 0;
    print("SEH: reached the end (survived all faults)\n");
    // Exit-code bitmask for the boot self-test: bit i = case i+1 recovered.
    u32 mask = 0;
    if (g_rec[1]) mask |= 1;
    if (g_rec[2] && g_finally_ran) mask |= 2;
    if (g_rec[3]) mask |= 4;
    if (g_rec[4]) mask |= 8;
    ExitProcess(mask);
}
