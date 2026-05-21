/**
 * test_fork_waitpid.c -- 验证 vproc 的 fork + waitpid 路径。
 *
 * 对比三种模式：
 *   1. bash -c "echo hello"         (builtin echo, no fork)
 *   2. bash -c "/usr/bin/echo hi"   (fork + exec, external command)
 *   3. bash -c "echo hi | cat"      (pipe, two forks)
 *
 * 编译: cc -o test_fork_waitpid test_fork_waitpid.c -L../../target/debug -lvproc -lutil
 * 运行: VPROC=1 LD_LIBRARY_PATH=../../target/debug ./test_fork_waitpid
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <pty.h>
#include <poll.h>
#include <sys/syscall.h>
#include <sys/wait.h>

#define raw_write(fd, buf, len) syscall(__NR_write, (fd), (buf), (len))
#define raw_read(fd, buf, len) syscall(__NR_read, (fd), (buf), (len))

extern unsigned int vproc_ffi_create_process(
    const char *path, const char *const *argv, const char *const *envp,
    int stdin_fd, int stdout_fd, int stderr_fd);
extern int vproc_ffi_run_until_exit(unsigned int vpid);

static int tests = 0, passed = 0;

static void test(const char *name, int ok) {
    tests++;
    if (ok) {
        passed++;
        raw_write(1, "  PASS: ", 8);
    } else {
        raw_write(1, "  FAIL: ", 8);
    }
    raw_write(1, name, strlen(name));
    raw_write(1, "\n", 1);
}

static char **build_envp(void) {
    extern char **environ;
    int count = 0;
    while (environ[count]) count++;

    /* Add TERM and other vars */
    const char *extra[] = {
        "TERM=xterm-256color",
        "SHLVL=1",
        NULL,
    };
    int extra_count = 0;
    while (extra[extra_count]) extra_count++;

    char **envp = malloc((count + extra_count + 1) * sizeof(char *));
    int idx = 0;
    for (int i = 0; i < count; i++) envp[idx++] = environ[i];
    for (int i = 0; extra[i]; i++) envp[idx++] = (char *)extra[i];
    envp[idx] = NULL;
    return envp;
}

static int run_test(const char *cmd, const char *expect, int timeout_sec) {
    int master, slave;
    if (openpty(&master, &slave, NULL, NULL, NULL) < 0) {
        raw_write(2, "openpty failed\n", 15);
        return -1;
    }

    const char *bash = "/data/data/com.termux/files/usr/bin/bash";
    const char *argv[] = { "bash", "--norc", "--noprofile", "-c", cmd, NULL };
    char **envp = build_envp();

    unsigned int vpid = vproc_ffi_create_process(bash, argv, (const char *const *)envp, slave, slave, slave);
    free(envp);

    if (vpid == 0) {
        raw_write(2, "create_process failed\n", 22);
        close(slave);
        close(master);
        return -1;
    }

    /* Read PTY output */
    char output[4096] = {0};
    int total = 0;
    struct pollfd pfd = { .fd = master, .events = POLLIN };

    while (total < (int)sizeof(output) - 1) {
        int pret = poll(&pfd, 1, timeout_sec * 1000);
        if (pret <= 0) break;
        int n = raw_read(master, output + total, sizeof(output) - 1 - total);
        if (n <= 0) break;
        total += n;
    }
    output[total] = 0;

    int exit_code = vproc_ffi_run_until_exit(vpid);

    close(slave);
    close(master);

    /* Check result */
    int ok = (exit_code == 0) && strstr(output, expect);
    test(cmd, ok);
    if (!ok) {
        char buf[256];
        int n = snprintf(buf, sizeof(buf), "    exit=%d output[%d]=[", exit_code, total);
        raw_write(2, buf, n);
        int show = total > 300 ? 300 : total;
        raw_write(2, output, show);
        raw_write(2, "]\n", 2);
    }

    return ok;
}

int main(void) {
    setenv("VPROC", "1", 1);

    raw_write(1, "=== fork + waitpid test ===\n", 28);

    run_test("echo builtin_hello", "builtin_hello", 5);
    run_test("/data/data/com.termux/files/usr/bin/echo external_hello; true", "external_hello", 5);
    run_test("echo pipe_hello | cat", "pipe_hello", 5);
    run_test("/data/data/com.termux/files/usr/bin/echo fork_hello; /usr/bin/true", "fork_hello", 5);
    /* Additional pipe tests */
    run_test("/data/data/com.termux/files/usr/bin/echo pipe2; /data/data/com.termux/files/usr/bin/cat /proc/self/fd/0 <<< pipe2test", "pipe2test", 5);
    run_test("echo subshello | /data/data/com.termux/files/usr/bin/cat", "subshello", 5);

    char buf[128];
    int n = snprintf(buf, sizeof(buf), "\nResults: %d/%d passed\n", passed, tests);
    raw_write(1, buf, n);

    return (passed == tests) ? 0 : 1;
}
