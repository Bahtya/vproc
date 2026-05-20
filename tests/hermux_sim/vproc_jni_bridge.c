/**
 * vproc_jni_bridge.c — JNI native method implementations for TestTermuxSession.
 *
 * Thin wrappers around libvproc FFI functions + openpty for PTY creation.
 * Simulates the exact calling pattern used by Hermux's Java TerminalSession.
 */

#define _GNU_SOURCE
#include <jni.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <pty.h>

/* Ensure VPROC=1 is set on library load — required for GOT-patched interceptors. */
__attribute__((constructor))
static void ensure_vproc_env(void) {
    setenv("VPROC", "1", 1);
}

/* libvproc FFI functions */
extern unsigned int vproc_ffi_create_process(
    const char *path, const char *const *argv, const char *const *envp,
    int stdin_fd, int stdout_fd, int stderr_fd);
extern int vproc_ffi_run_until_exit(unsigned int vpid);

/* Create a PTY pair. Returns int[2] = {master_fd, slave_fd}. */
JNIEXPORT jintArray JNICALL Java_TestTermuxSession_openPty(JNIEnv *env, jobject obj) {
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

/* Close a file descriptor. */
JNIEXPORT void JNICALL Java_TestTermuxSession_closeFd(JNIEnv *env, jobject obj, jint fd) {
    (void)env; (void)obj;
    close(fd);
}

/* Read up to len bytes from fd into byte[]. Returns number of bytes read, or -1 on error. */
JNIEXPORT jint JNICALL Java_TestTermuxSession_readFd(JNIEnv *env, jobject obj, jint fd, jbyteArray buf, jint off, jint len) {
    (void)obj;
    jbyte *data = (*env)->GetByteArrayElements(env, buf, NULL);
    if (!data) return -1;
    int n = read(fd, data + off, len);
    (*env)->ReleaseByteArrayElements(env, buf, data, 0);
    return n;
}

/* Create a virtual process via vproc FFI. Returns vpid (>0 on success, 0 on error). */
JNIEXPORT jint JNICALL Java_TestTermuxSession_createProcess(
    JNIEnv *env, jobject obj,
    jstring path, jobjectArray argv, jobjectArray envp,
    jint stdin_fd, jint stdout_fd, jint stderr_fd)
{
    (void)obj;

    const char *c_path = (*env)->GetStringUTFChars(env, path, NULL);

    /* Build argv C array */
    int argc = (*env)->GetArrayLength(env, argv);
    const char **c_argv = malloc((argc + 1) * sizeof(char *));
    for (int i = 0; i < argc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, argv, i);
        c_argv[i] = (*env)->GetStringUTFChars(env, s, NULL);
    }
    c_argv[argc] = NULL;

    /* Build envp C array */
    int envc = (*env)->GetArrayLength(env, envp);
    const char **c_envp = malloc((envc + 1) * sizeof(char *));
    for (int i = 0; i < envc; i++) {
        jstring s = (jstring)(*env)->GetObjectArrayElement(env, envp, i);
        c_envp[i] = (*env)->GetStringUTFChars(env, s, NULL);
    }
    c_envp[envc] = NULL;

    unsigned int vpid = vproc_ffi_create_process(
        c_path, c_argv, c_envp, stdin_fd, stdout_fd, stderr_fd);

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

    return (jint)vpid;
}

/* Block until the virtual process exits. Returns exit code. */
JNIEXPORT jint JNICALL Java_TestTermuxSession_runUntilExit(JNIEnv *env, jobject obj, jint vpid) {
    (void)env; (void)obj;
    return vproc_ffi_run_until_exit((unsigned int)vpid);
}
