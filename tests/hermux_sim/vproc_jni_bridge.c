/**
 * vproc_jni_bridge.c -- JNI bridge for TestTermuxSession.
 *
 * Matches Hermux's termux.c JNI layer exactly:
 *   - Same function pointer types (6-arg API, no session)
 *   - Same PTY setup (manual /dev/ptmx, not openpty)
 *   - Same signal handling (re-raise for tombstone, no sigaltstack)
 *   - Same dlopen flags (RTLD_NOW, no RTLD_GLOBAL)
 *   - Same chdir before create_process
 *   - Device info detection (CPU, MTE, Seccomp)
 */

#define _GNU_SOURCE
#include <jni.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <stdarg.h>
#include <unistd.h>
#include <fcntl.h>
#include <dlfcn.h>
#include <signal.h>
#include <setjmp.h>
#include <poll.h>
#include <termios.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <link.h>
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdatomic.h>

/* Dynamic __android_log_print via dlsym — no liblog link dependency */
#include <dlfcn.h>
typedef int (*android_log_fn)(int, const char *, const char *, ...);
static android_log_fn get_android_log(void) {
    static android_log_fn fn = NULL;
    if (!fn) {
        void *liblog = dlopen("liblog.so", RTLD_NOW);
        if (liblog) fn = (android_log_fn)dlsym(liblog, "__android_log_print");
    }
    return fn;
}
static void jni_log(const char *fmt, ...) {
    android_log_fn logfn = get_android_log();
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    if (logfn) {
        logfn(4 /* INFO */, "vproc-jni", "%s", buf);  /* ANDROID_LOG_INFO=4 */
    }
}
#define ALOGI(...) jni_log(__VA_ARGS__)
#define ALOGE(...) jni_log(__VA_ARGS__)

/* ------------------------------------------------------------------
 * Raw I/O -- bypasses vproc interceptors
 * Uses libc write/read (not raw syscall) to avoid seccomp issues
 * on Android 16 untrusted_app.
 * ------------------------------------------------------------------ */

#include <unistd.h>

static ssize_t raw_write(int fd, const void *buf, size_t count) {
    return write(fd, buf, count);
}

static ssize_t raw_read(int fd, void *buf, size_t count) {
    return read(fd, buf, count);
}

static void raw_log(const char *msg) {
    raw_write(2, msg, strlen(msg));
    raw_write(2, "\n", 1);
}

/* ------------------------------------------------------------------
 * libvproc FFI — matches Hermux's termux.c exactly
 * Same function pointer types (6-arg, no session_id)
 * ------------------------------------------------------------------ */

typedef uint32_t (*vproc_create_process_fn)(
    const char *path, char *const argv[], char *const envp[],
    int stdin_fd, int stdout_fd, int stderr_fd);
typedef int (*vproc_run_until_exit_fn)(uint32_t vpid);
typedef int (*vproc_vpid_exists_fn)(uint32_t vpid);

static vproc_create_process_fn g_vproc_create_process = NULL;
static vproc_run_until_exit_fn g_vproc_run_until_exit = NULL;
static vproc_vpid_exists_fn    g_vproc_vpid_exists = NULL;
static atomic_int g_vproc_initialized = ATOMIC_VAR_INIT(0);

/* Signal handler — matches Hermux's termux.c exactly */
static struct sigaction g_old_sigsegv, g_old_sigabrt, g_old_sigbus;

static void vproc_crash_signal_handler(int sig, siginfo_t *info, void *uctx) {
    (void)uctx;
    const char *signame = sig == SIGSEGV ? "SIGSEGV" :
                          sig == SIGBUS  ? "SIGBUS"  :
                          sig == SIGABRT ? "SIGABRT" : "UNKNOWN";
    char buf[256];
    int n = snprintf(buf, sizeof(buf),
        "=== NATIVE CRASH in vproc context === signal=%s (%d) si_addr=%p ===",
        signame, sig, info->si_addr);
    raw_write(2, buf, n);
    raw_write(2, "\n", 1);

    /* Re-raise with default handler to get tombstone (includes backtrace) */
    struct sigaction *old = sig == SIGSEGV ? &g_old_sigsegv :
                            sig == SIGBUS  ? &g_old_sigbus  :
                                             &g_old_sigabrt;
    sigaction(sig, old, NULL);
    raise(sig);
}

