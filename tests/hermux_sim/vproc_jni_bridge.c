/**
 * vproc_jni_bridge.c -- JNI bridge for TestTermuxSession.
 *
 * Simulates Hermux's termux.c JNI layer:
 *   - Direct libvproc FFI calls (create_process / run_until_exit)
 *   - PTY creation (openpty)
 *   - Raw syscall I/O (bypasses vproc interceptors)
 *   - Crash recovery (sigaltstack + sigsetjmp/siglongjmp)
 *   - Device info detection (CPU, MTE, Seccomp)
 */

#define _GNU_SOURCE
#include <jni.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
#include <pty.h>
#include <dlfcn.h>
#include <signal.h>
#include <setjmp.h>
#include <poll.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <link.h>          /* dl_iterate_phdr */
#include <errno.h>
#include <pthread.h>

/* ------------------------------------------------------------------
 * Ensure VPROC=1 on library load
 * ------------------------------------------------------------------ */

__attribute__((constructor))
static void ensure_vproc_env(void) {
    /* vproc 0.2.2+ auto-enables via .init_array constructor */
    /* FFI auto-resolve done in ensure_ffi() after declarations */
}

/* ------------------------------------------------------------------
 * Raw syscall I/O -- bypasses vproc interceptors
 * ------------------------------------------------------------------ */

static ssize_t raw_write(int fd, const void *buf, size_t count) {
    return syscall(__NR_write, fd, buf, count);
}

static ssize_t raw_read(int fd, void *buf, size_t count) {
    return syscall(__NR_read, fd, buf, count);
}

static void raw_log(const char *msg) {
    raw_write(2, msg, strlen(msg));
    raw_write(2, "\n", 1);
}

/* ------------------------------------------------------------------
 * libvproc FFI — dlopen-based, session-managed
 * ------------------------------------------------------------------ */

static void *g_vproc_handle;
static unsigned int (*g_create_session)(void);
static void (*g_destroy_session)(unsigned int);
static unsigned int (*g_create_process)(
    unsigned int, const char *, const char *const *, const char *const *, int, int, int);
static int (*g_run_until_exit)(unsigned int, unsigned int);

static unsigned int g_session_id = 0;
static int g_ffi_resolved = 0;

/* Auto-resolve vproc FFI symbols via RTLD_DEFAULT (called once).
 * Does NOT overwrite pointers already set by nativeLoadVproc. */
static void ensure_ffi(void) {
    if (g_ffi_resolved) return;
    g_ffi_resolved = 1;
    /* Only resolve if not already set by nativeLoadVproc */
    if (g_create_session && g_create_process && g_run_until_exit) {
        raw_log("[jni] FFI already resolved (via nativeLoadVproc)");
        return;
    }
    g_create_session = dlsym(RTLD_DEFAULT, "vproc_ffi_create_session");
    g_destroy_session = dlsym(RTLD_DEFAULT, "vproc_ffi_destroy_session");
    g_create_process = dlsym(RTLD_DEFAULT, "vproc_ffi_create_process");
    g_run_until_exit = dlsym(RTLD_DEFAULT, "vproc_ffi_run_until_exit");
    if (g_create_session && g_create_process && g_run_until_exit) {
        raw_log("[jni] auto-resolved vproc FFI via RTLD_DEFAULT");
    } else {
        raw_log("[jni] WARNING: vproc symbols not found via RTLD_DEFAULT");
    }
}

/* Ensure a session exists, creating one if needed. Returns session_id or 0. */
static unsigned int ensure_session(void) {
    if (g_session_id != 0) return g_session_id;
    ensure_ffi();
    if (!g_create_session) {
        raw_log("[jni] ensure_session: g_create_session is NULL");
        return 0;
    }
    raw_log("[jni] calling g_create_session()...");
    g_session_id = g_create_session();
    if (g_session_id == 0) {
        raw_log("[jni] create_session failed (returned 0)");
    } else {
        char buf[64];
        snprintf(buf, sizeof(buf), "[jni] session created: %u", g_session_id);
        raw_log(buf);
    }
    return g_session_id;
}

