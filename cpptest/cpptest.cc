// cpptest.cc — C++ EH end-to-end test through the real machinery:
//   throw -> _CxxThrowException -> RaiseException -> dispatch walk ->
//   __CxxFrameHandler3 -> catch block, with destructor unwinding.
//
// Cases:
//  1. throw 42 / catch(int) — exact-type match through the catchable array;
//  2. an object's destructor must run while the throw unwinds its frame;
//  3. a nested try whose inner catch(double) does NOT match int — the outer
//     catch(int) must take it (the state walk skips the wrong handler);
//  4. catch(...) takes whatever's left;
//  5. an object in the *intermediate* frame between throw and catch is
//     destroyed as the unwind pass travels through (the second pass).
//
// Exit code = bitmask of the cases that behaved; the kernel boot test
// asserts 0x1F. Built with clang++ -fexceptions (scripts/build-cpptest.sh).

typedef unsigned long long u64;
typedef unsigned int u32;

extern "C" {
__declspec(dllimport) u64 GetStdHandle(u32);
__declspec(dllimport) int WriteConsoleA(u64, const void*, u32, u32*, u64);
__declspec(dllimport) void ExitProcess(u32);
}

static void print(const char* s) {
    u32 n = 0;
    while (s[n]) n++;
    u32 w = 0;
    WriteConsoleA(GetStdHandle((u32)-11), s, n, &w, 0);
}

// Case 2's destructor witness.
static volatile int g_dtor_ran;
struct Bomb {
    int id;
    explicit Bomb(int i) : id(i) {}
    ~Bomb() { g_dtor_ran = id; }
};

static volatile int g_caught_int, g_caught_value, g_outer, g_caught_all;

// Case 3's helper: the throw happens a frame down, inside a nested try.
__declspec(noinline) static void throw_int_nested() {
    try {
        throw 42;
    } catch (double) {
        // must NOT match an int throw
        g_outer = -1;
    }
}

// Case 5's witness: an object living in the intermediate frame between
// the throw and the catch — the unwind pass must destroy it on the way.
static volatile int g_mid_dtor;
struct MidBomb {
    int id;
    explicit MidBomb(int i) : id(i) {}
    ~MidBomb() { g_mid_dtor = id; }
};
__declspec(noinline) static void throw_through() {
    MidBomb m(9);
    throw 42;
}

int mainCRTStartup() {
    u32 mask = 0;

    // Case 1: exact-type catch.
    try {
        throw 42;
    } catch (int x) {
        g_caught_int = 1;
        g_caught_value = x;
    }
    if (g_caught_int == 1 && g_caught_value == 42) {
        print("CPP: throw 42 caught by catch(int)\n");
        mask |= 1;
    } else {
        print("CPP: FAIL case1\n");
    }

    // Case 2: the destructor runs while the throw unwinds the frame.
    g_dtor_ran = 0;
    try {
        Bomb b(7);
        throw 1;
    } catch (int) {
        // caught
    }
    if (g_dtor_ran == 7) {
        print("CPP: destructor ran during exception unwind\n");
        mask |= 2;
    } else {
        print("CPP: FAIL case2\n");
    }

    // Case 3: a wrong inner handler is skipped; the outer takes it.
    g_outer = 0;
    try {
        throw_int_nested();
    } catch (int) {
        g_outer = 1;
    }
    if (g_outer == 1) {
        print("CPP: inner catch(double) skipped, outer catch(int) took it\n");
        mask |= 4;
    } else {
        print("CPP: FAIL case3\n");
    }

    // Case 4: catch(...) gets whatever's left.
    g_caught_all = 0;
    try {
        throw Bomb(9);
    } catch (...) {
        g_caught_all = 1;
    }
    if (g_caught_all == 1) {
        print("CPP: catch(...) caught an unmatched type\n");
        mask |= 8;
    } else {
        print("CPP: FAIL case4\n");
    }

    // Case 5: the intermediate frame's destructor runs as the throw
    // unwinds through it to the outer catch (the second, unwind, pass).
    g_mid_dtor = 0;
    try {
        throw_through();
    } catch (int) {
        // caught
    }
    if (g_mid_dtor == 9) {
        print("CPP: intermediate-frame destructor ran on the way out\n");
        mask |= 0x10;
    } else {
        print("CPP: FAIL case5\n");
    }

    print("CPP: reached the end\n");
    ExitProcess(mask);
    return 0;
}
