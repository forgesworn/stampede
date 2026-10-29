package org.decented.stampede;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;

/**
 * A foreground service that exists only while the player is in an online room
 * (lobby, race or results). It does no work: its job is to make Android treat
 * the process as in use, so the network and the CPU stay available when the
 * player switches app for a few seconds. Without it newer Android versions cut
 * a backgrounded app's sockets within seconds, and the room (above all one
 * hosted from this phone) drops. Started and stopped from Rust
 * (app/src/platform/android.rs, `set_room_service`). The notification is silent
 * (low importance, no sound or vibration).
 */
public class RoomService extends Service {
    private static final String CHANNEL = "stampede-room";
    private static final int ID = 7715;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        NotificationManager nm = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
        Notification.Builder b;
        if (Build.VERSION.SDK_INT >= 26) {
            if (nm != null) {
                NotificationChannel ch = new NotificationChannel(CHANNEL, "Online game", NotificationManager.IMPORTANCE_LOW);
                ch.setSound(null, null);
                ch.enableVibration(false);
                nm.createNotificationChannel(ch);
            }
            b = new Notification.Builder(this, CHANNEL);
        } else {
            b = new Notification.Builder(this);
        }
        b.setContentTitle("Stampede")
                .setContentText("You are in an online game")
                .setSmallIcon(getApplicationInfo().icon)
                .setOngoing(true);
        Intent open = getPackageManager().getLaunchIntentForPackage(getPackageName());
        if (open != null) {
            int fl = Build.VERSION.SDK_INT >= 23 ? PendingIntent.FLAG_IMMUTABLE : 0;
            b.setContentIntent(PendingIntent.getActivity(this, 0, open, fl));
        }
        Notification n = b.build();
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC);
        } else {
            startForeground(ID, n);
        }
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