/* JNI: Load vproc from path */
JNIEXPORT jboolean JNICALL
Java_com_vproc_arttest_TestTermuxSession_nativeLoadVproc(
    JNIEnv *env, jclass cls, jstring jpath)
{
    const char *path = jpath ? (*env)->GetStringUTFChars(env, jpath, NULL) : NULL;
    if (path && path[0]) {
        g_vproc_handle = dlopen(path, RTLD_NOW | RTLD_GLOBAL);
    } else {
        /* Try RTLD_DEFAULT for already-loaded libvproc */
        g_create_session = dlsym(RTLD_DEFAULT, "vproc_ffi_create_session");
        g_destroy_session = dlsym(RTLD_DEFAULT, "vproc_ffi_destroy_session");
        g_create_process = dlsym(RTLD_DEFAULT, "vproc_ffi_create_process");
        g_run_until_exit = dlsym(RTLD_DEFAULT, "vproc_ffi_run_until_exit");
        if (path) (*env)->ReleaseStringUTFChars(env, jpath, path);
        return (g_create_session && g_create_process && g_run_until_exit) ? JNI_TRUE : JNI_FALSE;
    }
    if (!g_vproc_handle) {
        const char *err = dlerror();
        if (err) raw_log(err);
        raw_log("[jni] dlopen failed");
        (*env)->ReleaseStringUTFChars(env, jpath, path);
        return JNI_FALSE;
    }
    g_create_session = dlsym(g_vproc_handle, "vproc_ffi_create_session");
    g_destroy_session = dlsym(g_vproc_handle, "vproc_ffi_destroy_session");
    g_create_process = dlsym(g_vproc_handle, "vproc_ffi_create_process");
    g_run_until_exit = dlsym(g_vproc_handle, "vproc_ffi_run_until_exit");
    {
        char buf[256];
        snprintf(buf, sizeof(buf), "[jni] dlsym results: session=%p process=%p run=%p err=%s",
            (void*)g_create_session, (void*)g_create_process, (void*)g_run_until_exit,
            dlerror() ? dlerror() : "none");
        raw_log(buf);
    }
    (*env)->ReleaseStringUTFChars(env, jpath, path);
    return (g_create_session && g_create_process && g_run_until_exit) ? JNI_TRUE : JNI_FALSE;
}

/* Wrappers that auto-manage session and call through function pointers */
static unsigned int vproc_create_process(
    const char *path, const char *const *argv, const char *const *envp,
    int stdin_fd, int stdout_fd, int stderr_fd)
{
    ensure_ffi();
    if (!g_create_process) {
        raw_log("[jni] ERROR: g_create_process is NULL");
        return 0;
    }
    unsigned int sid = ensure_session();
    if (!sid) {
        raw_log("[jni] ERROR: ensure_session returned 0");
        return 0;
    }
    char diag[256];
    snprintf(diag, sizeof(diag), "[jni] g_create_process(sid=%u, path=%s, fds=%d/%d/%d)...",
        sid, path, stdin_fd, stdout_fd, stderr_fd);
    raw_log(diag);
    unsigned int vpid = g_create_process(sid, path, argv, envp, stdin_fd, stdout_fd, stderr_fd);
    snprintf(diag, sizeof(diag), "[jni] g_create_process returned vpid=%u", vpid);
    raw_log(diag);
    return vpid;
}

static int vproc_run_until_exit(unsigned int vpid) {
    ensure_ffi();
    if (!g_session_id || !g_run_until_exit) return -1;
    return g_run_until_exit(g_session_id, vpid);
}

/* ------------------------------------------------------------------
 * Crash recovery -- sigaltstack + sigsetjmp/siglongjmp
 * Thread-safe: only longjmps if crash happens in the test thread.
 * ------------------------------------------------------------------ */

static sigjmp_buf g_crash_jmp;
static volatile sig_atomic_t g_crash_signal = 0;
static volatile void *g_crash_fault_addr = NULL;
static volatile int g_crash_stage = 0;
static void *g_crash_stack = NULL;
static volatile int g_crash_thread_set = 0;
static volatile int g_crash_thread_id = 0;

