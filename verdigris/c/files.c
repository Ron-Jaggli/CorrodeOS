/* Reading files through the view with stdio. Under `win`, the program's
 * view is its own folder (/programs/c), shown as C:\. lines.txt is staged
 * beside it. Files outside the folder are real but hidden: reaching for
 * one (..\..\etc\motd, which climbs no higher than C:\) must fail exactly
 * as a name that exists nowhere does. Nothing in a view can be written. */
#include <errno.h>
#include <stdlib.h>
#include "check.h"

int main(void) {
    char line[128];
    FILE *f = fopen("lines.txt", "r");
    CHECK("fopen lines.txt", f != NULL);
    if (f) {
        CHECK_STR("fgets, line 1", fgets(line, sizeof line, f) ? line : "(null)", "first line\n");
        CHECK_STR("fgets, line 2", fgets(line, sizeof line, f) ? line : "(null)", "second line, a little longer\n");
        CHECK_STR("fgets, a short buffer", fgets(line, 5, f) ? line : "(null)", "last");
        CHECK_STR("fgets, the rest without a line feed", fgets(line, sizeof line, f) ? line : "(null)", " line without an end");
        CHECK("then end of file", fgets(line, sizeof line, f) == NULL && feof(f));
        CHECK("fclose", fclose(f) == 0);
    }

    f = fopen("C:\\lines.txt", "rb");
    CHECK("fopen C:\\lines.txt, binary", f != NULL);
    if (f) {
        size_t n = fread(line, 1, sizeof line, f);
        CHECK("fread gives the whole file (64 bytes)", n == 64);
        CHECK("its bytes as written", n == 64 && memcmp(line, "first line\nsecond", 17) == 0);
        CHECK("feof after a short fread", feof(f));
        fclose(f);
    }

    f = fopen("lines.txt", "r");
    if (f) {
        int a = fgetc(f), b = fgetc(f);
        CHECK("fgetc", a == 'f' && b == 'i');
        fclose(f);
    }

    f = fopen("files.exe", "rb");
    CHECK("fopen its own program", f != NULL);
    if (f) {
        char mz[2] = {0, 0};
        CHECK("which starts MZ", fread(mz, 1, 2, f) == 2 && mz[0] == 'M' && mz[1] == 'Z');
        fclose(f);
    }

    errno = 0;
    FILE *missing = fopen("C:\\no such file.txt", "r");
    int missing_errno = errno;
    errno = 0;
    FILE *hidden = fopen("..\\..\\etc\\motd", "r");
    int hidden_errno = errno;
    CHECK("a missing file is not opened", missing == NULL);
    CHECK("missing: ENOENT", missing_errno == ENOENT);
    CHECK("a hidden file is not opened", hidden == NULL);
    CHECK("hidden: the same error as missing", hidden_errno == missing_errno);
    CHECK_STR("and the same text", strerror(hidden_errno), strerror(missing_errno));

    errno = 0;
    FILE *written = fopen("lines.txt", "w");
    CHECK("nothing can be opened to write", written == NULL && errno != 0);
    if (written) fclose(written);
    DONE("files");
}
