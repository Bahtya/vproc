package com.vproc.arttest;

import java.io.*;
import java.util.ArrayList;
import java.util.concurrent.*;

/**
 * Simulates Hermux's TerminalSession flow:
 *   Java -> JNI -> vproc FFI -> driver thread -> minicoro coroutine -> ELF shell
 *
 * Tests both sh and bash via PTY, with crash recovery for SIGSEGV detection.
 * Issue #28: Hermux bash session crash reproduction on Cortex-A710/A715/X3.
 *
 * Build: make -f Makefile.java
 * Run:   make -f Makefile.java test
 */
public class TestTermuxSession {

    static final String SHELL = "/data/data/com.termux/files/usr/bin/sh";
    static final String BASH  = "/data/data/com.termux/files/usr/bin/bash";

    static boolean libsLoaded = false;

    static void ensureLibsLoaded() {
        if (libsLoaded) return;
        try {
            System.loadLibrary("vproc");
            System.loadLibrary("vproc_jni_bridge");
        } catch (UnsatisfiedLinkError e) {
            System.load("/data/data/com.termux/files/home/project/vproc/target/debug/libvproc.so");
            System.load("/data/data/com.termux/files/home/project/vproc/tests/hermux_sim/libvproc_jni_bridge.so");
        }
        libsLoaded = true;
    }

    // JNI native methods
    native int[] openPty();
    native void closeFd(int fd);
    native int readFd(int fd, byte[] buf, int off, int len);
    native int writeFd(int fd, byte[] buf, int off, int len);
    native int createProcess(String path, String[] argv, String[] envp,
                             int stdinFd, int stdoutFd, int stderrFd);
    native int runUntilExit(int vpid);
    native void installCrashRecovery();
    // Returns int[5]: {vpid, crashed(0/1), fault_addr_low, fault_addr_high, crash_stage}
    native int[] createProcessWithRecovery(String path, String[] argv, String[] envp,
                                           int stdinFd, int stdoutFd, int stderrFd);
    native String[] detectDeviceInfo();

    int passed = 0;
    int failed = 0;
    boolean hasCrash = false;

    String[] buildEnvp() {
        ArrayList<String> list = new ArrayList<>();
        for (Object key : System.getenv().keySet().toArray()) {
            String k = (String) key;
            list.add(k + "=" + System.getenv(k));
        }
        return list.toArray(new String[0]);
    }

    /** Build large envp simulating Hermux 40+ environment variables */
    String[] buildLargeEnvp() {
        ArrayList<String> list = new ArrayList<>();
        for (Object key : System.getenv().keySet().toArray()) {
            String k = (String) key;
            list.add(k + "=" + System.getenv(k));
        }
        String[] extras = {
            "TERM=xterm-256color",
            "TERM_PROGRAM=hermux",
            "COLORTERM=truecolor",
            "LANG=en_US.UTF-8",
            "LC_ALL=en_US.UTF-8",
            "HISTSIZE=1000",
            "HISTFILESIZE=2000",
            "PROMPT_COMMAND=",
            "SHLVL=1",
            "OLDPWD=/data/data/com.termux/files/home",
            "MAIL=/var/mail/root",
            "LOGNAME=root",
            "HOSTNAME=localhost",
            "TMPDIR=/data/data/com.termux/files/usr/tmp",
            "ANDROID_ROOT=/system",
            "ANDROID_DATA=/data",
            "EXTERNAL_STORAGE=/sdcard",
        };
        for (String e : extras) {
            if (!list.contains(e)) list.add(e);
        }
        return list.toArray(new String[0]);
    }

    void test(String name, boolean condition) {
        if (condition) {
            System.err.printf("  TEST: %-55s PASS%n", name);
            passed++;
        } else {
            System.err.printf("  TEST: %-55s FAIL%n", name);
            failed++;
        }
    }

    // --- sh baseline tests (existing) ---