static void crash_handler(int sig, siginfo_t *info, void *uctx) {
    g_crash_signal = sig;
    g_crash_fault_addr = info->si_addr;

    char buf[256];
    int n = snprintf(buf, sizeof(buf),
        "[CRASH] signal=%d fault_addr=%p stage=%d tid=%d",
        sig, info->si_addr, g_crash_stage, (int)syscall(__NR_gettid));
    raw_write(2, buf, n);
    raw_write(2, "\n", 1);

    /* fp backtrace (aarch64) */
    void *fp;
    __asm__ volatile("mov %0, x29" : "=r"(fp));
    for (int i = 0; i < 8 && fp; i++) {
        void *lr = *((void**)((char*)fp + 8));
        n = snprintf(buf, sizeof(buf), "  #%d fp=%p lr=%p", i, fp, lr);
        raw_write(2, buf, n);
        raw_write(2, "\n", 1);
        fp = *(void**)fp;
    }

    /* Only longjmp if crash is in the test thread that called sigsetjmp */
    int my_tid = (int)syscall(__NR_gettid);
    if (g_crash_thread_set && my_tid == g_crash_thread_id) {
        siglongjmp(g_crash_jmp, sig);
    }

    /* Otherwise: crash in vproc driver thread or other thread — log and exit thread only */
    raw_log("[CRASH] Non-test thread crash, cannot recover. Exiting thread.");
    /* Use pthread_exit to only kill this thread, not the entire process */
    pthread_exit(NULL);
    _exit(128 + sig); /* fallback, shouldn't reach */
}

/* ------------------------------------------------------------------
 * JNI: Basic PTY / fd operations
 * *** CRITICAL: JNI 函数名必须包含完整包名: Java_com_vproc_arttest_TestTermuxSession_xxx ***
 * *** 否则 ART 抛出 UnsatisfiedLinkError（OpenJDK 通过 RegisterNatives 不受影响）***
 * ------------------------------------------------------------------ */

JNIEXPORT jintArray JNICALL Java_com_vproc_arttest_TestTermuxSession_openPty(JNIEnv *env, jobject obj) {
    int master, slave;
    if (openpty(&master, &slave, NULL, NULL, NULL) < 0) {
        return NULL;
    }
    jintArray result = (*env)->NewIntArray(env, 2);
    if (!result) {
        close(master);
        close(slave);
        return NULL;
    }
    jint fds[2] = {master, slave};
    (*env)->SetIntArrayRegion(env, result, 0, 2, fds);
    return result;
}

JNIEXPORT void JNICALL Java_com_vproc_arttest_TestTermuxSession_closeFd(JNIEnv *env, jobject obj, jint fd) {
    (void)env; (void)obj;
    close(fd);
}

JNIEXPORT jint JNICALL Java_com_vproc_arttest_TestTermuxSession_readFd(
    JNIEnv *env, jobject obj, jint fd, jbyteArray buf, jint off, jint len)
{
    (void)obj;
    /* poll with 1s timeout to avoid blocking */
    struct pollfd pfd = { .fd = fd, .events = POLLIN };
    int pret = poll(&pfd, 1, 1000);
    if (pret <= 0) return (jint)pret;

    jbyte *data = (*env)->GetByteArrayElements(env, buf, NULL);
    if (!data) return -1;
    int n = (int)raw_read(fd, data + off, (size_t)len);
    (*env)->ReleaseByteArrayElements(env, buf, data, 0);
    return n;
}

JNIEXPORT jint JNICALL Java_com_vproc_arttest_TestTermuxSession_writeFd(
    JNIEnv *env, jobject obj, jint fd, jbyteArray buf, jint off, jint len)
{
    (void)obj;
    jbyte *data = (*env)->GetByteArrayElements(env, buf, NULL);
    if (!data) return -1;
    int n = (int)raw_write(fd, data + off, (size_t)len);
    (*env)->ReleaseByteArrayElements(env, buf, data, JNI_ABORT);
    return n;
}

/* ------------------------------------------------------------------
 * JNI: Process creation (basic, no crash recovery)
 * ------------------------------------------------------------------ */

