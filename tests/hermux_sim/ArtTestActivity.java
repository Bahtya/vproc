package com.vproc.arttest;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import java.io.OutputStream;
import java.io.PrintStream;

/**
 * Direct test launcher — runs tests on a background thread, output to logcat.
 */
public class ArtTestActivity extends Activity {
    static final String TAG = "vproc-arttest";

    static class LogcatStream extends OutputStream {
        StringBuilder buf = new StringBuilder();
        public void write(int b) {
            if (b == 10) {
                if (buf.length() > 0) Log.i(TAG, buf.toString());
                buf.setLength(0);
            } else {
                buf.append((char) b);
            }
        }
        public void flush() {
            if (buf.length() > 0) { Log.i(TAG, buf.toString()); buf.setLength(0); }
        }
    }

    @Override
    public void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        System.setErr(new PrintStream(new LogcatStream()));
        Log.i(TAG, "ArtTestActivity: starting tests");
        try {
            System.loadLibrary("vproc");
            System.loadLibrary("vproc_jni_bridge");
        } catch (Throwable e) {
            Log.e(TAG, "lib load failed", e);
            return;
        }
        new Thread(() -> {
            try {
                TestTermuxSession.ensureLibsLoaded();
                TestTermuxSession.main(new String[]{});
            } catch (Throwable e) {
                Log.e(TAG, "test error", e);
            }
        }, "test-thread").start();
    }
}
