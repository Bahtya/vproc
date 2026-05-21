package com.vproc.arttest;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import java.io.*;

public class ArtTestActivity extends Activity {

    static final String TAG = "vproc-arttest";
    static final String OUTPUT_DIR = "/sdcard";
    static final String OUTPUT_PATH = OUTPUT_DIR + "/art_test_output.txt";

    private void writeOutput(String msg) {
        Log.i(TAG, msg);
        try {
            new File(OUTPUT_DIR).mkdirs();
            FileWriter fw = new FileWriter(OUTPUT_PATH, true);
            fw.write(msg + "\n");
            fw.close();
        } catch (Exception e) {
            Log.e(TAG, "writeOutput failed", e);
        }
    }

    @Override
    public void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        writeOutput("=== ArtTestActivity onCreate ===");

        new Thread(() -> {
            try {
                writeOutput("Loading libvproc.so...");
                System.loadLibrary("vproc");
                writeOutput("libvproc.so loaded OK");

                writeOutput("Loading libvproc_jni_bridge.so...");
                System.loadLibrary("vproc_jni_bridge");
                writeOutput("libvproc_jni_bridge.so loaded OK");

                writeOutput("Running TestTermuxSession...");
                // Redirect System.err to output file so test results are captured
                PrintStream ps = new PrintStream(new FileOutputStream(OUTPUT_PATH, true));
                System.setErr(ps);
                TestTermuxSession.main(new String[]{});
                writeOutput("Test complete.");

            } catch (Throwable t) {
                writeOutput("FATAL: " + t);
                StringWriter sw = new StringWriter();
                t.printStackTrace(new PrintWriter(sw));
                writeOutput(sw.toString());
            }
            finish();
        }).start();
    }
}
