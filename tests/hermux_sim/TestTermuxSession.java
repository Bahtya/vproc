package com.vproc.arttest;

import java.io.*;
import java.util.ArrayList;
import java.util.concurrent.*;

/**
 * Matches Hermux's TerminalSession flow exactly:
 *   Java -> JNI -> vproc FFI -> driver thread -> minicoro coroutine -> ELF shell
 *
 * Uses Hermux's exact termux.c integration:
 *   - createSubprocess (returns ptm fd, like Hermux)
 *   - waitFor (matches Hermux's waitFor)
 *   - Persistent InputReader/TermSessionWaiter threads
 *   - Hermux shell paths and environment variables
 *
 * Issue #29: Hermux v0.3.0 crash reproduction on vivo V2419A.
 */
public class TestTermuxSession {

    // Hermux's actual shell paths
    static final String SHELL = "/data/data/com.hermux/files/usr/bin/sh";
    static final String BASH  = "/data/data/com.hermux/files/usr/bin/bash";

    // ART: untrusted_app 无法访问 Termux 数据目录，用 APK 内嵌的 shell
    static String getShellPath() {
        String bundled = getNativeLibDir() + "/libsh.so";
        if (new java.io.File(bundled).canExecute()) return bundled;
        return SHELL;
    }

    static String getBashPath() {
        String bundled = getNativeLibDir() + "/libbash.so";
        if (new java.io.File(bundled).canExecute()) return bundled;
        return BASH;
    }

