package com.vproc.arttest;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.graphics.Color;
import android.graphics.Typeface;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;
import java.io.BufferedReader;
import java.io.File;
import java.io.FileReader;
import java.io.FileWriter;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.PrintStream;

/**
 * ART test launcher with terminal-style UI.
 * Shows test output + logcat, one-click copy to clipboard.
 * Persists crash logs to file for display after process restart.
 */
public class ArtTestActivity extends Activity {
    static final String TAG = "vproc-arttest";
    static final String CRASH_FILE = "crash_log.txt";

    private TextView outputView;
    private ScrollView scrollView;
    private final StringBuilder allOutput = new StringBuilder();
    private final Handler uiHandler = new Handler(Looper.getMainLooper());
    private volatile boolean running = false;

    /** Stream that writes to both logcat and the UI TextView. */
    class UiStream extends OutputStream {
        StringBuilder buf = new StringBuilder();

        public void write(int b) {
            if (b == 10) {
                final String line = buf.toString();
                buf.setLength(0);
                Log.i(TAG, line);
                appendOutput(line);
            } else {
                buf.append((char) b);
            }
        }

        public void flush() {
            if (buf.length() > 0) {
                final String line = buf.toString();
                buf.setLength(0);
                Log.i(TAG, line);
                appendOutput(line);
            }
        }
    }

    private void appendOutput(String line) {
        uiHandler.post(() -> {
            outputView.append(line + "\n");
            scrollView.post(() -> scrollView.fullScroll(ScrollView.FOCUS_DOWN));
        });
        synchronized (allOutput) {
            allOutput.append(line).append("\n");
        }
    }

    @Override
    public void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        // --- Build UI programmatically ---
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(8, 8, 8, 8);

        // Button bar
        LinearLayout buttons = new LinearLayout(this);
        buttons.setOrientation(LinearLayout.HORIZONTAL);
        buttons.setPadding(0, 0, 0, 8);

        Button btnRun = new Button(this);
        btnRun.setText("Run");
        btnRun.setAllCaps(false);
        btnRun.setOnClickListener(v -> runTests());
        buttons.addView(btnRun);

        Button btnCopy = new Button(this);
        btnCopy.setText("Copy");
        btnCopy.setAllCaps(false);
        btnCopy.setOnClickListener(v -> copyOutput());
        buttons.addView(btnCopy);

        Button btnLog = new Button(this);
        btnLog.setText("Logcat");
        btnLog.setAllCaps(false);
        btnLog.setOnClickListener(v -> showLogcat());
        buttons.addView(btnLog);

        Button btnClear = new Button(this);
        btnClear.setText("Clear");
        btnClear.setAllCaps(false);
        btnClear.setOnClickListener(v -> clearOutput());
        buttons.addView(btnClear);

        root.addView(buttons);

        // Output area — terminal style
        scrollView = new ScrollView(this);
        scrollView.setFillViewport(true);