static void vproc_install_crash_handler(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = vproc_crash_signal_handler;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, &g_old_sigsegv);
    sigaction(SIGABRT, &sa, &g_old_sigabrt);
    sigaction(SIGBUS,  &sa, &g_old_sigbus);
    raw_log("[jni] vproc: native crash signal handler installed (SIGSEGV/SIGABRT/SIGBUS)");
}

/* Matches Hermux's vproc_ensure_loaded() */
static void vproc_ensure_loaded(void) {
    int expected = 0;
    if (!atomic_compare_exchange_strong(&g_vproc_initialized, &expected, 1))
        return;

    raw_log("[jni] vproc: attempting dlopen(\"libvproc.so\", RTLD_NOW)");
    void *lib = dlopen("libvproc.so", RTLD_NOW);
    if (!lib) {
        const char *err = dlerror();
        if (err) { raw_write(2, err, strlen(err)); raw_write(2, "\n", 1); }
        raw_log("[jni] vproc: dlopen failed");
        return;
    }
    raw_log("[jni] vproc: dlopen succeeded, resolving symbols");
    vproc_install_crash_handler();

    /* Use 6-arg compat versions (no session_id) */
    g_vproc_create_process = (vproc_create_process_fn)dlsym(lib, "vproc_ffi_create_process_default");
    g_vproc_run_until_exit = (vproc_run_until_exit_fn)dlsym(lib, "vproc_ffi_run_until_exit_default");
    g_vproc_vpid_exists = (vproc_vpid_exists_fn)dlsym(lib, "vproc_ffi_vpid_exists_default");

    {
        char buf[256];
        snprintf(buf, sizeof(buf),
            "[jni] vproc: create_process=%p, run_until_exit=%p, vpid_exists=%p",
            (void*)g_vproc_create_process, (void*)g_vproc_run_until_exit,
            (void*)g_vproc_vpid_exists);
        raw_log(buf);
    }

    if (!g_vproc_create_process || !g_vproc_run_until_exit || !g_vproc_vpid_exists) {
        const char *err = dlerror();
        if (err) { raw_write(2, err, strlen(err)); raw_write(2, "\n", 1); }
        g_vproc_create_process = NULL;
        g_vproc_run_until_exit = NULL;
        g_vproc_vpid_exists = NULL;
    }
}

/* ------------------------------------------------------------------
 * JNI: Load vproc (for explicit path loading from Java)
 * ------------------------------------------------------------------ */

JNIEXPORT jboolean JNICALL
Java_com_vproc_arttest_TestTermuxSession_nativeLoadVproc(
    JNIEnv *env, jclass cls, jstring jpath)
{
    (void)cls;
    vproc_ensure_loaded();
    return (g_vproc_create_process != NULL) ? JNI_TRUE : JNI_FALSE;
}

/* ------------------------------------------------------------------
 * JNI: createSubprocess — matches Hermux's create_subprocess_vproc exactly
 * Returns ptm fd (>=0) on success, -1 on error.
 * pProcessId receives vpid.
 * ------------------------------------------------------------------ */

