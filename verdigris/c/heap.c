/* malloc, calloc, realloc and free under load: thousands of blocks of
 * many sizes, each filled and checked; half freed and the rest resized;
 * then blocks of 1 and 4 MiB, which the heap's first piece (256 KiB)
 * cannot hold, so it must grow; then everything freed and taken again. */
#include <stdint.h>
#include <stdlib.h>
#include "check.h"

#define BLOCKS 2000

static uint32_t seed = 12345;
static uint32_t next(void) { seed = seed * 1103515245u + 12345u; return seed >> 8; }

static unsigned char *block[BLOCKS];
static size_t size[BLOCKS];

static void fill(unsigned char *p, size_t n, unsigned tag) {
    for (size_t i = 0; i < n; i++) p[i] = (unsigned char)(tag * 31 + i);
}
static int intact(const unsigned char *p, size_t n, unsigned tag) {
    for (size_t i = 0; i < n; i++) if (p[i] != (unsigned char)(tag * 31 + i)) return 0;
    return 1;
}

int main(void) {
    int got = 1, aligned = 1, good = 1;
    for (int i = 0; i < BLOCKS; i++) {
        size[i] = 1 + next() % 4000;
        block[i] = malloc(size[i]);
        if (!block[i]) { got = 0; break; }
        if ((uintptr_t)block[i] % 16) aligned = 0;
        fill(block[i], size[i], i);
    }
    CHECK("2000 blocks of 1 to 4000 bytes", got);
    CHECK("each aligned to 16", aligned);
    for (int i = 0; got && i < BLOCKS; i++) if (!intact(block[i], size[i], i)) good = 0;
    CHECK("none overwrote another", got && good);

    for (int i = 0; got && i < BLOCKS; i += 2) { free(block[i]); block[i] = NULL; }
    good = 1;
    for (int i = 1; got && i < BLOCKS; i += 2) {
        size_t old = size[i], now = (i % 4 == 1) ? old * 3 : old / 2 + 1;
        unsigned char *p = realloc(block[i], now);
        if (!p) { good = 0; break; }
        if (!intact(p, old < now ? old : now, i)) good = 0;
        block[i] = p; size[i] = now;
        fill(p, now, i);
    }
    CHECK("realloc keeps what fits, growing and shrinking", got && good);

    unsigned char *zeroed = calloc(1000, 33);
    good = zeroed != NULL;
    for (size_t i = 0; good && i < 33000; i++) if (zeroed[i]) good = 0;
    CHECK("calloc gives zeroes", good);
    free(zeroed);

    size_t mib = 1024 * 1024;
    unsigned char *one = malloc(mib), *four = malloc(4 * mib);
    CHECK("1 MiB and 4 MiB: the heap grew past its first piece", one && four);
    if (one && four) {
        fill(one, mib, 1); fill(four, 4 * mib, 4);
        CHECK("both whole", intact(one, mib, 1) && intact(four, 4 * mib, 4));
    }
    good = 1;
    for (int i = 1; got && i < BLOCKS; i += 2) if (!intact(block[i], size[i], i)) good = 0;
    CHECK("the small blocks survived the big ones", got && good);
    free(one); free(four);

    for (int i = 0; i < BLOCKS; i++) { free(block[i]); block[i] = NULL; }
    got = 1;
    for (int round = 0; round < 3; round++) {
        unsigned char *again = malloc(2 * mib);
        if (!again) { got = 0; break; }
        fill(again, 2 * mib, round);
        if (!intact(again, 2 * mib, round)) got = 0;
        free(again);
    }
    CHECK("freed memory is used again (3 x 2 MiB)", got);

    void *empty = malloc(0);
    CHECK("malloc(0) is a block of its own", empty != NULL);
    free(empty);
    free(NULL);
    CHECK("free(NULL) does nothing", 1);
    char *fresh = realloc(NULL, 10);
    CHECK("realloc(NULL, n) is malloc", fresh != NULL);
    free(fresh);
    DONE("heap");
}
