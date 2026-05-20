/**
 * test_c_session.c — Simulate Hermux's JNI calling pattern from C.
 *
 * Exercises the SPAWN_QUEUE → driver thread → virtual_execve_via_entry
 * path (same as Hermux's JNI calls).
 *
 * Build:
 *   cc -o test_c_session test_c_session.c -lvproc -lpthread -ldl
 * Run:
 *   LD_LIBRARY_PATH=../../target/debug ./test_c_session
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>

extern unsigned int vproc_ffi_create_process(
    const char *path, const char *const *argv, const char *const *envp,
    int stdin_fd, int stdout_fd, int stderr_fd);
extern int vproc_ffi_run_until_exit(unsigned int vpid);

#define SHELL "/data/data/com.termux/files/usr/bin/sh"

static int devnull_rd = -1;
static int devnull_wr = -1;
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) fprintf(stderr, "  TEST: %-50s", name)
#define PASS() do { fprintf(stderr, "PASS\n"); tests_passed++; } while(0)
#define FAIL(msg) do { fprintf(stderr, "FAIL: %s\n", msg); tests_failed++; } while(0)

static void alarm_handler(int sig) { (void)sig; fprintf(stderr, "\n  *** TIMEOUT ***\n"); _exit(2); }
static void set_timeout(int secs) {
    struct sigaction sa = {.sa_handler = alarm_handler};
    sigaction(SIGALRM, &sa, NULL);
    alarm(secs);
}

static int run_command(const char *cmd, char **out) {
    int pfd[2];
    if (pipe(pfd) < 0) { *out = NULL; return -1; }

    const char *argv[] = {SHELL, "-c", cmd, NULL};
    const char *envp[] = {
        "PATH=/data/data/com.termux/files/usr/bin:/usr/bin",
        "VPROC=1",
        NULL
    };

    unsigned int vpid = vproc_ffi_create_process(
        SHELL, argv, envp, devnull_rd, pfd[1], devnull_wr);

    if (vpid == 0) {
        close(pfd[0]); close(pfd[1]);
        *out = NULL;
        return -1;
    }

    int exit_code = vproc_ffi_run_until_exit(vpid);
    close(pfd[1]);

    char buf[16384];
    ssize_t total = 0, n;
    while ((n = read(pfd[0], buf + total, sizeof(buf) - total - 1)) > 0) {
        total += n;
        if (total >= (ssize_t)sizeof(buf) - 1) break;
    }
    close(pfd[0]);
    buf[total] = '\0';
    *out = strdup(buf);
    return exit_code;
}

#define RUN_TEST(fn, cmd, check) do { \
    TEST(cmd); \
    set_timeout(15); \
    char *out = NULL; \
    int code = run_command(cmd, &out); \
    if (check) PASS(); else { char m[256]; snprintf(m,256,"code=%d out=%s",code,out?out:"(null)"); FAIL(m); } \
    free(out); alarm(0); \
} while(0)

int main(void) {
    // Required: set VPROC=1 in the process environment so the preload
    // layer's enabled() check returns true for GOT-patched binaries.
    setenv("VPROC", "1", 1);

    fprintf(stderr, "\n=== C FFI Session Test (Hermux JNI path) ===\n\n");
    devnull_rd = open("/dev/null", O_RDONLY);
    devnull_wr = open("/dev/null", O_WRONLY);

    RUN_TEST(t, "echo hello", code==0 && out && strstr(out,"hello"));
    RUN_TEST(t, "exit 42", code==42);
    RUN_TEST(t, "echo hello | cat", code==0 && out && strstr(out,"hello"));
    RUN_TEST(t, "echo test_pipe | cat | cat", code==0 && out && strstr(out,"test_pipe"));
    // cat with pipe (cat is a real binary, tests ELF loading)
    {
        TEST("echo hello | cat (round 2)");
        set_timeout(15);
        char *out = NULL;
        int code = run_command("echo test_cat | cat", &out);
        if (code==0 && out && strstr(out,"test_cat")) PASS();
        else { char m[256]; snprintf(m,256,"code=%d out=%s",code,out?out:"(null)"); FAIL(m); }
        free(out); alarm(0);
    }
    RUN_TEST(t, "(echo a; echo b) | cat", code==0 && out && strstr(out,"a") && strstr(out,"b"));
    RUN_TEST(t, "printf 'foo\\nbar\\nbaz\\n' | grep ba", code==0 && out && strstr(out,"bar"));

    // Sequential commands
    set_timeout(15);
    TEST("two sequential commands");
    char *o1=NULL, *o2=NULL;
    int c1 = run_command("echo first", &o1);
    int c2 = run_command("echo second", &o2);
    if (c1==0 && c2==0 && strstr(o1,"first") && strstr(o2,"second")) PASS();
    else FAIL("see above");
    free(o1); free(o2); alarm(0);

    set_timeout(15);
    TEST("sequential pipes: hello|cat, world|cat");
    o1=NULL; o2=NULL;
    c1 = run_command("echo hello | cat", &o1);
    c2 = run_command("echo world | cat", &o2);
    if (c1==0 && c2==0 && strstr(o1,"hello") && strstr(o2,"world")) PASS();
    else FAIL("see above");
    free(o1); free(o2); alarm(0);

    // Stress test
    set_timeout(60);
    TEST("stress: 10 sequential commands");
    int ok = 1;
    for (int i = 0; i < 10; i++) {
        char cmd[64]; snprintf(cmd,64,"echo iter_%d",i);
        char *out = NULL;
        int code = run_command(cmd, &out);
        char exp[32]; snprintf(exp,32,"iter_%d",i);
        if (code!=0 || !out || !strstr(out,exp)) {
            ok = 0;
            fprintf(stderr, "\n    FAIL i=%d code=%d out=%s",i,code,out?out:"(null)");
        }
        free(out);
    }
    if (ok) PASS(); else FAIL("see above");
    alarm(0);

    fprintf(stderr, "\n--- Results: %d passed, %d failed ---\n", tests_passed, tests_failed);

    if (devnull_rd >= 0) close(devnull_rd);
    if (devnull_wr >= 0) close(devnull_wr);
    return tests_failed ? 1 : 0;
}