JNIEXPORT jint JNICALL Java_com_vproc_arttest_TestTermuxSession_createSubprocess(
    JNIEnv *env, jclass cls,
    jstring cmd, jstring cwd,
    jobjectArray args, jobjectArray envVars,
    jintArray processIdArray,
    jint rows, jint columns, jint cell_width, jint cell_height)
{
    (void)cls;

    ALOGI("createSubprocess: enter");
    vproc_ensure_loaded();
    if (!g_vproc_create_process) {
        ALOGE("createSubprocess: vproc not loaded");
        raw_log("[jni] vproc: not loaded");
        return -1;
    }

    /* Convert Java arrays to C */
    jsize size = args ? (*env)->GetArrayLength(env, args) : 0;
    char **argv = NULL;
    if (size > 0) {
        argv = (char**) malloc((size + 1) * sizeof(char*));
        for (int i = 0; i < size; ++i) {
            jstring s = (jstring)(*env)->GetObjectArrayElement(env, args, i);
            argv[i] = strdup((*env)->GetStringUTFChars(env, s, NULL));
            (*env)->ReleaseStringUTFChars(env, s, argv[i]);
        }
        argv[size] = NULL;
    }

    size = envVars ? (*env)->GetArrayLength(env, envVars) : 0;
    char **envp = NULL;
    if (size > 0) {
        envp = (char**) malloc((size + 1) * sizeof(char *));
        for (int i = 0; i < size; ++i) {
            jstring s = (jstring)(*env)->GetObjectArrayElement(env, envVars, i);
            envp[i] = strdup((*env)->GetStringUTFChars(env, s, 0));
            (*env)->ReleaseStringUTFChars(env, s, envp[i]);
        }
        envp[size] = NULL;
    }

    const char *cmd_utf8 = (*env)->GetStringUTFChars(env, cmd, NULL);
    const char *cwd_utf8 = cwd ? (*env)->GetStringUTFChars(env, cwd, NULL) : NULL;

    /* Count argv and envp for logging */
    int argc = 0, envc = 0;
    if (argv) { for (char *const *p = argv; *p; ++p) ++argc; }
    if (envp) { for (char **p = envp; *p; ++p) ++envc; }
    {
        char buf[256];
        snprintf(buf, sizeof(buf),
            "[jni] vproc: creating virtual process cmd=%s argc=%d envc=%d cwd=%s",
            cmd_utf8, argc, envc, cwd_utf8 ? cwd_utf8 : "(null)");
        raw_log(buf);
    }

    /* PTY setup — matches Hermux exactly */
    int ptm = open("/dev/ptmx", O_RDWR | O_CLOEXEC);
    ALOGI("createSubprocess: ptm=%d errno=%d", ptm, ptm < 0 ? errno : 0);
    if (ptm < 0) {
        char buf[128];
        snprintf(buf, sizeof(buf), "[jni] vproc: open /dev/ptmx failed: %s", strerror(errno));
        raw_log(buf);
        (*env)->ReleaseStringUTFChars(env, cmd, cmd_utf8);
        if (cwd) (*env)->ReleaseStringUTFChars(env, cwd, cwd_utf8);
        return -1;
    }
    {
        char buf[64];
        snprintf(buf, sizeof(buf), "[jni] vproc: ptm fd=%d", ptm);
        raw_log(buf);
    }

    char devname[64];
    if (grantpt(ptm) || unlockpt(ptm) ||
        ptsname_r(ptm, devname, sizeof(devname))) {
        raw_log("[jni] vproc: grantpt/unlockpt/ptsname_r failed");
        close(ptm);
        (*env)->ReleaseStringUTFChars(env, cmd, cmd_utf8);
        if (cwd) (*env)->ReleaseStringUTFChars(env, cwd, cwd_utf8);
        return -1;
    }
    {
        char buf[128];
        snprintf(buf, sizeof(buf), "[jni] vproc: pts device=%s", devname);
        raw_log(buf);
    }

    struct termios tios;
    tcgetattr(ptm, &tios);
    tios.c_iflag |= IUTF8;
    tios.c_iflag &= ~(IXON | IXOFF);
    tcsetattr(ptm, TCSANOW, &tios);

    struct winsize sz = {
        .ws_row = (unsigned short) rows,
        .ws_col = (unsigned short) columns,
        .ws_xpixel = (unsigned short) (columns * cell_width),
        .ws_ypixel = (unsigned short) (rows * cell_height)
    };
    ioctl(ptm, TIOCSWINSZ, &sz);
    {
        char buf[64];
        snprintf(buf, sizeof(buf), "[jni] vproc: winsize %dx%d", columns, rows);
        raw_log(buf);
    }

    /* Open PTY slave */
    int pts = open(devname, O_RDWR);
    ALOGI("createSubprocess: pts=%d (%s) errno=%d", pts, devname, pts < 0 ? errno : 0);
    if (pts < 0) {
        raw_log("[jni] vproc: open PTY slave failed");
        close(ptm);
        (*env)->ReleaseStringUTFChars(env, cmd, cmd_utf8);
        if (cwd) (*env)->ReleaseStringUTFChars(env, cwd, cwd_utf8);
        return -1;
    }
    {
        char buf[64];
        snprintf(buf, sizeof(buf), "[jni] vproc: pts fd=%d", pts);
        raw_log(buf);
    }

    /* chdir before creating coroutine (matches Hermux) */
    if (cwd_utf8 && chdir(cwd_utf8) != 0) {
        char buf[128];
        snprintf(buf, sizeof(buf), "[jni] vproc: chdir(\"%s\") failed: %s",
            cwd_utf8, strerror(errno));
        raw_log(buf);
    }

    /* Create virtual process — matches Hermux's 6-arg call exactly */
    {
        char buf[256];
        snprintf(buf, sizeof(buf),
            "[jni] vproc: calling vproc_ffi_create_process(cmd=%s, stdin=%d, stdout=%d, stderr=%d)",
            cmd_utf8, pts, pts, pts);
        raw_log(buf);
    }
    uint32_t vpid = g_vproc_create_process(cmd_utf8, argv, envp, pts, pts, pts);
    ALOGI("createSubprocess: create_process returned vpid=%u", vpid);
    {
        char buf[64];
        snprintf(buf, sizeof(buf), "[jni] vproc: vproc_ffi_create_process returned vpid=%u", vpid);
        raw_log(buf);
    }

    if (vpid == 0) {
        raw_log("[jni] vproc: create_process returned 0 (failure)");
        close(pts);
        close(ptm);
        (*env)->ReleaseStringUTFChars(env, cmd, cmd_utf8);
        if (cwd) (*env)->ReleaseStringUTFChars(env, cwd, cwd_utf8);
        return -1;
    }

    /* pts stays open — the coroutine's VfdTable references it */
    {
        char buf[128];
        snprintf(buf, sizeof(buf),
            "[jni] vproc: virtual process created successfully vpid=%u ptm=%d pts=%d",
            vpid, ptm, pts);
        raw_log(buf);
    }

    (*env)->ReleaseStringUTFChars(env, cmd, cmd_utf8);
    if (cwd) (*env)->ReleaseStringUTFChars(env, cwd, cwd_utf8);

    if (argv) { for (char **tmp = argv; *tmp; ++tmp) free(*tmp); free(argv); }
    if (envp) { for (char **tmp = envp; *tmp; ++tmp) free(*tmp); free(envp); }

    /* Write vpid to processIdArray */
    int *pProcId = (int*) (*env)->GetPrimitiveArrayCritical(env, processIdArray, NULL);
    if (pProcId) {
        *pProcId = (int) vpid;
        (*env)->ReleasePrimitiveArrayCritical(env, processIdArray, pProcId, 0);
    }

    return ptm;
}