JNIEXPORT jint JNICALL Java_com_vproc_arttest_TestTermuxSession_createProcess(
    JNIEnv *env, jobject obj,
    jstring path, jobjectArray argv, jobjectArray envp,
    jint stdin_fd, jint stdout_fd, jint stderr_fd)
{
    (void)obj;

    const char *c_path = (*env)->GetStringUTFChars(env, path, NULL);

    int argc = (*env)->GetArrayLength(env, argv);
    const char **c_argv = malloc((argc + 1) * sizeof(char *));
    for (int i = 0; i < argc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, argv, i);
        c_argv[i] = (*env)->GetStringUTFChars(env, s, NULL);
    }
    c_argv[argc] = NULL;

    int envc = (*env)->GetArrayLength(env, envp);
    const char **c_envp = malloc((envc + 1) * sizeof(char *));
    for (int i = 0; i < envc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, envp, i);
        c_envp[i] = (*env)->GetStringUTFChars(env, s, NULL);
    }
    c_envp[envc] = NULL;

    unsigned int vpid = vproc_create_process(
        c_path, c_argv, c_envp, stdin_fd, stdout_fd, stderr_fd);

    for (int i = 0; i < argc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, argv, i);
        (*env)->ReleaseStringUTFChars(env, s, c_argv[i]);
    }
    free(c_argv);
    for (int i = 0; i < envc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, envp, i);
        (*env)->ReleaseStringUTFChars(env, s, c_envp[i]);
    }
    free(c_envp);
    (*env)->ReleaseStringUTFChars(env, path, c_path);

    return (jint)vpid;
}

JNIEXPORT jint JNICALL Java_com_vproc_arttest_TestTermuxSession_runUntilExit(JNIEnv *env, jobject obj, jint vpid) {
    (void)env; (void)obj;
    return vproc_run_until_exit((unsigned int)vpid);
}

/* ------------------------------------------------------------------
 * JNI: Crash recovery installation
 * ------------------------------------------------------------------ */

JNIEXPORT void JNICALL Java_com_vproc_arttest_TestTermuxSession_installCrashRecovery(JNIEnv *env, jobject obj) {
    (void)env; (void)obj;

    g_crash_stack = malloc(64 * 1024);
    if (!g_crash_stack) {
        raw_log("[jni] sigaltstack alloc failed");
        return;
    }

    stack_t ss = {
        .ss_sp = g_crash_stack,
        .ss_size = 64 * 1024,
        .ss_flags = 0,
    };
    if (sigaltstack(&ss, NULL) != 0) {
        raw_log("[jni] sigaltstack failed");
        return;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = crash_handler;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);

    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);

    raw_log("[jni] crash recovery installed");
}

/* ------------------------------------------------------------------
 * JNI: Process creation with crash recovery
 * Returns int[5]: {vpid, crashed(0/1), fault_addr_low, fault_addr_high, crash_stage}
 *   crash_stage: 0=none, 1=dlopen, 2=create_process, 3=run_until_exit
 * ------------------------------------------------------------------ */

JNIEXPORT jintArray JNICALL Java_com_vproc_arttest_TestTermuxSession_createProcessWithRecovery(
    JNIEnv *env, jobject obj,
    jstring path, jobjectArray argv, jobjectArray envp,
    jint stdin_fd, jint stdout_fd, jint stderr_fd)
{
    int result[5] = {0, 0, 0, 0, 0};

    const char *c_path = (*env)->GetStringUTFChars(env, path, NULL);

    int argc = (*env)->GetArrayLength(env, argv);
    const char **c_argv = malloc((argc + 1) * sizeof(char *));
    for (int i = 0; i < argc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, argv, i);
        c_argv[i] = (*env)->GetStringUTFChars(env, s, NULL);
    }
    c_argv[argc] = NULL;

    int envc = (*env)->GetArrayLength(env, envp);
    const char **c_envp = malloc((envc + 1) * sizeof(char *));
    for (int i = 0; i < envc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, envp, i);
        c_envp[i] = (*env)->GetStringUTFChars(env, s, NULL);
    }
    c_envp[envc] = NULL;

    /* Stage 2: create_process */
    g_crash_stage = 2;
    g_crash_signal = 0;
    g_crash_fault_addr = NULL;
    g_crash_thread_id = (int)syscall(__NR_gettid);
    g_crash_thread_set = 1;

    int jmp_ret = sigsetjmp(g_crash_jmp, 1);
    if (jmp_ret != 0) {
        result[1] = 1;
        result[2] = (int)((uintptr_t)g_crash_fault_addr & 0xFFFFFFFF);
        result[3] = (int)((uintptr_t)g_crash_fault_addr >> 32);
        result[4] = g_crash_stage;
        goto cleanup;
    }

    {
        char logbuf[256];
        snprintf(logbuf, sizeof(logbuf), "[jni] vproc_ffi_create_process(%s, fd=%d/%d/%d)...",
            c_path, stdin_fd, stdout_fd, stderr_fd);
        raw_log(logbuf);
    }

    unsigned int vpid = vproc_create_process(
        c_path, c_argv, c_envp, stdin_fd, stdout_fd, stderr_fd);

    if (vpid == 0) {
        raw_log("[jni] create_process failed (vpid=0)");
        goto cleanup;
    }

    result[0] = (int)vpid;
    g_crash_stage = 0;
    g_crash_thread_set = 0;

