/* printf's formats, through msvcrt's own formatter: built with
 * __USE_MINGW_ANSI_STDIO=0, so sprintf and printf are msvcrt.dll's
 * imports, not mingw's built-in ones. Each result is compared with the
 * string the C standard gives. Exponents have two digits, as C99 and
 * Microsoft's newer runtime print them (the old msvcrt.dll printed three).
 * Rounding ties, where runtimes differ, are avoided. */
#include <limits.h>
#include "check.h"

static char buffer[256];
#define FMT(name, want, ...) do { sprintf(buffer, __VA_ARGS__); CHECK_STR(name, buffer, want); } while (0)

int main(void) {
    FMT("%d", "42", "%d", 42);
    FMT("%d negative", "-42", "%d", -42);
    FMT("%i", "7", "%i", 7);
    FMT("%d INT_MIN", "-2147483648", "%d", INT_MIN);
    FMT("%u", "4294967295", "%u", 4294967295u);
    FMT("%x", "ff", "%x", 255);
    FMT("%X", "FF", "%X", 255);
    FMT("%#x", "0xff", "%#x", 255);
    FMT("%#X", "0XFF", "%#X", 255);
    FMT("%o", "10", "%o", 8);
    FMT("%#o", "010", "%#o", 8);
    FMT("%lld", "-9223372036854775807", "%lld", -9223372036854775807LL);
    FMT("%I64u", "18446744073709551615", "%I64u", 18446744073709551615ULL);
    FMT("%hhd", "-1", "%hhd", 255);
    FMT("%hd", "-1", "%hd", 65535);
    FMT("%5d", "   42", "%5d", 42);
    FMT("%-5d|", "42   |", "%-5d|", 42);
    FMT("%05d", "-0042", "%05d", -42);
    FMT("%+d", "+5", "%+d", 5);
    FMT("% d", " 5", "% d", 5);
    FMT("%.3d", "007", "%.3d", 7);
    FMT("%.0d of 0", "", "%.0d", 0);
    FMT("%*d", "    9", "%*d", 5, 9);
    FMT("%-*d|", "9    |", "%-*d|", 5, 9);
    FMT("%s", "hello", "%s", "hello");
    FMT("%10s", "     hello", "%10s", "hello");
    FMT("%-10s|", "hello     |", "%-10s|", "hello");
    FMT("%.3s", "abc", "%.3s", "abcdef");
    FMT("%.*s", "ab", "%.*s", 2, "abcdef");
    FMT("%c", "A", "%c", 'A');
    FMT("%3c", "  A", "%3c", 'A');
    FMT("%%", "100%", "100%%");
    FMT("%p", "0000000000001234", "%p", (void *)0x1234);
    FMT("%f", "3.141593", "%f", 3.14159265);
    FMT("%.2f", "3.14", "%.2f", 3.14159265);
    FMT("%8.3f", "  -2.500", "%8.3f", -2.5);
    FMT("%.0f", "3", "%.0f", 2.75);
    FMT("%#.0f", "3.", "%#.0f", 2.75);
    FMT("%f large", "1234567.125000", "%f", 1234567.125);
    FMT("%010.3f", "-00003.142", "%010.3f", -3.14159265);
    FMT("%e", "1.234568e+04", "%e", 12345.678);
    FMT("%E", "1.234568E+04", "%E", 12345.678);
    FMT("%.2e small", "1.50e-07", "%.2e", 0.00000015);
    FMT("%e zero", "0.000000e+00", "%e", 0.0);
    FMT("%g", "0.0001", "%g", 0.0001);
    FMT("%g exponent", "1e-05", "%g", 0.00001);
    FMT("%g big", "1.23457e+08", "%g", 123456789.0);
    FMT("%g whole", "100000", "%g", 100000.0);
    FMT("%g million", "1e+06", "%g", 1000000.0);
    FMT("%g trims zeros", "2.5", "%g", 2.5);
    FMT("%#g keeps zeros", "2.50000", "%#g", 2.5);
    FMT("%G", "1E+06", "%G", 1000000.0);
    FMT("%.3g", "3.14", "%.3g", 3.14159265);
    FMT("%f negative zero", "-0.000000", "%f", -0.0);
    FMT("several at once", "[  7|abc|-1.50|ff]", "[%3d|%s|%.2f|%x]", 7, "abc", -1.5, 255);
    CHECK("sprintf returns the length", sprintf(buffer, "%d-%s", 123, "xy") == 6);
    CHECK("printf returns the length", printf("%s %d\n", "printf says", 2026) == 17);
    DONE("printf");
}