/* ------------------------------------------------------------------
 * JNI: waitFor — matches Hermux's waitFor exactly
 * ------------------------------------------------------------------ */

JNIEXPORT jint JNICALL Java_com_vproc_arttest_TestTermuxSession_waitFor(
    JNIEnv *env, jclass cls, jint pid)
{
    (void)env; (void)cls;
    ALOGI("waitFor: enter vpid=%d", pid);
    vproc_ensure_loaded();
    if (g_vproc_vpid_exists && g_vproc_run_until_exit &&
        g_vproc_vpid_exists((uint32_t)pid)) {
        ALOGI("waitFor: calling run_until_exit vpid=%d", pid);
        int code = g_vproc_run_until_exit((uint32_t)pid);
        ALOGI("waitFor: run_until_exit returned %d", code);
        {
            char buf[64];
            snprintf(buf, sizeof(buf), "[jni] vproc: run_until_exit(vpid=%d) returned %d", pid, code);
            raw_log(buf);
        }
        if (code >= 0) return code;
    }

    /* Fallback to real waitpid */
    int status;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    if (WIFSIGNALED(status)) return -WTERMSIG(status);
    return 0;
}

/* ------------------------------------------------------------------
 * JNI: Basic fd operations (unchanged)
 * ------------------------------------------------------------------ */

