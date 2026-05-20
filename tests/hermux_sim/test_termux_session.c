/**
 * test_termux_session.c — Simulate Hermux/Termux terminal sessions.
 *
 * Uses the same Hermux JNI calling pattern as the C FFI test,
 * but with a session-oriented approach (multiple commands per session)
 * and pipe-based I/O (matching Hermux's actual PTY → pipe bridge).
 *
 * Build:
 *   cc -o test_termux_session test_termux_session.c -lvproc -lpthread -ldl
 * Run:
 *   LD_LIBRARY_PATH=../../target/debug ./test_termux_session
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

static int tests_passed = 0;
static int tests_failed = 0;
static int devnull_rd = -1;
static int devnull_wr = -1;

#define TEST(name) fprintf(stderr, "  TEST: %-50s", name)
#define PASS() do { fprintf(stderr, "PASS\n"); tests_passed++; } while(0)
#define FAIL(msg) do { fprintf(stderr, "FAIL: %s\n", msg); tests_failed++; } while(0)

static void alarm_handler(int sig) { (void)sig; fprintf(stderr, "\n  *** TIMEOUT ***\n"); _exit(2); }
static void set_timeout(int secs) {
    struct sigaction sa = {.sa_handler = alarm_handler};
    sigaction(SIGALRM, &sa, NULL);
    alarm(secs);
}

/**
 * Run sh -c "cmd" with stdin=devnull, stdout=pipe, stderr=devnull.
 * Returns exit code, writes output to *out (caller frees).
 */
static int run_cmd(const char *cmd, char **out) {
    int pfd[2];
    if (pipe(pfd) < 0) { *out = NULL; return -1; }

    const char *argv[] = {SHELL, "-c", cmd, NULL};
    const char *envp[] = {
        "PATH=/data/data/com.termux/files/usr/bin:/usr/bin",
        "HOME=/data/data/com.termux/files/home",
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

    int code = vproc_ffi_run_until_exit(vpid);
    close(pfd[1]);

    *out = malloc(16384);
    if (*out) {
        ssize_t total = 0, n;
        while ((n = read(pfd[0], *out + total, 16384 - total - 1)) > 0) {
            total += n;
            if (total >= 16384 - 1) break;
        }
        (*out)[total] = '\0';
    }
    close(pfd[0]);
    return code;
}

#define CHECK(cmd, expect_code, expect_str) do { \
    char *_o = NULL; \
    int _c = run_cmd(cmd, &_o); \
    if (_c == expect_code && _o && strstr(_o, expect_str)) { PASS(); } \
    else { \
        char _m[256]; \
        snprintf(_m, 256, "code=%d out=%s", _c, _o ? _o : "(null)"); \
        FAIL(_m); \
    } \
    free(_o); \
} while(0)

/* ================================================================== */
/* Basic commands                                                     */
/* ================================================================== */

static void test_echo(void) {
    TEST("echo hello");
    set_timeout(15);
    CHECK("echo hello", 0, "hello");
    alarm(0);
}

static void test_exit42(void) {
    TEST("exit 42");
    set_timeout(15);
    char *out = NULL;
    int code = run_cmd("exit 42", &out);
    if (code == 42) PASS();
    else { char m[64]; snprintf(m,64,"code=%d expected 42",code); FAIL(m); }
    free(out); alarm(0);
}

static void test_exit1(void) {
    TEST("false (exit 1)");
    set_timeout(15);
    char *out = NULL;
    int code = run_cmd("false", &out);
    if (code == 1) PASS();
    else { char m[64]; snprintf(m,64,"code=%d expected 1",code); FAIL(m); }
    free(out); alarm(0);
}

/* ================================================================== */
/* Pipes                                                              */
/* ================================================================== */

static void test_pipe_basic(void) {
    TEST("echo hello | cat");
    set_timeout(15);
    CHECK("echo hello | cat", 0, "hello");
    alarm(0);
}

static void test_pipe_multi(void) {
    TEST("echo aaa bbb | cat | cat");
    set_timeout(15);
    CHECK("echo aaa bbb | cat | cat", 0, "aaa");
    alarm(0);
}

static void test_pipe_subshell(void) {
    TEST("(echo a; echo b) | cat");
    set_timeout(15);
    char *out = NULL;
    int code = run_cmd("(echo a; echo b) | cat", &out);
    if (code == 0 && out && strstr(out, "a") && strstr(out, "b")) PASS();
    else { char m[256]; snprintf(m,256,"code=%d out=%s",code,out?out:"(null)"); FAIL(m); }
    free(out); alarm(0);
}

static void test_pipe_grep(void) {
    TEST("printf ... | grep ba");
    set_timeout(15);
    CHECK("printf 'foo\\nbar\\nbaz\\n' | grep ba", 0, "bar");
    alarm(0);
}

static void test_pipe_grep_v(void) {
    TEST("printf ... | grep -v foo");
    set_timeout(15);
    char *out = NULL;
    int code = run_cmd("printf 'foo\\nbar\\nbaz\\n' | grep -v foo", &out);
    if (code == 0 && out && strstr(out, "bar") && strstr(out, "baz") && !strstr(out, "foo")) PASS();
    else { char m[256]; snprintf(m,256,"code=%d out=%s",code,out?out:"(null)"); FAIL(m); }
    free(out); alarm(0);
}

/* ================================================================== */
/* Shell features                                                     */
/* ================================================================== */