    Result runCmdPty(String cmd) throws Exception {
        int[] pty = openPty();
        if (pty == null) throw new RuntimeException("openpty failed");
        int masterFd = pty[0], slaveFd = pty[1];

        String[] argv = {SHELL, "-c", cmd};
        String[] envp = buildEnvp();

        int vpid = createProcess(SHELL, argv, envp, slaveFd, slaveFd, slaveFd);
        if (vpid == 0) {
            closeFd(slaveFd); closeFd(masterFd);
            return new Result(-1, "", false, 0, 0);
        }

        StringBuilder output = new StringBuilder();
        byte[] buf = new byte[4096];
        ExecutorService executor = Executors.newFixedThreadPool(2);

        Future<Integer> readFuture = executor.submit(() -> {
            int total = 0;
            while (true) {
                int n = readFd(masterFd, buf, 0, buf.length);
                if (n <= 0) break;
                output.append(new String(buf, 0, n, "UTF-8"));
                total += n;
            }
            return total;
        });

        Future<Integer> exitFuture = executor.submit(() -> runUntilExit(vpid));
        int exitCode;
        try {
            exitCode = exitFuture.get(5, TimeUnit.SECONDS);
        } catch (TimeoutException e) {
            exitFuture.cancel(true);
            exitCode = -2;
        }

        closeFd(slaveFd);
        Thread.sleep(50);
        closeFd(masterFd);

        try { readFuture.get(2, TimeUnit.SECONDS); }
        catch (Exception e) { readFuture.cancel(true); }
        executor.shutdownNow();

        return new Result(exitCode, output.toString(), false, 0, 0);
    }

    // --- bash tests with crash recovery ---

    /**
     * Run bash -c cmd via PTY with crash recovery.
     * Simulates Hermux's full call chain: PTY -> bash -> vproc coroutine.
     */
    Result runBashCmd(String cmd, boolean useLargeEnvp) throws Exception {
        int[] pty = openPty();
        if (pty == null) throw new RuntimeException("openpty failed");
        int masterFd = pty[0], slaveFd = pty[1];

        String[] argv = {BASH, "--norc", "--noprofile", "-c", cmd};
        String[] envp = useLargeEnvp ? buildLargeEnvp() : buildEnvp();

        int[] result = createProcessWithRecovery(BASH, argv, envp, slaveFd, slaveFd, slaveFd);

        if (result[1] == 1) {
            // Crash detected
            closeFd(slaveFd); closeFd(masterFd);
            long faultAddr = ((long)result[3] << 32) | (result[2] & 0xFFFFFFFFL);
            String[] stages = {"none", "dlopen", "create_process", "run_until_exit"};
            String stage = result[4] < stages.length ? stages[result[4]] : "unknown";
            System.err.printf("    CRASH: signal=%d addr=%#x stage=%s%n",
                result[1], faultAddr, stage);
            hasCrash = true;
            failed++;
            return new Result(-11, "", true, faultAddr, result[4]);
        }

        int vpid = result[0];
        if (vpid == 0) {
            closeFd(slaveFd); closeFd(masterFd);
            return new Result(-1, "", false, 0, 0);
        }

        // Read output
        StringBuilder output = new StringBuilder();
        byte[] buf = new byte[4096];
        ExecutorService executor = Executors.newFixedThreadPool(2);

        Future<Integer> readFuture = executor.submit(() -> {
            int total = 0;
            while (true) {
                int n = readFd(masterFd, buf, 0, buf.length);
                if (n <= 0) break;
                output.append(new String(buf, 0, n, "UTF-8"));
                total += n;
            }
            return total;
        });

        Future<Integer> exitFuture = executor.submit(() -> runUntilExit(vpid));
        int exitCode;
        try {
            exitCode = exitFuture.get(5, TimeUnit.SECONDS);
        } catch (TimeoutException e) {
            exitFuture.cancel(true);
            System.err.printf("    TIMEOUT: runUntilExit 5s (vpid=%d)%n", vpid);
            exitCode = -2;
        }

        closeFd(slaveFd);
        Thread.sleep(50);
        closeFd(masterFd);

        try { readFuture.get(2, TimeUnit.SECONDS); }
        catch (Exception e) { readFuture.cancel(true); }
        executor.shutdownNow();

        return new Result(exitCode, output.toString(), false, 0, 0);
    }

