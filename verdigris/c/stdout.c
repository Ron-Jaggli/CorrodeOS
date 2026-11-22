/* Writing to the standard streams: fputs, puts, putchar, fputc to stdout
 * and stderr, fwrite, a line longer than any buffer, printf's return
 * values, fflush, and a last line with no line feed, written at exit. It
 * cannot read its own output, so it checks what each call returns and
 * prints lines whose text win-test looks for. A test program, not part of
 * CorrodeOS (which is Rust only). */
#include <stdlib.h>
#include "check.h"

static void last_words(void) {
    /* No line feed: the runtime must still send it when the program ends. */
    fputs("last words, no line feed", stdout);
}

int main(void) {
    int r;
    size_t n;

    r = fputs("fputs: one line\n", stdout);
    CHECK("fputs returns non-negative", r >= 0);
    r = puts("puts: adds the line feed");
    CHECK("puts returns non-negative", r >= 0);

    const char *bytes = "putchar: byte by byte\n";
    int last = 0;
    for (const char *p = bytes; *p; p++)
        last = putchar(*p);
    CHECK("putchar returns the byte", last == '\n');
    fputs("putchar wraps: ", stdout);
    r = putchar(0x141);
    putchar('\n');
    CHECK("putchar of 0x141 is 'A'", r == 'A');

    int all = 1;
    for (const char *p = "fputc: to stdout\n"; *p; p++)
        all &= fputc(*p, stdout) == *p;
    CHECK("fputc to stdout returns each byte", all);
    all = 1;
    for (const char *p = "fputc: to stderr\n"; *p; p++)
        all &= fputc(*p, stderr) == *p;
    CHECK("fputc to stderr returns each byte", all);
    r = fprintf(stderr, "stderr: %s %d\n", "printf", 2);
    CHECK("fprintf to stderr counts 17", r == 17);

    n = fwrite("fwrite: abcd", 4, 3, stdout);
    fwrite("\n", 1, 1, stdout);
    CHECK("fwrite returns the count", n == 3);
    n = fwrite("zzz", 0, 3, stdout);
    CHECK("fwrite of size 0 returns 0", n == 0);
    n = fwrite("zzz", 3, 0, stdout);
    CHECK("fwrite of count 0 returns 0", n == 0);

    /* 600 bytes in one line: "long: " and 594 digits, 0 to 9 over and over. */
    char long_line[601];
    memcpy(long_line, "long: ", 6);
    for (int i = 6; i < 600; i++)
        long_line[i] = (char)('0' + (i - 6) % 10);
    long_line[600] = '\n';
    n = fwrite(long_line, 1, 601, stdout);
    CHECK("a 600-byte line is written whole", n == 601);
    puts("after the long line");

    r = printf("printf: %d %s\n", 12345, "abc");
    CHECK("printf counts 18", r == 18);
    r = printf("%5s|\n", "ab");
    CHECK("printf counts its padding", r == 7);
    r = printf("%s", "");
    CHECK("printf of nothing counts 0", r == 0);

    /* A part line, flushed, then its end: still one line. */
    printf("part one, ");
    r = fflush(stdout);
    printf("part two\n");
    CHECK("fflush(stdout)", r == 0);
    CHECK("fflush(stderr) and fflush(NULL)", fflush(stderr) == 0 && fflush(NULL) == 0);

    atexit(last_words);
    DONE("stdout");
}
