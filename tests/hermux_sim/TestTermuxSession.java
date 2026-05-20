import java.io.*;
import java.util.ArrayList;
import java.util.concurrent.*;

/**
 * Simulates Hermux's TerminalSession flow:
 *   Java -> JNI -> vproc FFI -> driver thread -> minicoro coroutine -> ELF shell
 *
 * Uses PTY (not pipes) to match real terminal behavior:
 *   - Terminal line discipline echo
 *   - PTY slave fd passed to vproc_ffi_create_process
 *   - Master fd read for output capture
 *
 * Build: make
 * Run:   make test
 */
public class TestTermuxSession {

    static final String SHELL = "/data/data/com.termux/files/usr/bin/sh";

    static {
        System.loadLibrary("vproc");
        System.loadLibrary("vproc_jni_bridge");
    }

    // JNI native methods
    native int[] openPty();
    native void closeFd(int fd);
    native int readFd(int fd, byte[] buf, int off, int len);
    native int createProcess(String path, String[] argv, String[] envp,
                             int stdinFd, int stdoutFd, int stderrFd);
    native int runUntilExit(int vpid);

    int passed = 0;
    int failed = 0;

    String[] buildEnvp() {
        ArrayList<String> list = new ArrayList<>();
        for (Object key : System.getenv().keySet().toArray()) {
            String k = (String) key;
            list.add(k + "=" + System.getenv(k));
        }
        return list.toArray(new String[0]);
    }

    /**
     * Run a command via PTY, return (exitCode, output).
     * Simulates: openpty -> createProcess(slaveFd for 0/1/2) -> runUntilExit -> read master.
     */
    Result runCmdPty(String cmd) throws Exception {
        int[] pty = openPty();
        if (pty == null) throw new RuntimeException("openpty failed");

        int masterFd = pty[0];
        int slaveFd = pty[1];

        String[] argv = {SHELL, "-c", cmd};
        String[] envp = buildEnvp();

        int vpid = createProcess(SHELL, argv, envp, slaveFd, slaveFd, slaveFd);

        if (vpid == 0) {
            closeFd(slaveFd);
            closeFd(masterFd);
            return new Result(-1, "");
        }

        // Read output from master in a separate thread (with timeout)
        StringBuilder output = new StringBuilder();
        byte[] buf = new byte[4096];
        ExecutorService executor = Executors.newSingleThreadExecutor();
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

        // Wait for process to exit
        int exitCode = runUntilExit(vpid);

        // Close slave AFTER process exits — vfd table stores the raw fd number,
        // so closing it before the process runs would invalidate the fd.
        closeFd(slaveFd);

        // Close master to unblock reader thread
        Thread.sleep(50);
        closeFd(masterFd);

        try {
            readFuture.get(5, TimeUnit.SECONDS);
        } catch (TimeoutException e) {
            readFuture.cancel(true);
        }
        executor.shutdown();

        return new Result(exitCode, output.toString());
    }

    void test(String name, boolean condition) {
        if (condition) {
            System.err.printf("  TEST: %-50s PASS%n", name);
            passed++;
        } else {
            System.err.printf("  TEST: %-50s FAIL%n", name);
            failed++;
        }
    }

    void testEchoHello() throws Exception {
        Result r = runCmdPty("echo hello");
        test("echo hello (PTY)", r.exitCode == 0 && r.output.contains("hello"));
    }

    void testPipeWithCat() throws Exception {
        Result r = runCmdPty("echo hello | cat");
        test("echo hello | cat (PTY)", r.exitCode == 0 && r.output.contains("hello"));
    }

    void testCommandSubstitution() throws Exception {
        Result r = runCmdPty("echo $(echo nested)");
        test("echo $(echo nested) (PTY)", r.exitCode == 0 && r.output.contains("nested"));
    }

    void testExitCode() throws Exception {
        Result r = runCmdPty("exit 42");
        test("exit 42 (PTY)", r.exitCode == 42);
    }

    void testMultiLineOutput() throws Exception {
        Result r = runCmdPty("for i in 1 2 3; do echo item_$i; done");
        boolean ok = r.exitCode == 0
            && r.output.contains("item_1")
            && r.output.contains("item_2")
            && r.output.contains("item_3");
        test("for loop multi-line (PTY)", ok);
    }

    void testShellVariable() throws Exception {
        Result r = runCmdPty("VAR=x; echo $VAR");
        test("VAR=x; echo $VAR (PTY)", r.exitCode == 0 && r.output.contains("x"));
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
        test("3 sequential sessions (PTY)", ok);
    }

    void testLargeData() throws Exception {
        Result r = runCmdPty("yes a | head -100 | tr -d '\\n' | cat");
        boolean ok = r.exitCode == 0;
        // Count 'a' characters in output (PTY may add echo/cr)
        int count = 0;
        for (char c : r.output.toCharArray()) {
            if (c == 'a') count++;
        }
        test("100 chars through PTY", ok && count == 100);
    }

    public static void main(String[] args) throws Exception {
        TestTermuxSession t = new TestTermuxSession();

        System.err.println("=== Java PTY Session Test (Hermux Simulation) ===");
        System.err.println();

        System.err.println("--- Basic commands ---");
        t.testEchoHello();
        t.testExitCode();

        System.err.println();
        System.err.println("--- Pipes ---");
        t.testPipeWithCat();

        System.err.println();
        System.err.println("--- Shell features ---");
        t.testCommandSubstitution();
        t.testMultiLineOutput();
        t.testShellVariable();
        t.testLargeData();

        System.err.println();
        System.err.println("--- Sequential sessions ---");
        t.testSequentialSessions();

        System.err.println();
        System.err.printf("--- Results: %d passed, %d failed ---%n", t.passed, t.failed);

        if (t.failed > 0) {
            System.exit(1);
        }
    }

    static class Result {
        int exitCode;
        String output;
        Result(int code, String out) { exitCode = code; output = out; }
    }
}