    // --- Test cases ---

    void testDeviceInfo() {
        System.err.println("  --- Device Info ---");
        String[] info = detectDeviceInfo();
        for (String line : info) {
            System.err.println("  " + line);
        }
        for (String line : info) {
            if (line.contains("Cortex-A710") || line.contains("A715") || line.contains("X3")) {
                System.err.println("  *** Cortex-A7xx/X3 detected -- may trigger issue #28 ***");
                break;
            }
        }
    }

    // sh baseline tests
    void testEchoHello() throws Exception {
        Result r = runCmdPty("echo hello");
        test("sh -c echo hello (PTY)", r.exitCode == 0 && r.output.contains("hello"));
    }

    void testExitCode() throws Exception {
        Result r = runCmdPty("exit 42");
        test("sh -c exit 42 (PTY)", r.exitCode == 42);
    }

    void testPipeWithCat() throws Exception {
        Result r = runCmdPty("echo hello | cat");
        test("sh -c 'echo hello | cat' (PTY)", r.exitCode == 0 && r.output.contains("hello"));
    }

    void testCommandSubstitution() throws Exception {
        Result r = runCmdPty("echo $(echo nested)");
        test("sh -c echo $(echo nested) (PTY)", r.exitCode == 0 && r.output.contains("nested"));
    }

    void testMultiLineOutput() throws Exception {
        Result r = runCmdPty("for i in 1 2 3; do echo item_$i; done");
        boolean ok = r.exitCode == 0
            && r.output.contains("item_1")
            && r.output.contains("item_2")
            && r.output.contains("item_3");
        test("sh -c for loop (PTY)", ok);
    }

    void testShellVariable() throws Exception {
        Result r = runCmdPty("VAR=x; echo $VAR");
        test("sh -c VAR=x; echo $VAR (PTY)", r.exitCode == 0 && r.output.contains("x"));
    }

    void testSequentialSessions() throws Exception {
        String[] cmds = {"echo one", "echo two", "echo three"};
        boolean ok = true;
        for (int i = 0; i < cmds.length; i++) {
            Result r = runCmdPty(cmds[i]);
            if (r.exitCode != 0 || !r.output.contains(new String[]{"one", "two", "three"}[i])) {
                ok = false;
                System.err.printf("    session %d failed: exit=%d output=[%s]%n", i, r.exitCode, r.output);
                break;
            }
        }
        test("3 sequential sh sessions (PTY)", ok);
    }

    void testLargeData() throws Exception {
        Result r = runCmdPty("yes a | head -100 | tr -d '\\n' | cat");
        boolean ok = r.exitCode == 0;
        int count = 0;
        for (char c : r.output.toCharArray()) {
            if (c == 'a') count++;
        }
        test("sh 100 chars through PTY", ok && count == 100);
    }

    // bash crash recovery tests
    void testBashEcho() throws Exception {
        System.err.println("  --- Bash Tests ---");
        Result r = runBashCmd("echo hello_from_bash", false);
        if (r.crashed) return; // already reported in runBashCmd
        boolean ok = r.exitCode == 0 && r.output.contains("hello_from_bash");
        test("bash -c echo hello (PTY)", ok);
        if (!ok && !r.crashed) {
            System.err.printf("    exit=%d output=[%s]%n", r.exitCode,
                r.output.length() > 200 ? r.output.substring(0, 200) + "..." : r.output);
        }
    }

    void testBashTrue() throws Exception {
        Result r = runBashCmd("true", false);
        if (r.crashed) return;
        test("bash -c true (PTY) [minimal init]", r.exitCode == 0);
    }