cleanup:
    for (int i = 0; i < argc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, argv, i);
        (*env)->ReleaseStringUTFChars(env, s, c_argv[i]);
    }
    free(c_argv);
    for (int i = 0; i < envc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, envp, i);
        (*env)->ReleaseStringUTFChars(env, s, c_envp[i]);
    }
    free(c_envp);
    (*env)->ReleaseStringUTFChars(env, path, c_path);

    jintArray arr = (*env)->NewIntArray(env, 5);
    (*env)->SetIntArrayRegion(env, arr, 0, 5, result);
    return arr;
}

/* ------------------------------------------------------------------
 * JNI: Diagnostics for ART — step-by-step failure isolation
 * ------------------------------------------------------------------ */

/* Diag 1: Path access — canonicalize + read first 64 bytes */
JNIEXPORT jstring JNICALL Java_com_vproc_arttest_TestTermuxSession_diagPathAccess(
    JNIEnv *env, jobject obj, jstring path)
{
    (void)obj;
    const char *c_path = (*env)->GetStringUTFChars(env, path, NULL);
    char buf[1024];

    /* canonicalize */
    char *real = realpath(c_path, NULL);
    if (!real) {
        snprintf(buf, sizeof(buf), "FAIL realpath: errno=%d (%s)", errno, strerror(errno));
        (*env)->ReleaseStringUTFChars(env, path, c_path);
        return (*env)->NewStringUTF(env, buf);
    }

    /* read first 64 bytes */
    int fd = open(real, O_RDONLY);
    if (fd < 0) {
        snprintf(buf, sizeof(buf), "FAIL open(%s): errno=%d (%s)", real, errno, strerror(errno));
        free(real);
        (*env)->ReleaseStringUTFChars(env, path, c_path);
        return (*env)->NewStringUTF(env, buf);
    }
    char rbuf[64];
    ssize_t n = read(fd, rbuf, sizeof(rbuf));
    close(fd);

    /* check ELF magic */
    int is_elf = (n >= 4 && rbuf[0]==0x7f && rbuf[1]=='E' && rbuf[2]=='L' && rbuf[3]=='F');

    snprintf(buf, sizeof(buf), "OK: %s (%zd bytes read, %s)", real, n,
        is_elf ? "ELF" : "NOT ELF");
    free(real);
    (*env)->ReleaseStringUTFChars(env, path, c_path);
    return (*env)->NewStringUTF(env, buf);
}

/* Diag 2: dlopen test — try loading the binary */
JNIEXPORT jstring JNICALL Java_com_vproc_arttest_TestTermuxSession_diagDlopen(
    JNIEnv *env, jobject obj, jstring path)
{
    (void)obj;
    const char *c_path = (*env)->GetStringUTFChars(env, path, NULL);
    char buf[1024];

    void *handle = dlopen(c_path, RTLD_NOW | RTLD_GLOBAL);
    if (!handle) {
        const char *err = dlerror();
        snprintf(buf, sizeof(buf), "FAIL dlopen: %s", err ? err : "unknown");
        (*env)->ReleaseStringUTFChars(env, path, c_path);
        return (*env)->NewStringUTF(env, buf);
    }

    snprintf(buf, sizeof(buf), "OK: handle=%p", handle);
    /* don't dlclose — vproc may need it cached */
    (*env)->ReleaseStringUTFChars(env, path, c_path);
    return (*env)->NewStringUTF(env, buf);
}

/* Diag 3 helper: dl_iterate_phdr callback */
struct diag_dl_data {
    JNIEnv *env;
    jobjectArray *result;
    jclass strClass;
    int count;
    int max_count;
    char **names;
};

