/* Shared by the Verdigris C test programs: one line per check, then a
 * summary the boot test scores, and the number of failures as the exit
 * code. A test program, not part of CorrodeOS (which is Rust only). */
#ifndef CHECK_H
#define CHECK_H
#include <stdio.h>
#include <string.h>

static int checks_run, checks_failed;

#define CHECK(name, ok) do { \
    checks_run++; \
    if (ok) printf("ok   %s\n", name); \
    else { checks_failed++; printf("FAIL %s\n", name); } \
} while (0)

#define CHECK_STR(name, got, want) do { \
    const char *g_ = (got), *w_ = (want); \
    checks_run++; \
    if (strcmp(g_, w_) == 0) printf("ok   %s\n", name); \
    else { checks_failed++; printf("FAIL %s: got [%s] want [%s]\n", name, g_, w_); } \
} while (0)

#define DONE(program) do { \
    printf("%s: %d of %d checks passed\n", program, checks_run - checks_failed, checks_run); \
    return checks_failed; \
} while (0)
#endif