        outputView = new TextView(this);
        outputView.setTypeface(Typeface.MONOSPACE);
        outputView.setTextSize(11);
        outputView.setTextColor(0xFF00FF00);
        outputView.setBackgroundColor(Color.BLACK);
        outputView.setPadding(8, 8, 8, 8);
        outputView.setLineSpacing(2, 1);
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0);
        lp.weight = 1;
        scrollView.addView(outputView);
        root.addView(scrollView, lp);

        setContentView(root);

        // Load native libraries
        try {
            System.loadLibrary("vproc");
            System.loadLibrary("vproc_jni_bridge");
        } catch (Throwable e) {
            appendOutput("FATAL: lib load failed: " + e.getMessage());
            return;
        }

        // Redirect stderr to UI
        System.setErr(new PrintStream(new UiStream()));

        appendOutput("=== vproc ART Test ===");
        appendOutput("Device: " + android.os.Build.MODEL + " / Android " + android.os.Build.VERSION.SDK_INT);
        appendOutput("");

        // Show crash log from previous run if exists
        showPreviousCrash();
    }

    private void showPreviousCrash() {
        try {
            File f = new File(getFilesDir(), CRASH_FILE);
            if (!f.exists()) return;
            BufferedReader br = new BufferedReader(new FileReader(f));
            StringBuilder sb = new StringBuilder();
            String line;
            while ((line = br.readLine()) != null) {
                sb.append(line).append("\n");
            }
            br.close();
            if (sb.length() > 0) {
                appendOutput("!!! PREVIOUS CRASH (process was killed) !!!");
                appendOutput(sb.toString().trim());
                appendOutput("!!! END PREVIOUS CRASH !!!");
                appendOutput("");
            }
            // Delete after showing
            f.delete();
        } catch (Exception e) {
            // ignore
        }
    }

    /** Persist crash log to file before process dies. */
    static void saveCrashLog(String info) {
        try {
            // Write to a known location accessible from next launch
            File dir = new File("/data/data/com.vproc.arttest/files");
            dir.mkdirs();
            File f = new File(dir, CRASH_FILE);
            FileWriter fw = new FileWriter(f, true);
            fw.write(info);
            fw.write("\n");
            fw.close();
        } catch (Exception e) {
            // Last resort — can't do much
        }
    }

    private void runTests() {
        if (running) {
            Toast.makeText(this, "Tests already running", Toast.LENGTH_SHORT).show();
            return;
        }
        running = true;

        // Clear old crash log before new test run
        new File(getFilesDir(), CRASH_FILE).delete();

        appendOutput("--- Running tests ---");

        new Thread(() -> {
            try {
                TestTermuxSession.ensureLibsLoaded();
                TestTermuxSession.main(new String[]{});
            } catch (Throwable e) {
                appendOutput("TEST ERROR: " + e);
                for (StackTraceElement f : e.getStackTrace()) {
                    appendOutput("  at " + f);
                }
                saveCrashLog("Java exception: " + e);
            } finally {
                appendOutput("--- Tests complete ---");
                running = false;
                // Auto-capture logcat after tests finish
                captureLogcatQuiet();
            }
        }, "test-thread").start();
    }

    /** Capture logcat silently after test run, store for crash recovery. */
    private void captureLogcatQuiet() {
        try {
            Process p = Runtime.getRuntime().exec(new String[]{
                "logcat", "-d", "-t", "200",
                "-s", "vproc-arttest:*", "vproc:*", "vproc-jni:*", "DEBUG:*", "AndroidRuntime:*"
            });
            BufferedReader br = new BufferedReader(new InputStreamReader(p.getInputStream()));
            StringBuilder sb = new StringBuilder();
            String line;
            while ((line = br.readLine()) != null) {
                sb.append(line).append("\n");
            }
            br.close();
            p.waitFor();
            // Save for crash recovery
            if (sb.length() > 0) {
                saveCrashLog("=== Logcat at test end ===\n" + sb.toString());
            }
        } catch (Exception e) {
            // ignore
        }
    }

    private void copyOutput() {
        String text;
        synchronized (allOutput) {
            text = allOutput.toString();
        }
        if (text.isEmpty()) {
            Toast.makeText(this, "Nothing to copy", Toast.LENGTH_SHORT).show();
            return;
        }
        ClipboardManager clipboard = (ClipboardManager) getSystemService(CLIPBOARD_SERVICE);
        clipboard.setPrimaryClip(ClipData.newPlainText("vproc-test", text));
        Toast.makeText(this, "Copied " + text.length() + " chars", Toast.LENGTH_SHORT).show();
    }

    private void showLogcat() {
        appendOutput("--- Logcat (last 300 lines) ---");
        new Thread(() -> {
            try {
                Process p = Runtime.getRuntime().exec(new String[]{
                    "logcat", "-d", "-t", "300",
                    "-s", "vproc-arttest:*", "vproc:*", "vproc-jni:*", "DEBUG:*", "AndroidRuntime:*",
                    "libc:*", "signal:*"
                });
                BufferedReader br = new BufferedReader(new InputStreamReader(p.getInputStream()));
                String line;
                while ((line = br.readLine()) != null) {
                    appendOutput(line);
                }
                br.close();
                p.waitFor();
                appendOutput("--- End logcat ---");
            } catch (Exception e) {
                appendOutput("logcat error: " + e.getMessage());
            }
        }, "logcat-thread").start();
    }

    private void clearOutput() {
        outputView.setText("");
        synchronized (allOutput) {
            allOutput.setLength(0);
        }
    }
}