JNIEXPORT jintArray JNICALL Java_com_vproc_arttest_TestTermuxSession_openPty(JNIEnv *env, jobject obj) {
    (void)obj;
    int ptm = open("/dev/ptmx", O_RDWR | O_CLOEXEC);
    if (ptm < 0) return NULL;
    char devname[64];
    if (grantpt(ptm) || unlockpt(ptm) || ptsname_r(ptm, devname, sizeof(devname))) {
        close(ptm);
        return NULL;
    }
    int pts = open(devname, O_RDWR);
    if (pts < 0) {
        close(ptm);
        return NULL;
    }
    jintArray result = (*env)->NewIntArray(env, 2);
    if (!result) { close(ptm); close(pts); return NULL; }
    jint fds[2] = {ptm, pts};
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

JNIEXPORT void JNICALL Java_com_vproc_arttest_TestTermuxSession_setPtyWindowSize(
    JNIEnv *env, jclass cls, jint fd, jint rows, jint cols, jint cell_width, jint cell_height)
{
    (void)env; (void)cls;
    struct winsize sz = {
        .ws_row = (unsigned short) rows,
        .ws_col = (unsigned short) cols,
        .ws_xpixel = (unsigned short) (cols * cell_width),
        .ws_ypixel = (unsigned short) (rows * cell_height)
    };
    ioctl(fd, TIOCSWINSZ, &sz);
}

/* ------------------------------------------------------------------
 * JNI: Legacy process creation wrappers (for backward compat)
 * ------------------------------------------------------------------ */

JNIEXPORT jint JNICALL Java_com_vproc_arttest_TestTermuxSession_createProcess(
    JNIEnv *env, jobject obj,
    jstring path, jobjectArray argv, jobjectArray envp,
    jint stdin_fd, jint stdout_fd, jint stderr_fd)
{
    (void)obj;
    vproc_ensure_loaded();
    if (!g_vproc_create_process) return 0;

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

    {
        char buf[256];
        snprintf(buf, sizeof(buf), "[jni] vproc_ffi_create_process(%s, fd=%d/%d/%d)...",
            c_path, stdin_fd, stdout_fd, stderr_fd);
        raw_log(buf);
    }

    uint32_t vpid = g_vproc_create_process(c_path, (char*const*)c_argv, (char*const*)c_envp,
        stdin_fd, stdout_fd, stderr_fd);

    {
        char buf[64];
        snprintf(buf, sizeof(buf), "[jni] create_process returned vpid=%u", vpid);
        raw_log(buf);
    }

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

JNIEXPORT jint JNICALL Java_com_vproc_arttest_TestTermuxSession_runUntilExit(
    JNIEnv *env, jobject obj, jint vpid)
{
    (void)env; (void)obj;
    vproc_ensure_loaded();
    if (!g_vproc_run_until_exit) return -1;
    {
        char buf[64];
        snprintf(buf, sizeof(buf), "[jni] run_until_exit(vpid=%d)...", vpid);
        raw_log(buf);
    }
    int code = g_vproc_run_until_exit((uint32_t)vpid);
    {
        char buf[64];
        snprintf(buf, sizeof(buf), "[jni] run_until_exit returned %d", code);
        raw_log(buf);
    }
    return code;
}

/* ------------------------------------------------------------------
 * JNI: Crash recovery installation (Hermux-style, no sigaltstack)
 * ------------------------------------------------------------------ */

JNIEXPORT void JNICALL Java_com_vproc_arttest_TestTermuxSession_installCrashRecovery(
    JNIEnv *env, jobject obj)
{
    (void)env; (void)obj;
    vproc_install_crash_handler();
}

/* ------------------------------------------------------------------
 * JNI: Process creation with crash recovery (legacy)
 * ------------------------------------------------------------------ */

static sigjmp_buf g_crash_jmp;
static volatile sig_atomic_t g_crash_signal = 0;
static volatile void *g_crash_fault_addr = NULL;
static volatile int g_crash_stage = 0;
static volatile int g_crash_thread_set = 0;
static volatile int g_crash_thread_id = 0;

static struct sigaction g_old_crash_segv, g_old_crash_bus;

static void recovery_handler(int sig, siginfo_t *info, void *uctx) {
    (void)uctx;
    g_crash_signal = sig;
    g_crash_fault_addr = info->si_addr;

    char buf[256];
    int n = snprintf(buf, sizeof(buf),
        "[CRASH] signal=%d fault_addr=%p stage=%d tid=%d",
        sig, info->si_addr, g_crash_stage, (int)syscall(__NR_gettid));
    raw_write(2, buf, n);
    raw_write(2, "\n", 1);

    void *fp;
    __asm__ volatile("mov %0, x29" : "=r"(fp));
    for (int i = 0; i < 8 && fp; i++) {
        void *lr = *((void**)((char*)fp + 8));
        n = snprintf(buf, sizeof(buf), "  #%d fp=%p lr=%p", i, fp, lr);
        raw_write(2, buf, n);
        raw_write(2, "\n", 1);
        fp = *(void**)fp;
    }

    int my_tid = (int)syscall(__NR_gettid);
    if (g_crash_thread_set && my_tid == g_crash_thread_id) {
        __asm__ volatile("xpaclri"); /* Strip PAC from lr before siglongjmp */
        siglongjmp(g_crash_jmp, sig);
    }

    raw_log("[CRASH] Non-test thread crash, cannot recover.");
    pthread_exit(NULL);
    _exit(128 + sig);
}

JNIEXPORT jintArray JNICALL Java_com_vproc_arttest_TestTermuxSession_createProcessWithRecovery(
    JNIEnv *env, jobject obj,
    jstring path, jobjectArray argv, jobjectArray envp,
    jint stdin_fd, jint stdout_fd, jint stderr_fd)
{
    (void)obj;
    int result[5] = {0, 0, 0, 0, 0};

    /* Install recovery handler with sigaltstack for test recovery */
    static void *crash_stack = NULL;
    if (!crash_stack) {
        crash_stack = malloc(64 * 1024);
        if (crash_stack) {
            stack_t ss = { .ss_sp = crash_stack, .ss_size = 64 * 1024, .ss_flags = 0 };
            sigaltstack(&ss, NULL);
        }
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = recovery_handler;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, &g_old_crash_segv);
    sigaction(SIGBUS, &sa, &g_old_crash_bus);

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

    vproc_ensure_loaded();
    if (!g_vproc_create_process) {
        raw_log("[jni] vproc not loaded");
        goto cleanup;
    }

    unsigned int vpid = g_vproc_create_process(c_path, (char*const*)c_argv, (char*const*)c_envp,
        stdin_fd, stdout_fd, stderr_fd);

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

JNIEXPORT jstring JNICALL Java_com_vproc_arttest_TestTermuxSession_diagPathAccess(
    JNIEnv *env, jobject obj, jstring path)
{
    (void)obj;
    const char *c_path = (*env)->GetStringUTFChars(env, path, NULL);
    char buf[1024];

    char *real = realpath(c_path, NULL);
    if (!real) {
        snprintf(buf, sizeof(buf), "FAIL realpath: errno=%d (%s)", errno, strerror(errno));
        (*env)->ReleaseStringUTFChars(env, path, c_path);
        return (*env)->NewStringUTF(env, buf);
    }

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

    int is_elf = (n >= 4 && rbuf[0]==0x7f && rbuf[1]=='E' && rbuf[2]=='L' && rbuf[3]=='F');

    snprintf(buf, sizeof(buf), "OK: %s (%zd bytes read, %s)", real, n,
        is_elf ? "ELF" : "NOT ELF");
    free(real);
    (*env)->ReleaseStringUTFChars(env, path, c_path);
    return (*env)->NewStringUTF(env, buf);
}

JNIEXPORT jstring JNICALL Java_com_vproc_arttest_TestTermuxSession_diagDlopen(
    JNIEnv *env, jobject obj, jstring path)
{
    (void)obj;
    const char *c_path = (*env)->GetStringUTFChars(env, path, NULL);
    char buf[1024];

    void *handle = dlopen(c_path, RTLD_NOW);
    if (!handle) {
        const char *err = dlerror();
        snprintf(buf, sizeof(buf), "FAIL dlopen: %s", err ? err : "unknown");
        (*env)->ReleaseStringUTFChars(env, path, c_path);
        return (*env)->NewStringUTF(env, buf);
    }

    snprintf(buf, sizeof(buf), "OK: handle=%p", handle);
    (*env)->ReleaseStringUTFChars(env, path, c_path);
    return (*env)->NewStringUTF(env, buf);
}

struct diag_dl_data {
    int count;
    int max_count;
    char **names;
};

static int diag_dl_callback(struct dl_phdr_info *info, size_t size, void *data) {
    (void)size;
    struct diag_dl_data *d = (struct diag_dl_data *)data;
    if (d->count < d->max_count) {
        d->names[d->count] = info->dlpi_name ? strdup(info->dlpi_name) : strdup("(null)");
        d->count++;
    }
    return 0;
}

JNIEXPORT jobjectArray JNICALL Java_com_vproc_arttest_TestTermuxSession_diagDlIterate(
    JNIEnv *env, jobject obj)
{
    (void)obj;
    enum { MAX_LIBS = 128 };
    char *names[MAX_LIBS];
    struct diag_dl_data data = {
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

    int err_pipe[2];
    int saved_stderr = dup(2);
    if (pipe(err_pipe) == 0) {
        int flags = fcntl(err_pipe[0], F_GETFL);
        fcntl(err_pipe[0], F_SETFL, flags | O_NONBLOCK);
        dup2(err_pipe[1], 2);
        close(err_pipe[1]);
    }

    vproc_ensure_loaded();
    unsigned int vpid = 0;
    if (g_vproc_create_process) {
        vpid = g_vproc_create_process(c_path, (char*const*)c_argv, (char*const*)c_envp,
            stdin_fd, stdout_fd, stderr_fd);
    }

    dup2(saved_stderr, 2);
    close(saved_stderr);

    if (err_pipe[0] >= 0) {
        ssize_t n = read(err_pipe[0], errbuf, sizeof(errbuf) - 1);
        if (n > 0) {
            errbuf[n] = '\0';
            if (n > 0 && errbuf[n-1] == '\n') errbuf[n-1] = '\0';
        } else {
            errbuf[0] = '\0';
        }
        close(err_pipe[0]);
    }

    snprintf(vpidbuf, sizeof(vpidbuf), "%u", vpid);

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
 * ------------------------------------------------------------------ */

JNIEXPORT jobjectArray JNICALL Java_com_vproc_arttest_TestTermuxSession_detectDeviceInfo(
    JNIEnv *env, jobject obj)
{
    (void)obj;
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