    void testBashLargeEnvp() throws Exception {
        Result r = runBashCmd("echo envp_test", true);
        if (r.crashed) return;
        boolean ok = r.exitCode == 0 && r.output.contains("envp_test");
        test("bash -c echo (large envp, 40+ vars)", ok);
        if (!ok) {
            System.err.printf("    exit=%d output=[%s]%n", r.exitCode,
                r.output.length() > 200 ? r.output.substring(0, 200) + "..." : r.output);
        }
    }

    void testBashForkSubprocess() throws Exception {
        // "; true" prevents bash last-command exec optimization
        Result r = runBashCmd("/data/data/com.termux/files/usr/bin/echo fork_test; true", false);
        if (r.crashed) return;
        boolean ok = r.exitCode == 0 && r.output.contains("fork_test");
        test("bash -c /usr/bin/echo; true (fork subprocess)", ok);
        if (!ok) {
            System.err.printf("    exit=%d output=[%s]%n", r.exitCode,
                r.output.length() > 200 ? r.output.substring(0, 200) + "..." : r.output);
        }
    }

    void testBashPipe() throws Exception {
        Result r = runBashCmd("echo hello_pipe | cat", false);
        if (r.crashed) return;
        boolean ok = r.exitCode == 0 && r.output.contains("hello_pipe");
        test("bash -c 'echo hello_pipe | cat' (PTY)", ok);
        if (!ok) {
            System.err.printf("    exit=%d output=[%s]%n", r.exitCode,
                r.output.length() > 300 ? r.output.substring(0, 300) + "..." : r.output);
        }
    }

    void testBashSequential() throws Exception {
        boolean ok = true;
        for (int i = 0; i < 3; i++) {
            Result r = runBashCmd("echo session_" + i, false);
            if (r.crashed) { ok = false; break; }
            if (r.exitCode != 0 || !r.output.contains("session_" + i)) {
                System.err.printf("    bash session %d: exit=%d output=[%s]%n", i, r.exitCode,
                    r.output.length() > 100 ? r.output.substring(0, 100) + "..." : r.output);
                ok = false;
                break;
            }
        }
        test("3 sequential bash -c sessions (PTY)", ok);
    }

    public static void main(String[] args) throws Exception {
        ensureLibsLoaded();
        TestTermuxSession t = new TestTermuxSession();

        System.err.println("============================================================");
        System.err.println("vproc PTY Session Test (Hermux Simulation)");
        System.err.println("Java -> JNI -> vproc FFI -> PTY -> sh/bash");
        System.err.println("============================================================");
        System.err.println();

        // Install crash recovery
        t.installCrashRecovery();
        System.err.println("[setup] crash recovery installed");
        System.err.println();

        // Device detection
        t.testDeviceInfo();
        System.err.println();

        // sh baseline tests
        System.err.println("--- sh baseline ---");
        t.testEchoHello();
        t.testExitCode();
        t.testPipeWithCat();
        t.testCommandSubstitution();
        t.testMultiLineOutput();
        t.testShellVariable();
        t.testLargeData();
        t.testSequentialSessions();

        System.err.println();

        // bash crash recovery tests (issue #28)
        t.testBashEcho();
        t.testBashTrue();
        t.testBashLargeEnvp();
        t.testBashForkSubprocess();
        t.testBashPipe();
        t.testBashSequential();

        System.err.println();

        // Summary
        System.err.println("============================================================");
        System.err.printf("Results: %d passed, %d failed%n", t.passed, t.failed);
        if (t.hasCrash) {
            System.err.println("*** SIGSEGV detected -- issue #28 reproduced! ***");
        } else if (t.failed > 0) {
            System.err.println("Some tests failed (non-crash)");
        } else {
            System.err.println("All passed -- no crash on this device");
        }
        System.err.println("============================================================");

        if (t.failed > 0) System.exit(1);
    }

    static class Result {
        int exitCode;
        String output;
        boolean crashed;
        long faultAddr;
        int crashStage;
        Result(int code, String out, boolean crashed, long faultAddr, int crashStage) {
            this.exitCode = code;
            this.output = out;
            this.crashed = crashed;
            this.faultAddr = faultAddr;
            this.crashStage = crashStage;
        }
    }
}