static void diag_dl_collect(struct diag_dl_data *d, const char *name) {
    if (d->count < d->max_count) {
        d->names[d->count] = name ? strdup(name) : strdup("(null)");
        d->count++;
    }
}

static int diag_dl_callback(struct dl_phdr_info *info, size_t size, void *data) {
    (void)size;
    struct diag_dl_data *d = (struct diag_dl_data *)data;
    diag_dl_collect(d, info->dlpi_name);
    return 0;
}

/* Diag 3: dl_iterate_phdr — list all loaded shared objects */
JNIEXPORT jobjectArray JNICALL Java_com_vproc_arttest_TestTermuxSession_diagDlIterate(
    JNIEnv *env, jobject obj)
{
    (void)obj;
    enum { MAX_LIBS = 128 };
    char *names[MAX_LIBS];
    struct diag_dl_data data = {
        .env = env,
        .names = names,
        .count = 0,
        .max_count = MAX_LIBS,
    };

    dl_iterate_phdr(diag_dl_callback, &data);

    jclass strClass = (*env)->FindClass(env, "java/lang/String");
    jobjectArray result = (*env)->NewObjectArray(env, data.count, strClass, NULL);
    for (int i = 0; i < data.count; i++) {
        jstring s = (*env)->NewStringUTF(env, names[i]);
        (*env)->SetObjectArrayElement(env, result, i, s);
        (*env)->DeleteLocalRef(env, s);
        free(names[i]);
    }
    return result;
}

/* Diag 4: Full createProcess with stderr capture via pipe */
JNIEXPORT jobjectArray JNICALL Java_com_vproc_arttest_TestTermuxSession_diagCreateProcess(
    JNIEnv *env, jobject obj,
    jstring path, jobjectArray argv, jobjectArray envp,
    jint stdin_fd, jint stdout_fd, jint stderr_fd)
{
    (void)obj;
    char errbuf[4096] = "";
    char vpidbuf[32] = "0";

    const char *c_path = (*env)->GetStringUTFChars(env, path, NULL);

    int argc = (*env)->GetArrayLength(env, argv);
    const char **c_argv = malloc((argc + 1) * sizeof(char *));
    for (int i = 0; i < argc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, argv, i);
        c_argv[i] = (*env)->GetStringUTFChars(env, s, NULL);
    }
    c_argv[argc] = NULL;

    int envc = (*env)->GetArrayLength(env, envp);
    const char **c_envp = malloc((envc + 1) * sizeof(char *));
    for (int i = 0; i < envc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, envp, i);
        c_envp[i] = (*env)->GetStringUTFChars(env, s, NULL);
    }
    c_envp[envc] = NULL;

    /* Capture stderr via pipe */
    int err_pipe[2];
    int saved_stderr = dup(2);
    if (pipe(err_pipe) == 0) {
        /* Make read end non-blocking */
        int flags = fcntl(err_pipe[0], F_GETFL);
        fcntl(err_pipe[0], F_SETFL, flags | O_NONBLOCK);

        dup2(err_pipe[1], 2);
        close(err_pipe[1]);
    }

    unsigned int vpid = vproc_create_process(
        c_path, c_argv, c_envp, stdin_fd, stdout_fd, stderr_fd);

    /* Restore stderr and read captured errors */
    dup2(saved_stderr, 2);
    close(saved_stderr);

    if (err_pipe[0] >= 0) {
        ssize_t n = read(err_pipe[0], errbuf, sizeof(errbuf) - 1);
        if (n > 0) {
            errbuf[n] = '\0';
            /* Strip trailing newline */
            if (n > 0 && errbuf[n-1] == '\n') errbuf[n-1] = '\0';
        } else {
            errbuf[0] = '\0';
        }
        close(err_pipe[0]);
    }

    snprintf(vpidbuf, sizeof(vpidbuf), "%u", vpid);

    /* Cleanup */
    for (int i = 0; i < argc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, argv, i);
        (*env)->ReleaseStringUTFChars(env, s, c_argv[i]);
    }
    free(c_argv);
    for (int i = 0; i < envc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, envp, i);
        (*env)->ReleaseStringUTFChars(env, s, c_envp[i]);
    }
    free(c_envp);
    (*env)->ReleaseStringUTFChars(env, path, c_path);

    /* Return String[]: {vpid, error_msg} */
    jclass strClass = (*env)->FindClass(env, "java/lang/String");
    jobjectArray result = (*env)->NewObjectArray(env, 2, strClass, NULL);
    jstring svpid = (*env)->NewStringUTF(env, vpidbuf);
    jstring serr = (*env)->NewStringUTF(env, errbuf[0] ? errbuf : "(no error captured)");
    (*env)->SetObjectArrayElement(env, result, 0, svpid);
    (*env)->SetObjectArrayElement(env, result, 1, serr);
    (*env)->DeleteLocalRef(env, svpid);
    (*env)->DeleteLocalRef(env, serr);
    return result;
}

