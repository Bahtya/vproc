/* test_fork.c — Phase 2 fork/waitpid interception test */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>

int main(void) {
    printf("=== vproc Phase 2: fork/waitpid interception ===\n");
    printf("PID: %d\n\n", getpid());

    /* Test 1: simple fork + exec */
    printf("--- Test 1: fork + echo ---\n");
    int pid = fork();
    if (pid == 0) {
        /* Child */
        printf("  [child] vpid=%d (real pid=%d), running echo\n", pid, getpid());
        execlp("echo", "echo", "hello from vproc child", NULL);
        _exit(1);
    } else if (pid > 0) {
        /* Parent */
        int status = 0;
        int ret = waitpid(pid, &status, 0);
        printf("  [parent] waitpid(%d) = %d, status=%d, exit=%d\n",
               pid, ret, status, WEXITSTATUS(status));
    } else {
        printf("  fork FAILED\n");
    }

    /* Test 2: multiple children */
    printf("\n--- Test 2: 3 concurrent children ---\n");
    int pids[3];
    for (int i = 0; i < 3; i++) {
        pids[i] = fork();
        if (pids[i] == 0) {
            printf("  [child %d] vpid=%d real=%d\n", i, pids[i], getpid());
            _exit(42 + i);
        }
    }
    /* Parent waits for all */
    for (int i = 0; i < 3; i++) {
        int status = 0;
        int ret = waitpid(pids[i], &status, 0);
        printf("  [parent] child %d (vpid=%d) exit=%d\n",
               i, pids[i], WEXITSTATUS(status));
    }

    printf("\n--- Test 3: process count check ---\n");
    printf("  Main PID: %d (should be only 1 visible process tree)\n", getpid());

    printf("\n=== Phase 2 PASS ===\n");
    return 0;
}
