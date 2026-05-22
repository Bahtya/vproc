package com.vproc.arttest;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.util.Log;

/**
 * Trivial launcher — starts HttpServerService and finishes.
 * HTTP server runs in the foreground service, not here.
 */
public class ArtTestActivity extends Activity {
    static final String TAG = "vproc-arttest";

    @Override
    public void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "ArtTestActivity onCreate — starting HttpServerService");
        Intent intent = new Intent(this, HttpServerService.class);
        startForegroundService(intent);
        finish();
    }
}