/* ------------------------------------------------------------------
 * JNI: Device info detection
 * Returns String[] with CPU, SoC, MTE, Seccomp, Kernel info
 * ------------------------------------------------------------------ */

JNIEXPORT jobjectArray JNICALL Java_com_vproc_arttest_TestTermuxSession_detectDeviceInfo(
    JNIEnv *env, jobject obj)
{
    char buf[512];
    int count = 0;
    char *lines[32];

    #define ADD_LINE(fmt, ...) do { \
        if (count < 32) { \
            int _n = snprintf(buf, sizeof(buf), fmt, ##__VA_ARGS__); \
            lines[count] = strndup(buf, _n); \
            count++; \
        } \
    } while(0)

    /* CPU info -- deduplicate CPU part lines */
    FILE *f = fopen("/proc/cpuinfo", "r");
    if (f) {
        char line[256];
        int saw_hardware = 0;
        int saw_a710 = 0, saw_a715 = 0, saw_x3 = 0;
        while (fgets(line, sizeof(line), f)) {
            if (strstr(line, "Hardware")) {
                char *p = strchr(line, ':');
                ADD_LINE("CPU: %s", p ? p + 2 : "?");
                if (lines[count-1]) {
                    char *nl = strchr(lines[count-1], '\n');
                    if (nl) *nl = 0;
                }
                saw_hardware = 1;
            }
            if (strstr(line, "CPU part") && !saw_hardware) {
                char *p = strchr(line, ':');
                if (p) {
                    int part;
                    sscanf(p + 2, "0x%x", &part);
                    if (part == 0xd81 && !saw_a710) { ADD_LINE("CPU part: 0xd81 (Cortex-A710)"); saw_a710 = 1; }
                    else if (part == 0xd82 && !saw_a715) { ADD_LINE("CPU part: 0xd82 (Cortex-A715)"); saw_a715 = 1; }
                    else if (part == 0xd85 && !saw_x3) { ADD_LINE("CPU part: 0xd85 (Cortex-X3)"); saw_x3 = 1; }
                }
            }
        }
        fclose(f);
    }

    /* MTE detection */
    f = fopen("/proc/self/maps", "r");
    if (f) {
        char line[512];
        int total = 0, tagged = 0;
        while (fgets(line, sizeof(line), f)) {
            total++;
            if (strstr(line, "mt") || strstr(line, "tag")) tagged++;
        }
        fclose(f);
        ADD_LINE("MTE: %s (maps: %d/%d tagged)", tagged > 0 ? "enabled" : "disabled", tagged, total);
    }

    /* Seccomp */
    f = fopen("/proc/self/status", "r");
    if (f) {
        char line[256];
        while (fgets(line, sizeof(line), f)) {
            if (strstr(line, "Seccomp:")) {
                char *p = strchr(line, ':');
                if (p) {
                    int level = atoi(p + 1);
                    ADD_LINE("Seccomp: %d (%s)", level,
                        level == 0 ? "disabled" : level == 1 ? "strict" : "filter");
                }
            }
        }
        fclose(f);
    }

    /* Kernel */
    struct utsname u;
    if (uname(&u) == 0) {
        ADD_LINE("Kernel: %s", u.release);
    }

    #undef ADD_LINE

    jclass strClass = (*env)->FindClass(env, "java/lang/String");
    jobjectArray result = (*env)->NewObjectArray(env, count, strClass, NULL);
    for (int i = 0; i < count; i++) {
        jstring s = (*env)->NewStringUTF(env, lines[i]);
        (*env)->SetObjectArrayElement(env, result, i, s);
        (*env)->DeleteLocalRef(env, s);
        free(lines[i]);
    }
    return result;
}
