/* Threads, for the day Verdigris has them (verdigris/THREADS.md, step 5).
 * Not staged or scored yet: CreateThread is not exported, so today this
 * program is refused at load ("an import kernel32 does not have").
 *
 * Eight threads share a counter under a critical section; each keeps its
 * own __thread variable, TLS slot value and GetLastError; each returns its
 * own exit code; two hand a pair of semaphores back and forth; one ends
 * early with ExitThread; and all eight print lines that must never be
 * split by another's. A test program, not part of CorrodeOS (which is
 * Rust only). */
#include <windows.h>
#include "check.h"

#define THREADS 8
#define ADDS 100000
#define LINES 20
#define PING_PONGS 1000

static CRITICAL_SECTION lock;
static volatile long counter;
static DWORD tls_slot;
static __thread int mine;

struct result {
    int thread_local_kept;
    int tls_kept;
    int last_error_kept;
    DWORD id;
};
static struct result results[THREADS];

static DWORD WINAPI worker(LPVOID argument) {
    int index = (int)(INT_PTR)argument;
    struct result *r = &results[index];

    mine = index * 10;
    TlsSetValue(tls_slot, (LPVOID)(INT_PTR)(index + 1));
    SetLastError(1000 + index);
    r->id = GetCurrentThreadId();

    for (int i = 0; i < ADDS; i++) {
        EnterCriticalSection(&lock);
        counter++;
        LeaveCriticalSection(&lock);
        if (i % 10000 == 0)
            Sleep(0);
    }
    /* A line of fixed shape, printed whole by one call: win-test checks no
     * line was cut by another thread's. */
    for (int line = 0; line < LINES; line++)
        printf("thread %d line %02d: abcdefghijklmnopqrstuvwxyz0123456789\n", index, line);

    r->thread_local_kept = mine == index * 10;
    r->tls_kept = TlsGetValue(tls_slot) == (LPVOID)(INT_PTR)(index + 1);
    r->last_error_kept = GetLastError() == (DWORD)(1000 + index);
    return 100 + index;
}

static HANDLE ping, pong;
static volatile long handed;

static DWORD WINAPI pinger(LPVOID unused) {
    (void)unused;
    for (int i = 0; i < PING_PONGS; i++) {
        WaitForSingleObject(ping, INFINITE);
        handed++;
        ReleaseSemaphore(pong, 1, NULL);
    }
    return 0;
}

static DWORD WINAPI ponger(LPVOID unused) {
    (void)unused;
    for (int i = 0; i < PING_PONGS; i++) {
        WaitForSingleObject(pong, INFINITE);
        handed++;
        ReleaseSemaphore(ping, 1, NULL);
    }
    return 0;
}

static DWORD WINAPI quitter(LPVOID unused) {
    (void)unused;
    ExitThread(55);
    return 1; /* never reached */
}

static DWORD WINAPI sleeper(LPVOID unused) {
    (void)unused;
    Sleep(300);
    return 7;
}

int main(void) {
    HANDLE threads[THREADS];
    DWORD code;

    InitializeCriticalSection(&lock);
    tls_slot = TlsAlloc();
    CHECK("a TLS slot", tls_slot != TLS_OUT_OF_INDEXES);
    TlsSetValue(tls_slot, (LPVOID)0x5a5a);
    mine = -1;

    for (int i = 0; i < THREADS; i++)
        threads[i] = CreateThread(NULL, 0, worker, (LPVOID)(INT_PTR)i, 0, NULL);
    int created = 1;
    for (int i = 0; i < THREADS; i++)
        created &= threads[i] != NULL;
    CHECK("eight threads start", created);
    if (!created)
        DONE("threads");

    int waited = 1, codes = 1;
    for (int i = 0; i < THREADS; i++) {
        waited &= WaitForSingleObject(threads[i], 10000) == WAIT_OBJECT_0;
        codes &= GetExitCodeThread(threads[i], &code) && code == (DWORD)(100 + i);
        CloseHandle(threads[i]);
    }
    CHECK("each thread is waited for", waited);
    CHECK("each thread's exit code", codes);
    CHECK("the counter reaches 800000 under the lock", counter == (long)THREADS * ADDS);

    int local = 1, tls = 1, errors = 1, ids = 1;
    for (int i = 0; i < THREADS; i++) {
        local &= results[i].thread_local_kept;
        tls &= results[i].tls_kept;
        errors &= results[i].last_error_kept;
        ids &= results[i].id != 0 && results[i].id != GetCurrentThreadId();
        for (int j = 0; j < i; j++)
            ids &= results[i].id != results[j].id;
    }
    CHECK("__thread is per thread", local && mine == -1);
    CHECK("TLS slots are per thread", tls && TlsGetValue(tls_slot) == (LPVOID)0x5a5a);
    CHECK("GetLastError is per thread", errors);
    CHECK("thread ids differ", ids);

    ping = CreateSemaphoreW(NULL, 1, 1, NULL);
    pong = CreateSemaphoreW(NULL, 0, 1, NULL);
    HANDLE a = CreateThread(NULL, 0, pinger, NULL, 0, NULL);
    HANDLE b = CreateThread(NULL, 0, ponger, NULL, 0, NULL);
    int both = a && b
        && WaitForSingleObject(a, 10000) == WAIT_OBJECT_0
        && WaitForSingleObject(b, 10000) == WAIT_OBJECT_0;
    CHECK("two semaphores handed back and forth 1000 times", both && handed == 2 * PING_PONGS);

    HANDLE q = CreateThread(NULL, 0, quitter, NULL, 0, NULL);
    CHECK("ExitThread's code", q && WaitForSingleObject(q, 10000) == WAIT_OBJECT_0
                               && GetExitCodeThread(q, &code) && code == 55);

    HANDLE s = CreateThread(NULL, 0, sleeper, NULL, 0, NULL);
    int still = s && WaitForSingleObject(s, 0) == WAIT_TIMEOUT
                && GetExitCodeThread(s, &code) && code == STILL_ACTIVE;
    CHECK("a running thread is not ended yet", still);
    CHECK("then it ends with its code", s && WaitForSingleObject(s, 10000) == WAIT_OBJECT_0
                                        && GetExitCodeThread(s, &code) && code == 7);

    DeleteCriticalSection(&lock);
    DONE("threads");
}
