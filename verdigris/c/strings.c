/* string.h, stdlib.h and ctype.h, each result against the value the C
 * standard gives. Also getenv (no environment: always NULL), rand/srand
 * (same seed, same sequence), time and clock. */
#include <ctype.h>
#include <errno.h>
#include <limits.h>
#include <stdlib.h>
#include <time.h>
#include "check.h"

static int by_value(const void *a, const void *b) {
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}
static int by_name(const void *a, const void *b) {
    return strcmp(*(const char *const *)a, *(const char *const *)b);
}

int main(void) {
    char buffer[64], *end;

    CHECK("strlen", strlen("verdigris") == 9 && strlen("") == 0);
    CHECK("strcmp", strcmp("abc", "abd") < 0 && strcmp("b", "a") > 0 && strcmp("x", "x") == 0);
    CHECK("strncmp", strncmp("abcdef", "abcxyz", 3) == 0 && strncmp("abcdef", "abcxyz", 4) < 0);
    strcpy(buffer, "copper");
    CHECK_STR("strcpy", buffer, "copper");
    strcat(buffer, " oxide");
    CHECK_STR("strcat", buffer, "copper oxide");
    memset(buffer, 0, sizeof buffer);
    strncpy(buffer, "patina", 3);
    CHECK_STR("strncpy stops at n", buffer, "pat");
    CHECK_STR("strchr", strchr("green/blue", '/'), "/blue");
    CHECK("strchr misses", strchr("green", 'z') == NULL);
    CHECK("strchr finds the end", *strchr("green", '\0') == '\0');
    CHECK_STR("strrchr", strrchr("a/b/c", '/'), "/c");
    CHECK_STR("strstr", strstr("the copper roof", "per"), "per roof");
    CHECK("strstr misses", strstr("copper", "tin") == NULL);
    CHECK_STR("strstr of empty", strstr("copper", ""), "copper");
    memcpy(buffer, "12345", 6);
    memmove(buffer + 1, buffer, 4);
    CHECK_STR("memmove overlapping", buffer, "11234");
    CHECK("memcmp", memcmp("abc", "abd", 3) < 0 && memcmp("abc", "abd", 2) == 0);
    CHECK_STR("memchr", (char *)memchr("verdigris", 'd', 9), "digris");

    CHECK("atoi", atoi("  -123xyz") == -123 && atoi("42") == 42);
    CHECK("strtol base 10", strtol("  +77 rest", &end, 10) == 77 && strcmp(end, " rest") == 0);
    CHECK("strtol base 16", strtol("0x1F", &end, 16) == 31 && *end == 0);
    CHECK("strtol base 0", strtol("0x10", NULL, 0) == 16 && strtol("010", NULL, 0) == 8 && strtol("10", NULL, 0) == 10);
    CHECK("strtol base 2", strtol("1011", NULL, 2) == 11);
    CHECK("strtol nothing", strtol("zz", &end, 10) == 0 && strcmp(end, "zz") == 0);
    errno = 0;
    CHECK("strtol overflow", strtol("99999999999", NULL, 10) == LONG_MAX && errno == ERANGE);
    CHECK("strtoul", strtoul("4294967295", NULL, 10) == 4294967295ul);
    CHECK("strtoul negative wraps", strtoul("-1", NULL, 10) == ULONG_MAX);

    int numbers[] = {42, -7, 19, 0, 1000, -7, 3, 88};
    qsort(numbers, 8, sizeof numbers[0], by_value);
    int sorted[] = {-7, -7, 0, 3, 19, 42, 88, 1000};
    CHECK("qsort numbers", memcmp(numbers, sorted, sizeof sorted) == 0);
    int key = 88, miss = 5;
    int *found = bsearch(&key, numbers, 8, sizeof numbers[0], by_value);
    CHECK("bsearch finds", found && found - numbers == 6);
    CHECK("bsearch misses", bsearch(&miss, numbers, 8, sizeof numbers[0], by_value) == NULL);
    const char *names[] = {"kernel32", "msvcrt", "corrode", "abi", "verdigris"};
    qsort(names, 5, sizeof names[0], by_name);
    sprintf(buffer, "%s %s %s %s %s", names[0], names[1], names[2], names[3], names[4]);
    CHECK_STR("qsort strings", buffer, "abi corrode kernel32 msvcrt verdigris");
    int many[300];
    for (int i = 0; i < 300; i++) many[i] = (i * 7919) % 300;
    qsort(many, 300, sizeof many[0], by_value);
    int in_order = 1;
    for (int i = 0; i < 300; i++) if (many[i] != i) in_order = 0;
    CHECK("qsort 300", in_order);

    CHECK("snprintf truncates", snprintf(buffer, 6, "%s", "verdigris") == 9 && strcmp(buffer, "verdi") == 0);
    CHECK("snprintf of 0 counts", snprintf(NULL, 0, "%d-%d", 10, 200) == 6);

    CHECK("isdigit", isdigit('7') && !isdigit('a'));
    CHECK("isalpha", isalpha('q') && isalpha('Q') && !isalpha('1'));
    CHECK("isalnum", isalnum('z') && isalnum('5') && !isalnum('_'));
    CHECK("isspace", isspace(' ') && isspace('\t') && isspace('\n') && !isspace('x'));
    CHECK("isupper islower", isupper('A') && !isupper('a') && islower('a') && !islower('A'));
    CHECK("isxdigit", isxdigit('f') && isxdigit('F') && isxdigit('9') && !isxdigit('g'));
    CHECK("ispunct", ispunct('!') && !ispunct('a') && !ispunct(' '));
    CHECK("toupper tolower", toupper('a') == 'A' && tolower('Q') == 'q' && toupper('1') == '1');

    CHECK("getenv: no environment", getenv("PATH") == NULL);
    srand(2026);
    int first[5];
    for (int i = 0; i < 5; i++) first[i] = rand();
    srand(2026);
    int same = 1, in_range = 1;
    for (int i = 0; i < 5; i++) {
        int r = rand();
        if (r != first[i]) same = 0;
        if (r < 0 || r > RAND_MAX) in_range = 0;
    }
    CHECK("rand repeats after srand", same);
    CHECK("rand within RAND_MAX", in_range);
    CHECK("time after 2024", time(NULL) > 1704067200);
    clock_t c = clock();
    CHECK("clock runs", c != (clock_t)-1);
    DONE("strings");
}
