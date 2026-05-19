/*
 * libvproc_preload.so — Pure C LD_PRELOAD layer for vproc.
 *
 * Intercepts libc functions and delegates to libvproc.so (Rust runtime)
 * when VPROC=1 is set. Without VPROC=1, all calls pass through to libc.
 *
 * Build: cc -shared -fPIC -o libvproc_preload.so preload.c -ldl
 * Usage: LD_PRELOAD=./libvproc_preload.so VPROC=1 ./program
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <unistd.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <poll.h>

/* ------------------------------------------------------------------ */
/* Runtime function pointers, resolved lazily from libvproc.so        */
/* ------------------------------------------------------------------ */

struct vproc_ffi {
    void       *handle;
    unsigned   (*current_vpid)(void);
    void       (*exit)(int);
    void       (*yield)(void);
    int        (*get_exit_code)(unsigned);
    int        (*vpid_exists)(unsigned);
    int        (*pipe)(unsigned, int *);
    int        (*is_virtual_fd)(unsigned, int);
    ssize_t    (*read)(unsigned, int, void *, size_t);
    ssize_t    (*write)(unsigned, int, const void *, size_t);
    int        (*pipe_is_closed)(unsigned, int);
    int        (*close)(unsigned, int);
    int        (*dup)(unsigned, int);
    int        (*dup2)(unsigned, int, int);
    int        (*execve)(const char *, char *const *, char *const *);
    unsigned   (*fork)(void);
    unsigned   (*getpid)(void);
    unsigned   (*getppid)(void);
};

static struct vproc_ffi g_ffi;
static int g_ffi_loaded;

static const struct vproc_ffi *ffi(void) {
    if (!g_ffi_loaded) {
        g_ffi_loaded = 1;
        /* Try dlopen libvproc.so first (production: host loads runtime .so).
         * Fall back to RTLD_DEFAULT (e2e test: FFI symbols in main binary
         * compiled with -rdynamic). */
        g_ffi.handle = dlopen("libvproc.so", RTLD_NOW | RTLD_GLOBAL);
        void *src = g_ffi.handle ? g_ffi.handle : RTLD_DEFAULT;
#define LOAD(name) g_ffi.name = dlsym(src, "vproc_ffi_" #name)
        LOAD(current_vpid);
        LOAD(exit);
        LOAD(yield);
        LOAD(get_exit_code);
        LOAD(vpid_exists);
        LOAD(pipe);
        LOAD(is_virtual_fd);
        LOAD(read);
        LOAD(write);
        LOAD(pipe_is_closed);
        LOAD(close);
        LOAD(dup);
        LOAD(dup2);
        LOAD(execve);
        LOAD(fork);
        LOAD(getpid);
        LOAD(getppid);
#undef LOAD
        if (!g_ffi.exit) return NULL; /* nothing found */
    }
    return g_ffi.exit ? &g_ffi : NULL;
}

/* ------------------------------------------------------------------ */
/* Helpers                                                            */
/* ------------------------------------------------------------------ */

static int g_enabled = -1;

static int enabled(void) {
    if (g_enabled == -1) {
        const char *v = getenv("VPROC");
        g_enabled = (v && v[0] == '1' && v[1] == '\0') ? 1 : 0;
    }
    return g_enabled;
}

/* Typedefs for real libc function pointers */
typedef void    (*t_void_int)(int);
typedef pid_t   (*t_pid_void)(void);
typedef pid_t   (*t_pid_pid_intp_int)(pid_t, int *, int);
typedef pid_t   (*t_pid_pid_intp_int_rusagep)(pid_t, int *, int, struct rusage *);
typedef int     (*t_int_cstr_carr_carr)(const char *, char *const *, char *const *);
typedef int     (*t_int_intp)(int[2]);
typedef ssize_t (*t_ssize_int_voidp_size)(int, void *, size_t);
typedef ssize_t (*t_ssize_int_cvoidp_size)(int, const void *, size_t);
typedef int     (*t_int_int)(int);
typedef int     (*t_int_int_int)(int, int);
typedef pid_t   (*t_pid_int)(int);
typedef int     (*t_int_int_pid)(pid_t, pid_t);
typedef int     (*t_int_pollfd_nfds_int)(struct pollfd *, nfds_t, int);

#define REAL(type, name)                       \
    static type real_##name;                    \
    if (!real_##name) {                         \
        real_##name = (type)dlsym(RTLD_NEXT, #name); \
    }

