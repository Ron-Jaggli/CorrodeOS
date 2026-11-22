/* Arguments and exit codes. win-test types (in the CorrodeOS shell, where
 * single quotes keep everything):
 *   win args.exe plain 'two words' '' 'a\"b' 'C:\dir name\' 'say "hi"' 7
 * Each argument must arrive exactly as typed, through the terminal's
 * Windows quoting and msvcrt's split; then the exit code is the last
 * argument's number (7). With no arguments it checks argv[0] and exits 0. */
#include <stdlib.h>
#include "check.h"

int main(int argc, char **argv) {
    for (int i = 0; i < argc; i++)
        printf("argv[%d] = [%s]\n", i, argv[i]);
    if (argc == 1) {
        CHECK("argv[0] names the program", strstr(argv[0], "args.exe") != NULL);
        CHECK("argv ends in NULL", argv[1] == NULL);
        printf("args: %d of %d checks passed\n", checks_run - checks_failed, checks_run);
        return checks_failed;
    }
    CHECK("eight arguments and the name", argc == 8);
    if (argc == 8) {
        CHECK_STR("plain", argv[1], "plain");
        CHECK_STR("a space kept", argv[2], "two words");
        CHECK_STR("an empty argument", argv[3], "");
        CHECK_STR("a quote after a backslash", argv[4], "a\\\"b");
        CHECK_STR("a backslash before the end", argv[5], "C:\\dir name\\");
        CHECK_STR("quotes inside an argument", argv[6], "say \"hi\"");
        CHECK_STR("the last", argv[7], "7");
        CHECK("argv ends in NULL", argv[8] == NULL);
    }
    printf("args: %d of %d checks passed\n", checks_run - checks_failed, checks_run);
    /* The exit code is the last argument when every check passed. */
    return checks_failed ? 100 + checks_failed : atoi(argv[argc - 1]);
}
