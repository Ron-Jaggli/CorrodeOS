/* The stack a Windows program is given: mingw asks for 2 MiB
 * (SizeOfStackReserve), Verdigris gives at most 1 MiB, Windows' own
 * default. A 512 KiB array on the stack, and recursion 4000 calls deep,
 * both run as they do on Windows; and the TEB's stack bounds say the same
 * as the stack itself. A test program, not part of CorrodeOS (which is
 * Rust only). */
#include <windows.h>
#include "check.h"

#define BIG (512 * 1024)

/* Not inlined into main, so its frame is its own. */
static __attribute__((noinline)) int big_array(void) {
    volatile char big[BIG];
    for (size_t i = 0; i < BIG; i += 4096)
        big[i] = (char)(i >> 12);
    big[BIG - 1] = 7;
    int ok = big[BIG - 1] == 7;
    for (size_t i = 0; i < BIG; i += 4096)
        ok &= big[i] == (char)(i >> 12);
    return ok;
}

static __attribute__((noinline)) int deep(int depth) {
    volatile char pad[64];
    pad[0] = (char)depth;
    if (depth == 0)
        return pad[0];
    return deep(depth - 1) + pad[0] - (char)depth;
}

int main(void) {
    NT_TIB *tib = (NT_TIB *)NtCurrentTeb();
    char here;
    ULONG_PTR base = (ULONG_PTR)tib->StackBase, limit = (ULONG_PTR)tib->StackLimit;
    CHECK("the TEB's stack holds this frame", (ULONG_PTR)&here < base && (ULONG_PTR)&here >= limit);
    CHECK("the TEB says 1 MiB of stack", base - limit == 1024 * 1024);
    CHECK("a 512 KiB array on the stack", big_array());
    CHECK("recursion 4000 calls deep", deep(4000) == 0);
    DONE("bigstack");
}