/* ------------------------------------------------------------------ */
/* exit() / _exit()                                                   */
/* ------------------------------------------------------------------ */

void exit(int code) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->exit) {
            f->exit(code);
            __builtin_unreachable();
        }
    }
    REAL(t_void_int, exit);
    real_exit(code);
    __builtin_unreachable();
}

void _exit(int code) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->exit) {
            f->exit(code);
            __builtin_unreachable();
        }
    }
    REAL(t_void_int, _exit);
    real__exit(code);
    __builtin_unreachable();
}

/* ------------------------------------------------------------------ */
/* fork() / vfork()                                                   */
/* ------------------------------------------------------------------ */

pid_t fork(void) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->fork) {
            return (pid_t)f->fork();
        }
        errno = ENOSYS;
        return -1;
    }
    REAL(t_pid_void, fork);
    return real_fork();
}

pid_t vfork(void) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->fork) {
            return (pid_t)f->fork();
        }
        errno = ENOSYS;
        return -1;
    }
    REAL(t_pid_void, vfork);
    return real_vfork();
}

/* ------------------------------------------------------------------ */
/* getpid() / getppid()                                               */
/* ------------------------------------------------------------------ */

pid_t getpid(void) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->getpid) return (pid_t)f->getpid();
    }
    REAL(t_pid_void, getpid);
    return real_getpid();
}

pid_t getppid(void) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->getppid) return (pid_t)f->getppid();
    }
    REAL(t_pid_void, getppid);
    return real_getppid();
}

/* ------------------------------------------------------------------ */
/* waitpid() / wait4()                                                */
/* ------------------------------------------------------------------ */

pid_t waitpid(pid_t pid, int *status, int options) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->get_exit_code && f->vpid_exists && f->yield) {
            unsigned vpid = (unsigned)pid;
            for (;;) {
                int code = f->get_exit_code(vpid);
                if (code >= 0) {
                    if (status) *status = (code & 0xff) << 8;
                    return pid;
                }
                if (!f->vpid_exists(vpid)) {
                    errno = ECHILD;
                    return -1;
                }
                f->yield();
            }
        }
    }
    REAL(t_pid_pid_intp_int, waitpid);
    return real_waitpid(pid, status, options);
}

pid_t wait4(pid_t pid, int *status, int options, struct rusage *rusage) {
    if (enabled()) return waitpid(pid, status, options);
    REAL(t_pid_pid_intp_int_rusagep, wait4);
    return real_wait4(pid, status, options, rusage);
}

/* ------------------------------------------------------------------ */
/* execve()                                                           */
/* ------------------------------------------------------------------ */

int execve(const char *pathname, char *const argv[], char *const envp[]) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->execve) return f->execve(pathname, argv, envp);
    }
    REAL(t_int_cstr_carr_carr, execve);
    return real_execve(pathname, argv, envp);
}

/* ------------------------------------------------------------------ */
/* pipe()                                                             */
/* ------------------------------------------------------------------ */

int pipe(int fds[2]) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->current_vpid && f->pipe) {
            unsigned vpid = f->current_vpid();
            if (vpid != 0) return f->pipe(vpid, fds);
        }
    }
    REAL(t_int_intp, pipe);
    return real_pipe(fds);
}

/* ------------------------------------------------------------------ */
/* read() / write()                                                   */
/* ------------------------------------------------------------------ */

ssize_t read(int fd, void *buf, size_t count) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->current_vpid) {
            unsigned vpid = f->current_vpid();
            if (vpid != 0) {
                if (f->is_virtual_fd && f->is_virtual_fd(vpid, fd)) {
                    /* Virtual pipe fd — yield-based read */
                    if (f->read) {
                        for (;;) {
                            ssize_t n = f->read(vpid, fd, buf, count);
                            if (n >= 0) return n;
                            if (f->yield) f->yield();
                        }
                    }
                    errno = EBADF;
                    return -1;
                }
                /* Real fd — poll + yield to avoid blocking the driver thread */
                REAL(t_int_pollfd_nfds_int, poll);
                REAL(t_ssize_int_voidp_size, read);
                for (;;) {
                    struct pollfd pfd = { fd, POLLIN, 0 };
                    int ret = real_poll(&pfd, 1, 0);
                    if (ret < 0 || (pfd.revents & (POLLERR | POLLNVAL)))
                        return real_read(fd, buf, count);
                    if (pfd.revents & POLLIN)
                        return real_read(fd, buf, count);
                    if (f->yield) f->yield();
                }
            }
        }
    }
    REAL(t_ssize_int_voidp_size, read);
    return real_read(fd, buf, count);
}

