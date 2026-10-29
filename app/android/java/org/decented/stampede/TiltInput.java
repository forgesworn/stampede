package org.decented.stampede;

import android.app.Activity;
import android.content.Context;
import android.hardware.Sensor;
import android.hardware.SensorEvent;
import android.hardware.SensorEventListener;
import android.hardware.SensorManager;
import android.os.SystemClock;
import android.view.Display;
import android.util.Log;

/** Foreground-only motion input. Rust owns calibration and steering curves. */
final class TiltInput implements SensorEventListener {
    private final Display display;
    private final SensorManager manager;
    private final Sensor sensor;
    private boolean wanted, resumed, registered, failed;
    private int generation;
    private long timestamp;
    private final float[] gravity = new float[3];

    TiltInput(Activity activity) {
        display = activity.getWindowManager().getDefaultDisplay();
        manager = (SensorManager) activity.getSystemService(Context.SENSOR_SERVICE);
        Sensor chosen = manager == null ? null : manager.getDefaultSensor(Sensor.TYPE_GRAVITY);
        if (chosen == null && manager != null) chosen = manager.getDefaultSensor(Sensor.TYPE_ACCELEROMETER);
        sensor = chosen;
    }

    synchronized void enable(boolean on) { wanted = on; failed = false; sync(); }
    synchronized void resume() { resumed = true; failed = false; sync(); }
    synchronized void pause() { resumed = false; sync(); }

    private void sync() {
        boolean on = wanted && resumed && sensor != null;
        if (on == registered) return;
        timestamp = 0;
        try {
            if (on) {
                registered = manager.registerListener(this, sensor, 16667);
                failed = !registered;
                generation++;
            } else {
                manager.unregisterListener(this);
                registered = false;
            }
        } catch (RuntimeException error) {
            registered = false;
            failed = true;
            Log.w("Stampede", "Motion sensor unavailable", error);
        }
    }

    synchronized float[] read() {
        int rotation = display.getRotation();
        float age = timestamp == 0 ? 1f : (SystemClock.elapsedRealtimeNanos() - timestamp) * 1e-9f;
        return new float[] { gravity[0], gravity[1], gravity[2], rotation, age, generation,
                sensor == null || failed ? 0f : registered && timestamp != 0 ? 2f : 1f };
    }

    @Override public synchronized void onSensorChanged(SensorEvent event) {
        if (!registered || event.values.length < 3) return;
        float blend = 1f;
        if (sensor.getType() == Sensor.TYPE_ACCELEROMETER && timestamp != 0) {
            double dt = Math.max(0.0, Math.min(0.2, (event.timestamp - timestamp) * 1e-9));
            blend = (float) (1.0 - Math.exp(-dt / 0.055));
        }
        for (int i = 0; i < 3; i++) gravity[i] += (event.values[i] - gravity[i]) * blend;
        timestamp = event.timestamp;
    }

    @Override public void onAccuracyChanged(Sensor sensor, int accuracy) { }
}
