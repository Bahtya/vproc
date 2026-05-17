/*
 * Test program for libvproc_preload.so.
 *
 * When run with VPROC=1, exit(42) should be intercepted and NOT kill
 * the process. Without VPROC=1, exit(42) terminates normally.
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(void) {
    printf("[test] pid=%d, VPROC=%s\n", getpid(), getenv("VPROC") ?: "not set");
    printf("[test] calling exit(42)...\n");
    fflush(stdout);
    exit(42);
    /* Should not reach here without VPROC */
    printf("[test] AFTER exit (should not see this without VPROC)\n");
    return 0;
}