static void test_variable(void) {
    TEST("VAR=x; echo $VAR");
    set_timeout(15);
    CHECK("VAR=test123; echo $VAR", 0, "test123");
    alarm(0);
}

static void test_for_loop(void) {
    TEST("for i in 1 2 3; do echo item_$i; done");
    set_timeout(15);
    char *out = NULL;
    int code = run_cmd("for i in 1 2 3; do echo item_$i; done", &out);
    if (code == 0 && out && strstr(out, "item_1") && strstr(out, "item_3")) PASS();
    else { char m[256]; snprintf(m,256,"code=%d out=%s",code,out?out:"(null)"); FAIL(m); }
    free(out); alarm(0);
}

static void test_and_chain(void) {
    TEST("true && echo yes || echo no");
    set_timeout(15);
    CHECK("true && echo yes || echo no", 0, "yes");
    alarm(0);
}

static void test_or_chain(void) {
    TEST("false && echo yes || echo no");
    set_timeout(15);
    CHECK("false && echo yes || echo no", 0, "no");
    alarm(0);
}

static void test_semicolons(void) {
    TEST("echo a; echo b; echo c");
    set_timeout(15);
    char *out = NULL;
    int code = run_cmd("echo a; echo b; echo c", &out);
    if (code == 0 && out && strstr(out, "a") && strstr(out, "b") && strstr(out, "c")) PASS();
    else { char m[256]; snprintf(m,256,"code=%d out=%s",code,out?out:"(null)"); FAIL(m); }
    free(out); alarm(0);
}

static void test_command_subst(void) {
    TEST("echo $(echo nested)");
    set_timeout(15);
    CHECK("echo $(echo nested)", 0, "nested");
    alarm(0);
}

static void test_heredoc(void) {
    TEST("echo input_data | cat");
    set_timeout(15);
    CHECK("echo input_data | cat", 0, "input_data");
    alarm(0);
}

/* ================================================================== */
/* Sequential sessions (terminal tabs)                                */
/* ================================================================== */

static void test_sequential_tabs(void) {
    TEST("3 sequential sessions (terminal tabs)");
    set_timeout(30);
    int ok = 1;
    const char *cmds[] = {"echo tab1", "echo tab2 | cat", "echo tab3"};
    const char *exp[] = {"tab1", "tab2", "tab3"};

    for (int i = 0; i < 3; i++) {
        char *out = NULL;
        int code = run_cmd(cmds[i], &out);
        if (code != 0 || !out || !strstr(out, exp[i])) {
            ok = 0;
            fprintf(stderr, "\n    tab%d code=%d out=%s", i+1, code, out?out:"(null)");
        }
        free(out);
    }
    if (ok) PASS(); else FAIL("see above");
    alarm(0);
}

static void test_stress(void) {
    TEST("stress: 20 sequential sessions");
    set_timeout(120);
    int ok = 1;
    for (int i = 0; i < 20; i++) {
        char cmd[64], exp[32];
        snprintf(cmd, sizeof(cmd), "echo iter_%d", i);
        snprintf(exp, sizeof(exp), "iter_%d", i);
        char *out = NULL;
        int code = run_cmd(cmd, &out);
        if (code != 0 || !out || !strstr(out, exp)) {
            ok = 0;
            fprintf(stderr, "\n    i=%d code=%d out=%s", i, code, out?out:"(null)");
        }
        free(out);
        if (!ok) break;
    }
    if (ok) PASS(); else FAIL("see above");
    alarm(0);
}

static void test_stress_pipes(void) {
    TEST("stress: 10 sequential pipe sessions");
    set_timeout(120);
    int ok = 1;
    for (int i = 0; i < 10; i++) {
        char cmd[64], exp[32];
        snprintf(cmd, sizeof(cmd), "echo pipe_%d | cat", i);
        snprintf(exp, sizeof(exp), "pipe_%d", i);
        char *out = NULL;
        int code = run_cmd(cmd, &out);
        if (code != 0 || !out || !strstr(out, exp)) {
            ok = 0;
            fprintf(stderr, "\n    i=%d code=%d out=%s", i, code, out?out:"(null)");
        }
        free(out);
        if (!ok) break;
    }
    if (ok) PASS(); else FAIL("see above");
    alarm(0);
}

/* ================================================================== */

int main(void) {
    setenv("VPROC", "1", 1);
    devnull_rd = open("/dev/null", O_RDONLY);
    devnull_wr = open("/dev/null", O_WRONLY);

    fprintf(stderr, "\n=== Termux Session Simulation Test ===\n\n");

    fprintf(stderr, "--- Basic commands ---\n");
    test_echo();
    test_exit42();
    test_exit1();

    fprintf(stderr, "\n--- Pipes ---\n");
    test_pipe_basic();
    test_pipe_multi();
    test_pipe_subshell();
    test_pipe_grep();
    test_pipe_grep_v();

    fprintf(stderr, "\n--- Shell features ---\n");
    test_variable();
    test_for_loop();
    test_and_chain();
    test_or_chain();
    test_semicolons();
    test_command_subst();
    test_heredoc();

    fprintf(stderr, "\n--- Sequential sessions ---\n");
    test_sequential_tabs();
    test_stress();
    test_stress_pipes();

    fprintf(stderr, "\n--- Results: %d passed, %d failed ---\n", tests_passed, tests_failed);

    close(devnull_rd);
    close(devnull_wr);
    return tests_failed ? 1 : 0;
}