ssize_t write(int fd, const void *buf, size_t count) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->current_vpid) {
            unsigned vpid = f->current_vpid();
            if (vpid != 0 && f->is_virtual_fd && f->is_virtual_fd(vpid, fd)) {
                if (f->write) {
                    for (;;) {
                        ssize_t n = f->write(vpid, fd, buf, count);
                        if (n >= 0) return n;
                        if (f->pipe_is_closed && f->pipe_is_closed(vpid, fd)) {
                            errno = EPIPE;
                            return -1;
                        }
                        if (f->yield) f->yield();
                    }
                }
                errno = EBADF;
                return -1;
            }
        }
    }
    REAL(t_ssize_int_cvoidp_size, write);
    return real_write(fd, buf, count);
}

/* ------------------------------------------------------------------ */
/* close() / dup() / dup2()                                           */
/* ------------------------------------------------------------------ */

int close(int fd) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->current_vpid) {
            unsigned vpid = f->current_vpid();
            if (vpid != 0 && f->is_virtual_fd && f->is_virtual_fd(vpid, fd)) {
                if (f->close) return f->close(vpid, fd);
            }
        }
    }
    REAL(t_int_int, close);
    return real_close(fd);
}

int dup(int oldfd) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->current_vpid) {
            unsigned vpid = f->current_vpid();
            if (vpid != 0 && f->is_virtual_fd && f->is_virtual_fd(vpid, oldfd)) {
                if (f->dup) return f->dup(vpid, oldfd);
            }
        }
    }
    REAL(t_int_int, dup);
    return real_dup(oldfd);
}

int dup2(int oldfd, int newfd) {
    if (enabled()) {
        const struct vproc_ffi *f = ffi();
        if (f && f->current_vpid) {
            unsigned vpid = f->current_vpid();
            if (vpid != 0 && f->is_virtual_fd && f->is_virtual_fd(vpid, oldfd)) {
                if (f->dup2) return f->dup2(vpid, oldfd, newfd);
            }
        }
    }
    REAL(t_int_int_int, dup2);
    return real_dup2(oldfd, newfd);
}

/* ------------------------------------------------------------------ */
/* kill() / raise() / getpgid() / setpgid()                           */
/* ------------------------------------------------------------------ */

int kill(pid_t pid, int sig) {
    if (enabled()) {
        /* Check if pid is a virtual process */
        if (pid > 0) {
            const struct vproc_ffi *f = ffi();
            if (f && f->vpid_exists && f->vpid_exists((unsigned)pid)) {
                /* Virtual process — signal delivery not yet implemented */
                return 0;
            }
        }
        /* Negative pid (process group) or real pid — pass through */
    }
    REAL(t_int_int_int, kill);
    return real_kill(pid, sig);
}

int raise(int sig) {
    /* raise() sends a signal to the current process — always pass through */
    REAL(t_int_int, raise);
    return real_raise(sig);
}

pid_t getpgid(pid_t pid) {
    if (enabled()) {
        if (pid > 0) {
            const struct vproc_ffi *f = ffi();
            if (f && f->vpid_exists && f->vpid_exists((unsigned)pid)) {
                /* Virtual process — stub: return pid as pgid */
                return pid;
            }
        }
        if (pid == 0) {
            const struct vproc_ffi *f = ffi();
            if (f && f->current_vpid && f->current_vpid() != 0) {
                return (pid_t)f->current_vpid();
            }
        }
    }
    REAL(t_pid_int, getpgid);
    return real_getpgid(pid);
}

int setpgid(pid_t pid, pid_t pgid) {
    if (enabled()) {
        if (pid > 0) {
            const struct vproc_ffi *f = ffi();
            if (f && f->vpid_exists && f->vpid_exists((unsigned)pid)) {
                /* Virtual process — stub: return success */
                return 0;
            }
        }
        if (pid == 0) {
            const struct vproc_ffi *f = ffi();
            if (f && f->current_vpid && f->current_vpid() != 0) {
                return 0;
            }
        }
    }
    REAL(t_int_int_pid, setpgid);
    return real_setpgid(pid, pgid);
}
