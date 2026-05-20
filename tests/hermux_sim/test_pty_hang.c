/**
 * test_pty_hang.c — Minimal reproducer for PTY slave hang in vproc.
 *
 * Uses raw syscall write for debug output so it bypasses fd swap + LD_PRELOAD.
 * Build: cc -o test_pty_hang test_pty_hang.c -lvproc -lpthread -ldl -lutil
 * Run:   LD_LIBRARY_PATH=../../target/debug ./test_pty_hang
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <pty.h>

extern unsigned int vproc_ffi_create_process(
    const char *path, const char *const *argv, const char *const *envp,
    int stdin_fd, int stdout_fd, int stderr_fd);
extern int vproc_ffi_run_until_exit(unsigned int vpid);

/* Raw write via syscall — completely bypasses LD_PRELOAD */
static long raw_write2(int fd, const char *msg, size_t len) {
    register long x8 __asm__("x8") = 64;
    register long x0 __asm__("x0") = fd;
    register long x1 __asm__("x1") = (long)msg;
    register long x2 __asm__("x2") = (long)len;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory");
    return x0;
}

static void dbg(const char *msg) {
    static int saved_fd = -1;
    if (saved_fd == -1) saved_fd = dup(2);
    raw_write2(saved_fd, msg, strlen(msg));
}

#define SHELL "/data/data/com.termux/files/usr/bin/sh"

static void alarm_handler(int sig) { (void)sig; dbg("[pty] *** TIMEOUT ***\n"); _exit(2); }
static void set_alarm(int secs) {
    struct sigaction sa = {.sa_handler = alarm_handler};
    sigaction(SIGALRM, &sa, NULL);
    alarm(secs);
}

/* Test: run a command with given stdout fd, return exit code (or -1 on timeout) */
static int test_run(const char *cmd, int stdin_fd, int stdout_fd, int stderr_fd) {
    char buf[128];
    snprintf(buf, sizeof(buf), "[pty] test_run: cmd='%s' stdin=%d stdout=%d stderr=%d\n",
             cmd, stdin_fd, stdout_fd, stderr_fd);
    dbg(buf);

    const char *argv[] = {SHELL, "-c", cmd, NULL};
    const char *envp[] = {"PATH=/data/data/com.termux/files/usr/bin:/usr/bin", NULL};

    unsigned int vpid = vproc_ffi_create_process(SHELL, argv, envp, stdin_fd, stdout_fd, stderr_fd);
    snprintf(buf, sizeof(buf), "[pty] vpid=%u\n", vpid);
    dbg(buf);

    if (vpid == 0) { dbg("[pty] create_process FAILED\n"); return -1; }

    set_alarm(5);
    int code = vproc_ffi_run_until_exit(vpid);
    alarm(0);
    snprintf(buf, sizeof(buf), "[pty] exit_code=%d\n", code);
    dbg(buf);
    return code;
}

int main(void) {
    setenv("VPROC", "1", 1);
    int passed = 0, failed = 0;
    char buf[128];
    int devnull = open("/dev/null", O_RDWR);
    int pipe_fds[2], pty_fds[2];
    pipe(pipe_fds);
    openpty(&pty_fds[0], &pty_fds[1], NULL, NULL, NULL);

    snprintf(buf, sizeof(buf), "[pty] devnull=%d pipe_r=%d pipe_w=%d pty_master=%d pty_slave=%d\n",
             devnull, pipe_fds[0], pipe_fds[1], pty_fds[0], pty_fds[1]);
    dbg(buf);

    /* --- Test 1: pipe stdout, no output (sanity) --- */
    dbg("\n--- Test 1: pipe stdout, 'true' ---\n");
    {
        int code = test_run("true", devnull, pipe_fds[1], devnull);
        if (code == 0) { dbg("[pty] PASS\n"); passed++; } else { dbg("[pty] FAIL\n"); failed++; }
    }

    /* --- Test 2: pipe stdout, echo hello --- */
    dbg("\n--- Test 2: pipe stdout, 'echo hello' ---\n");
    {
        int code = test_run("echo hello", devnull, pipe_fds[1], devnull);
        if (code == 0) { dbg("[pty] PASS\n"); passed++; } else { dbg("[pty] FAIL\n"); failed++; }
    }

    /* --- Test 3: PTY slave stdout, no output (true) --- */
    dbg("\n--- Test 3: PTY slave stdout, 'true' ---\n");
    {
        int code = test_run("true", devnull, pty_fds[1], devnull);
        if (code == 0) { dbg("[pty] PASS\n"); passed++; } else { dbg("[pty] FAIL\n"); failed++; }
    }

    /* --- Test 4: PTY slave stdout, echo hello --- */
    dbg("\n--- Test 4: PTY slave stdout, 'echo hello' ---\n");
    {
        int code = test_run("echo hello", devnull, pty_fds[1], devnull);
        if (code == 0) { dbg("[pty] PASS\n"); passed++; } else { dbg("[pty] FAIL\n"); failed++; }
    }

    /* --- Test 5: PTY slave for ALL fds (0/1/2), true --- */
    dbg("\n--- Test 5: PTY slave for ALL fds (0/1/2), 'true' ---\n");
    {
        int code = test_run("true", pty_fds[1], pty_fds[1], pty_fds[1]);
        if (code == 0) { dbg("[pty] PASS\n"); passed++; } else { dbg("[pty] FAIL\n"); failed++; }
    }

    snprintf(buf, sizeof(buf), "\n=== Results: %d passed, %d failed ===\n", passed, failed);
    dbg(buf);

    close(devnull);
    close(pipe_fds[0]); close(pipe_fds[1]);
    close(pty_fds[0]); close(pty_fds[1]);
    return failed > 0 ? 1 : 0;
}
