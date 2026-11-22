/*
 * A test program for Verdigris, not part of CorrodeOS: an ordinary C
 * console program, compiled the normal way by mingw-w64's gcc
 * (`x86_64-w64-mingw32-gcc -O2 -s hello.c -o hello.exe`: stripped, and
 * no other flags), so it imports msvcrt.dll and kernel32.dll as such
 * programs do and starts through mingw's own start-up code. win-test checks what it prints.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static __thread int per_thread = 41;

static void goodbye(void)
{
    printf("atexit ran\n");
}

int main(int argc, char **argv)
{
    atexit(goodbye);
    printf("hello from C: %d arguments\n", argc);
    for (int i = 0; i < argc; i++)
        printf("argv[%d] = [%s]\n", i, argv[i]);

    /* mingw's printf formats these itself; fprintf is Verdigris's. */
    printf("printf: %5d|%-5d|%05.1f|%x|%s\n", 42, -7, 3.14159, 0xbeef, "ok");
    fprintf(stdout, "fprintf: %+d %u %08.3f %e %g %c %5s %.2s %%\n",
            7, 4000000000u, 2.5, 12345.678, 0.0001, 'Z', "ab", "xyz");

    size_t total = 0;
    char *grow = NULL;
    for (int i = 1; i <= 64; i++) {
        grow = realloc(grow, (size_t)i * 1000);
        memset(grow, i, (size_t)i * 1000);
        total += (size_t)i * 1000;
    }
    int *zeros = calloc(1000, sizeof *zeros);
    int clean = 1;
    for (int i = 0; i < 1000; i++)
        clean &= zeros[i] == 0;
    char *copy = malloc(32);
    strcpy(copy, "malloc and free");
    printf("heap: %s, calloc zeroed %s, last byte %d\n", copy,
           clean ? "yes" : "no", grow[64 * 1000 - 1]);
    free(copy);
    free(zeros);
    free(grow);

    per_thread++;
    printf("thread-local: %d\n", per_thread);

    return argc - 1;
}