    static String getNativeLibDir() {
        try {
            // Find the directory where our own JNI library was loaded from
            String classpath = System.getProperty("java.class.path");
            // Find libvproc_jni_bridge.so via /proc/self/maps
            java.io.BufferedReader br = new java.io.BufferedReader(
                new java.io.FileReader("/proc/self/maps"));
            String line;
            while ((line = br.readLine()) != null) {
                if (line.contains("libvproc_jni_bridge.so")) {
                    br.close();
                    // Extract directory from path like: /data/app/~~/.../lib/arm64/libvproc_jni_bridge.so
                    int lastSlash = line.lastIndexOf('/');
                    if (lastSlash > 0) {
                        // Find the start of the path (after the space)
                        String trimmed = line.trim();
                        int pathStart = 0;
                        for (int i = 0; i < trimmed.length(); i++) {
                            if (trimmed.charAt(i) == '/') {
                                pathStart = i;
                                break;
                            }
                        }
                        String path = trimmed.substring(pathStart, lastSlash);
                        return path;
                    }
                }
            }
            br.close();
        } catch (Exception e) {
            System.err.println("  getNativeLibDir failed: " + e);
        }
        return "/data/app/~~/com.vproc.arttest/lib/arm64";
    }

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
        // Resolve vproc FFI symbols — ART's nativeLoader hides symbols from RTLD_DEFAULT,
        // so try explicit dlopen with the library path first, then RTLD_DEFAULT fallback.
        try {
            String vprocPath = getNativeLibDir() + "/libvproc.so";
            if (!new java.io.File(vprocPath).exists()) {
                vprocPath = "/data/data/com.termux/files/home/project/vproc/target/debug/libvproc.so";
            }
            boolean ok = nativeLoadVproc(vprocPath);
            System.err.println("  nativeLoadVproc(" + vprocPath + ") = " + ok);
            if (!ok) {
                System.err.println("  trying RTLD_DEFAULT fallback...");
                ok = nativeLoadVproc(null);
                System.err.println("  nativeLoadVproc(null) = " + ok);
            }
        } catch (Exception e) {
            System.err.println("  WARNING: nativeLoadVproc failed: " + e);
        }
        libsLoaded = true;
    }

    // --- Hermux-matching native methods ---

    // Matches Hermux's JNI.createSubprocess exactly
    native int createSubprocess(String cmd, String cwd,
                                String[] args, String[] envVars,
                                int[] processIdArray,
                                int rows, int columns, int cellWidth, int cellHeight);
    // Matches Hermux's JNI.waitFor
    native int waitFor(int pid);
    native void setPtyWindowSize(int fd, int rows, int cols, int cellWidth, int cellHeight);

    // --- Legacy native methods (for backward compat) ---
    native int[] openPty();
    native void closeFd(int fd);
    native int readFd(int fd, byte[] buf, int off, int len);
    native int writeFd(int fd, byte[] buf, int off, int len);
    native int createProcess(String path, String[] argv, String[] envp,
                             int stdinFd, int stdoutFd, int stderrFd);
    native int runUntilExit(int vpid);
    native void installCrashRecovery();
    native int[] createProcessWithRecovery(String path, String[] argv, String[] envp,
                                           int stdinFd, int stdoutFd, int stderrFd);
    native String[] detectDeviceInfo();
    static native boolean nativeLoadVproc(String path);
    native String diagPathAccess(String path);
    native String diagDlopen(String path);
    native String[] diagDlIterate();
    native String[] diagCreateProcess(String path, String[] argv, String[] envp,
                                       int stdinFd, int stdoutFd, int stderrFd);

    int passed = 0;
    int failed = 0;
    boolean hasCrash = false;

    String[] buildEnvp() {
        ArrayList<String> list = new ArrayList<>();
        for (Object key : System.getenv().keySet().toArray()) {
            String k = (String) key;
            list.add(k + "=" + System.getenv(k));
        }
        // ART: add shell dir to PATH so sh can find commands
        String libDir = getNativeLibDir();
        boolean hasPath = false;
        for (int i = 0; i < list.size(); i++) {
            if (list.get(i).startsWith("PATH=")) {
                list.set(i, "PATH=" + libDir + ":" + list.get(i).substring(5));
                hasPath = true;
                break;
            }
        }
        if (!hasPath) list.add("PATH=" + libDir + ":/system/bin");
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

    /**
     * Run a session matching Hermux's TerminalSession flow exactly:
     *   createSubprocess → InputReader thread → waitFor thread → join
     *
     * This mirrors:
     *   TerminalSession() → JNI.createSubprocess()
     *   TerminalSession$1.run() → processOnStdoutRead() (read from ptm)
     *   TerminalSession$2.run() → JNI.waitFor(pid) (wait for exit)
     */
    Result runHermuxSession(String shellPath, String cmd) throws Exception {
        String cwd = "/data/data/com.hermux/files/home";
        String[] argv = {shellPath, "-c", cmd};
        String[] envp = buildEnvp();

        // Matches TerminalSession.java: JNI.createSubprocess(...)
        int[] pidArr = new int[1];
        int ptm = createSubprocess(shellPath, cwd, argv, envp, pidArr, 24, 80, 0, 0);
        if (ptm < 0) {
            System.err.println("    createSubprocess failed (ptm=" + ptm + ")");
            return new Result(-1, "", false, 0, 0);
        }
        int vpid = pidArr[0];
        System.err.println("    createSubprocess: ptm=" + ptm + " vpid=" + vpid);

        // Matches TerminalSession.java: InputReader thread
        final StringBuilder output = new StringBuilder();
        final byte[] buf = new byte[4096];
        Thread inputReader = new Thread(() -> {
            try {
                while (true) {
                    int n = readFd(ptm, buf, 0, buf.length);
                    if (n <= 0) break;
                    output.append(new String(buf, 0, n, "UTF-8"));
                }
            } catch (Exception e) {
                // Expected when ptm is closed
            }
        }, "InputReader-vpid" + vpid);
        inputReader.setDaemon(true);

        // Matches TerminalSession.java: TermSessionWaiter thread
        final int[] exitCodeHolder = new int[]{-2};
        Thread waiter = new Thread(() -> {
            try {
                int code = waitFor(vpid);
                exitCodeHolder[0] = code;
            } catch (Exception e) {
                System.err.println("    waitFor exception: " + e);
            }
        }, "TermSessionWaiter-vpid" + vpid);
        waiter.setDaemon(true);

        // Start both threads (matches Hermux's process creation flow)
        inputReader.start();
        waiter.start();

        // Wait for process to exit (with timeout)
        waiter.join(10000);
        if (waiter.isAlive()) {
            System.err.println("    TIMEOUT: waitFor did not return in 10s");
            waiter.interrupt();
        }

        // Give inputReader a moment to finish reading
        Thread.sleep(100);
        closeFd(ptm);
        inputReader.join(2000);

        return new Result(exitCodeHolder[0], output.toString(), false, 0, 0);
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

        String sh = getShellPath();
        String[] argv = {sh, "-c", cmd};
        String[] envp = buildEnvp();

        int vpid = createProcess(sh, argv, envp, slaveFd, slaveFd, slaveFd);
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

        String bash = getBashPath();
        String[] argv = {bash, "--norc", "--noprofile", "-c", cmd};
        String[] envp = useLargeEnvp ? buildLargeEnvp() : buildEnvp();

        int[] result = createProcessWithRecovery(bash, argv, envp, slaveFd, slaveFd, slaveFd);

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

    // --- ART Diagnostics ---

    void runDiagnostics() {
        System.err.println("=== ART Diagnostics ===");
        String shellPath = getShellPath();
        String bashPath = getBashPath();
        System.err.println("  shell path: " + shellPath);
        System.err.println("  bash path:  " + bashPath);
        try {
            // 1. Path access
            String pathSh = diagPathAccess(shellPath);
            System.err.println("  diag path(sh):  " + pathSh);
            String pathBash = diagPathAccess(bashPath);
            System.err.println("  diag path(bash): " + pathBash);

            // 2. dlopen
            String dlopenSh = diagDlopen(shellPath);
            System.err.println("  diag dlopen(sh):  " + dlopenSh);
            String dlopenBash = diagDlopen(bashPath);
            System.err.println("  diag dlopen(bash): " + dlopenBash);

            // 3. dl_iterate_phdr
            String[] loaded = diagDlIterate();
            System.err.println("  diag dl_iterate: " + loaded.length + " libs loaded");
            for (String s : loaded) {
                System.err.println("    " + s);
            }

            // 4. Full createProcess with stderr capture
            int[] pty = openPty();
            if (pty != null) {
                String[] argv = {shellPath, "-c", "echo diag"};
                String[] envp = buildEnvp();
                String[] cpResult = diagCreateProcess(shellPath, argv, envp,
                    pty[1], pty[1], pty[1]);
                System.err.println("  diag createProcess(sh): vpid=" + cpResult[0]
                    + " err=" + cpResult[1]);
                closeFd(pty[0]);
                closeFd(pty[1]);
            } else {
                System.err.println("  diag createProcess: openpty failed, skipping");
            }
        } catch (Exception e) {
            System.err.println("  DIAG ERROR: " + e);
        }
        System.err.println("=== End Diagnostics ===");
        System.err.println();
    }

    // --- Hermux flow test cases ---

    void testHermuxShEcho() throws Exception {
        String sh = getShellPath();
        Result r = runHermuxSession(sh, "echo hermux_sh");
        test("hermux flow: sh echo (createSubprocess+waitFor)",
            r.exitCode == 0 && r.output.contains("hermux_sh"));
        if (r.exitCode != 0) {
            System.err.println("    exit=" + r.exitCode + " output=" +
                (r.output.length() > 200 ? r.output.substring(0, 200) + "..." : r.output));
        }
    }

    void testHermuxBashEcho() throws Exception {
        String bash = getBashPath();
        Result r = runHermuxSession(bash, "echo hermux_bash");
        test("hermux flow: bash echo (createSubprocess+waitFor)",
            r.exitCode == 0 && r.output.contains("hermux_bash"));
        if (r.exitCode != 0) {
            System.err.println("    exit=" + r.exitCode + " output=" +
                (r.output.length() > 200 ? r.output.substring(0, 200) + "..." : r.output));
        }
    }

    void testHermuxBashPipe() throws Exception {
        String bash = getBashPath();
        Result r = runHermuxSession(bash, "echo hello_pipe | cat");
        test("hermux flow: bash pipe (createSubprocess+waitFor)",
            r.exitCode == 0 && r.output.contains("hello_pipe"));
        if (r.exitCode != 0) {
            System.err.println("    exit=" + r.exitCode + " output=" +
                (r.output.length() > 200 ? r.output.substring(0, 200) + "..." : r.output));
        }
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
        if (!ok || count != 100) {
            System.err.printf("    exit=%d a_count=%d output_len=%d output=[%s]%n",
                r.exitCode, count, r.output.length(),
                r.output.length() > 200 ? r.output.substring(0, 200) + "..." : r.output);
        }
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
        Result r = runBashCmd(getShellPath() + " -c 'echo fork_test'; true", false);
        if (r.crashed) return;
        boolean ok = r.exitCode == 0 && r.output.contains("fork_test");
        test("bash -c sh echo; true (fork subprocess)", ok);
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

    // --- Timing tests ---

    void testShColdStart() throws Exception {
        TimedResult r = runTimedCmdPty("echo cold_sh");
        boolean ok = r.exitCode == 0 && r.output.contains("cold_sh") && r.elapsedMs < 10000;
        test("sh cold start latency", ok);
        System.err.printf("    %d ms%n", r.elapsedMs);
    }

    void testShWarmStart() throws Exception {
        TimedResult r = runTimedCmdPty("echo warm_sh");
        boolean ok = r.exitCode == 0 && r.output.contains("warm_sh") && r.elapsedMs < 10000;
        test("sh warm start latency (cached)", ok);
        System.err.printf("    %d ms%n", r.elapsedMs);
    }

    void testBashColdStart() throws Exception {
        long start = System.nanoTime();
        Result br = runBashCmd("echo cold_bash", false);
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        boolean ok = br.exitCode == 0 && br.output.contains("cold_bash") && elapsed < 10000;
        test("bash cold start latency", ok);
        System.err.printf("    %d ms%n", elapsed);
    }

    void testBashWarmStart() throws Exception {
        long start = System.nanoTime();
        Result br = runBashCmd("echo warm_bash", false);
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        boolean ok = br.exitCode == 0 && br.output.contains("warm_bash") && elapsed < 10000;
        test("bash warm start latency (cached)", ok);
        System.err.printf("    %d ms%n", elapsed);
    }

    void testLargeOutputThroughput() throws Exception {
        long start = System.nanoTime();
        Result r = runCmdPty("seq 1 1000");
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        int lines = 0;
        for (int i = 0; i < r.output.length(); i++) {
            if (r.output.charAt(i) == '\n') lines++;
        }
        boolean ok = r.exitCode == 0 && lines >= 1000 && elapsed < 10000;
        test("sh large output (seq 1000) throughput", ok);
        System.err.printf("    %d ms, %d lines, %d bytes%n", elapsed, lines, r.output.length());
    }

    void testHermuxSessionLatency() throws Exception {
        String bash = getBashPath();
        long start = System.nanoTime();
        Result r = runHermuxSession(bash, "echo hermux_timed");
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        boolean ok = r.exitCode == 0 && r.output.contains("hermux_timed") && elapsed < 10000;
        test("hermux session round-trip latency", ok);
        System.err.printf("    %d ms%n", elapsed);
    }

    // --- Concurrency tests ---

    void testParallelSh() throws Exception {
        // Sequential hermux sessions — the driver thread is single-threaded,
        // so true parallelism requires separate sessions (not yet supported by JNI bridge).
        int n = 3;
        String[] expected = {"par_sh_0", "par_sh_1", "par_sh_2"};
        long start = System.nanoTime();
        boolean ok = true;
        for (int i = 0; i < n; i++) {
            Result r = runHermuxSession(getShellPath(), "echo " + expected[i]);
            if (r.exitCode != 0 || !r.output.contains(expected[i])) {
                System.err.printf("    session %d: exit=%d output=[%s]%n", i, r.exitCode,
                    r.output.length() > 100 ? r.output.substring(0, 100) + "..." : r.output);
                ok = false;
            }
        }
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        test("3 sequential hermux sh sessions", ok);
        System.err.printf("    total %d ms%n", elapsed);
    }

    void testParallelBash() throws Exception {
        int n = 3;
        String[] expected = {"par_bash_0", "par_bash_1", "par_bash_2"};
        long start = System.nanoTime();
        boolean ok = true;
        for (int i = 0; i < n; i++) {
            Result r = runHermuxSession(getBashPath(), "echo " + expected[i]);
            if (r.exitCode != 0 || !r.output.contains(expected[i])) {
                System.err.printf("    session %d: exit=%d output=[%s]%n", i, r.exitCode,
                    r.output.length() > 100 ? r.output.substring(0, 100) + "..." : r.output);
                ok = false;
            }
        }
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        test("3 sequential hermux bash sessions", ok);
        System.err.printf("    total %d ms%n", elapsed);
    }

    void testMixedConcurrent() throws Exception {
        String[] expected = {"mix_sh", "mix_bash", "mix_sh2", "mix_bash2"};
        long start = System.nanoTime();
        boolean ok = true;
        for (int i = 0; i < 4; i++) {
            String shell = (i % 2 == 0) ? getShellPath() : getBashPath();
            Result r = runHermuxSession(shell, "echo " + expected[i]);
            if (r.exitCode != 0 || !r.output.contains(expected[i])) {
                System.err.printf("    mixed %d: exit=%d output=[%s]%n", i, r.exitCode,
                    r.output.length() > 100 ? r.output.substring(0, 100) + "..." : r.output);
                ok = false;
            }
        }
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        test("4 mixed sh+bash sequential sessions", ok);
        System.err.printf("    total %d ms%n", elapsed);
    }

    // --- Stress tests ---

    void testStressShRapid() throws Exception {
        int iterations = 20;
        int failures = 0;
        long start = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            Result r = runHermuxSession(getShellPath(), "echo stress_" + i);
            if (r.exitCode != 0 || !r.output.contains("stress_" + i)) {
                failures++;
                if (failures <= 3) {
                    System.err.printf("    iter %d: exit=%d output=[%s]%n", i, r.exitCode,
                        r.output.length() > 80 ? r.output.substring(0, 80) + "..." : r.output);
                }
            }
        }
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        boolean ok = failures == 0;
        test("stress: 20 rapid sh sessions", ok);
        System.err.printf("    %d/%d ok, %d ms total, %.0f ms avg%n",
            iterations - failures, iterations, elapsed, (double)elapsed / iterations);
    }

    void testStressBashRapid() throws Exception {
        int iterations = 20;
        int failures = 0;
        long start = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            Result r = runHermuxSession(getBashPath(), "echo bstress_" + i);
            if (r.exitCode != 0 || !r.output.contains("bstress_" + i)) {
                failures++;
                if (failures <= 3) {
                    System.err.printf("    iter %d: exit=%d output=[%s]%n", i, r.exitCode,
                        r.output.length() > 80 ? r.output.substring(0, 80) + "..." : r.output);
                }
            }
        }
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        boolean ok = failures == 0;
        test("stress: 20 rapid bash sessions", ok);
        System.err.printf("    %d/%d ok, %d ms total, %.0f ms avg%n",
            iterations - failures, iterations, elapsed, (double)elapsed / iterations);
    }

    void testStressBashHeavyInit() throws Exception {
        // Each bash invocation does full __libc_init + init file parsing
        // Tests that _start path reinitializes correctly every time
        int iterations = 10;
        int failures = 0;
        long start = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            Result r = runHermuxSession(getBashPath(), "echo heavy_" + i + " $(echo nested_" + i + ")");
            if (r.exitCode != 0 || !r.output.contains("heavy_" + i) || !r.output.contains("nested_" + i)) {
                failures++;
                if (failures <= 3) {
                    System.err.printf("    iter %d: exit=%d output=[%s]%n", i, r.exitCode,
                        r.output.length() > 80 ? r.output.substring(0, 80) + "..." : r.output);
                }
            }
        }
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        boolean ok = failures == 0;
        test("stress: 10 bash heavy init (large envp + subshell)", ok);
        System.err.printf("    %d/%d ok, %d ms total, %.0f ms avg%n",
            iterations - failures, iterations, elapsed, (double)elapsed / iterations);
    }

    void testStressHermuxRapid() throws Exception {
        int iterations = 10;
        int failures = 0;
        long start = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            String bash = getBashPath();
            Result r = runHermuxSession(bash, "echo hermx_" + i);
            if (r.exitCode != 0 || !r.output.contains("hermx_" + i)) {
                failures++;
                if (failures <= 3) {
                    System.err.printf("    iter %d: exit=%d output=[%s]%n", i, r.exitCode,
                        r.output.length() > 80 ? r.output.substring(0, 80) + "..." : r.output);
                }
            }
        }
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        boolean ok = failures == 0;
        test("stress: 10 rapid hermux bash sessions", ok);
        System.err.printf("    %d/%d ok, %d ms total, %.0f ms avg%n",
            iterations - failures, iterations, elapsed, (double)elapsed / iterations);
    }

    void testStressShBashAlternating() throws Exception {
        int iterations = 10;
        int failures = 0;
        long start = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            String cmd = "echo alt_" + (i % 2 == 0 ? "sh" : "bash") + "_" + i;
            String shell = (i % 2 == 0) ? getShellPath() : getBashPath();
            Result r = runHermuxSession(shell, cmd);
            String expected = (i % 2 == 0) ? "alt_sh_" + i : "alt_bash_" + i;
            if (r.exitCode != 0 || !r.output.contains(expected)) {
                failures++;
            }
        }
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        boolean ok = failures == 0;
        test("stress: 10 alternating sh/bash sessions", ok);
        System.err.printf("    %d/%d ok, %d ms total, %.0f ms avg%n",
            iterations - failures, iterations, elapsed, (double)elapsed / iterations);
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

        // ART diagnostics — skip; creates orphan vpid that blocks driver
        // t.runDiagnostics();

        // --- Hermux flow tests (matches termux.c exactly) ---
        System.err.println("=== Hermux Flow Tests (createSubprocess + waitFor) ===");
        t.testHermuxShEcho();
        try {
            t.testHermuxBashEcho();
        } catch (Throwable e) {
            System.err.println("  SKIP: bash test crashed: " + e);
            t.failed++;
        }
        try {
            t.testHermuxBashPipe();
        } catch (Throwable e) {
            System.err.println("  SKIP: bash pipe test crashed: " + e);
            t.failed++;
        }
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

        // Timing tests
        System.err.println("--- Timing Tests ---");
        t.testShColdStart();
        t.testShWarmStart();
        t.testBashColdStart();
        t.testBashWarmStart();
        t.testLargeOutputThroughput();
        t.testHermuxSessionLatency();

        System.err.println();

        // Stress tests (before concurrency — uses hermux sessions, no default session corruption)
        System.err.println("--- Stress Tests ---");
        try {
            t.testStressShRapid();
        } catch (Throwable e) {
            System.err.println("  SKIP: stress sh crashed: " + e);
            t.failed++;
        }
        try {
            t.testStressBashRapid();
        } catch (Throwable e) {
            System.err.println("  SKIP: stress bash crashed: " + e);
            t.failed++;
        }
        try {
            t.testStressBashHeavyInit();
        } catch (Throwable e) {
            System.err.println("  SKIP: stress bash heavy crashed: " + e);
            t.failed++;
        }
        try {
            t.testStressHermuxRapid();
        } catch (Throwable e) {
            System.err.println("  SKIP: stress hermux crashed: " + e);
            t.failed++;
        }
        try {
            t.testStressShBashAlternating();
        } catch (Throwable e) {
            System.err.println("  SKIP: stress alternating crashed: " + e);
            t.failed++;
        }

        System.err.println();

        // Concurrency tests (last — uses separate hermux sessions for parallelism)
        System.err.println("--- Concurrency Tests ---");
        try {
            t.testParallelSh();
        } catch (Throwable e) {
            System.err.println("  SKIP: parallel sh crashed: " + e);
            t.failed++;
        }
        try {
            t.testParallelBash();
        } catch (Throwable e) {
            System.err.println("  SKIP: parallel bash crashed: " + e);
            t.failed++;
        }
        try {
            t.testMixedConcurrent();
        } catch (Throwable e) {
            System.err.println("  SKIP: mixed concurrent crashed: " + e);
            t.failed++;
        }

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

        if (t.failed > 0) {
            System.err.println("(exit code would be 1 — suppressed to keep service alive)");
        }
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

    static class TimedResult extends Result {
        long elapsedMs;
        TimedResult(Result r, long elapsedMs) {
            super(r.exitCode, r.output, r.crashed, r.faultAddr, r.crashStage);
            this.elapsedMs = elapsedMs;
        }
    }

    TimedResult runTimedCmdPty(String cmd) throws Exception {
        long start = System.nanoTime();
        Result r = runCmdPty(cmd);
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        return new TimedResult(r, elapsed);
    }
}
