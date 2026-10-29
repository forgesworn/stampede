package org.decented.stampede;

import android.app.NativeActivity;
import android.content.Intent;
import android.os.Bundle;

/**
 * NativeActivity that keeps the newest intent. Stock NativeActivity drops
 * onNewIntent, so a link opened while the game runs (singleTask) would leave
 * getIntent() on the launch intent. The native side reads getIntent() over JNI
 * (app/src/platform/android.rs) and consumes the link with setData(null).
 *
 * It also hands itself to {@link Updater} (in-app updates and the More games
 * installs need an Activity to show Android's install prompt) and tells it when
 * the game comes back to the front.
 */
public class StampedeActivity extends NativeActivity {
    private TiltInput tilt;
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        Updater.attach(this);
        tilt = new TiltInput(this);
        super.onCreate(savedInstanceState);
    }

    @Override
    protected void onResume() {
        super.onResume();
        tilt.resume();
        Updater.onResume();
    }

    @Override
    protected void onPause() {
        tilt.pause();
        super.onPause();
    }

    public void setTiltEnabled(boolean enabled) {
        runOnUiThread(() -> tilt.enable(enabled));
    }

    public float[] readTilt() { return tilt.read(); }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
    }
}
