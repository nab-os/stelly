package dev.dioxus.main

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.drawable.Icon
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import java.net.URL

typealias BuildConfig = fr.glargh.twokhz.BuildConfig

class MainActivity : WryActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // Before super, which starts the Rust side.
        Playback.context = applicationContext
        super.onCreate(savedInstanceState)
    }
}

// The lock screen's and the shade's "now playing", for whatever this device's
// <audio> element holds. Its service is also what keeps the process running
// once the screen locks: without one, Android freezes the app and the sound
// stops with it.
//
// Rust calls `show` and `hide` from its own thread, see src/ui/now_playing.rs.
// Everything else happens on the main one.
object Playback {
    const val CHANNEL = "playback"
    const val ID = 1

    lateinit var context: Context
    private val main = Handler(Looper.getMainLooper())
    private var session: MediaSession? = null
    private var track: Track? = null
    private var playing = false
    private var artwork: Bitmap? = null
    private var running = false

    private class Track(
        val title: String,
        val artist: String,
        val album: String,
        val artwork: String?,
        val duration: Long,
    )

    // Bound from Rust with RegisterNatives. A button here becomes the same
    // session op as the one on screen, so every device sees it.
    external fun transport(action: String, position: Double)

    @JvmStatic
    fun show(
        title: String,
        artist: String,
        album: String,
        artwork: String?,
        duration: Long,
        position: Long,
        playing: Boolean,
    ) {
        main.post { update(title, artist, album, artwork, duration, position, playing) }
    }

    private fun update(
        title: String,
        artist: String,
        album: String,
        artwork: String?,
        duration: Long,
        position: Long,
        playing: Boolean,
    ) {
        val session = session ?: open()
        if (track?.artwork != artwork) {
            this.artwork = null
            if (artwork != null) fetch(artwork)
        }
        track = Track(title, artist, album, artwork, duration)
        this.playing = playing
        session.setMetadata(metadata())
        session.setPlaybackState(
            PlaybackState.Builder()
                .setActions(
                    PlaybackState.ACTION_PLAY or PlaybackState.ACTION_PAUSE or
                        PlaybackState.ACTION_PLAY_PAUSE or PlaybackState.ACTION_SKIP_TO_NEXT or
                        PlaybackState.ACTION_SKIP_TO_PREVIOUS or PlaybackState.ACTION_SEEK_TO,
                )
                .setState(
                    if (playing) PlaybackState.STATE_PLAYING else PlaybackState.STATE_PAUSED,
                    position,
                    if (playing) 1f else 0f,
                )
                .build(),
        )
        post()
    }

    @JvmStatic
    fun hide() {
        main.post { close() }
    }

    private fun close() {
        track = null
        artwork = null
        session?.release()
        session = null
        if (running) {
            running = false
            context.stopService(Intent(context, PlaybackService::class.java))
        }
        context.getSystemService(NotificationManager::class.java).cancel(ID)
    }

    private fun open(): MediaSession {
        val session = MediaSession(context, "2kHz")
        session.setCallback(object : MediaSession.Callback() {
            override fun onPlay() = transport("play", 0.0)
            override fun onPause() = transport("pause", 0.0)
            override fun onStop() = transport("pause", 0.0)
            override fun onSkipToNext() = transport("next", 0.0)
            override fun onSkipToPrevious() = transport("previous", 0.0)
            override fun onSeekTo(position: Long) = transport("seek", position / 1000.0)
        })
        session.isActive = true
        this.session = session
        return session
    }

    private fun metadata(): MediaMetadata {
        val track = track!!
        return MediaMetadata.Builder()
            .putString(MediaMetadata.METADATA_KEY_TITLE, track.title)
            .putString(MediaMetadata.METADATA_KEY_ARTIST, track.artist)
            .putString(MediaMetadata.METADATA_KEY_ALBUM, track.album)
            .putLong(MediaMetadata.METADATA_KEY_DURATION, track.duration)
            .putBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART, artwork)
            .build()
    }

    // Covers come from Qobuz's CDN, off the main thread, and only land if the
    // track has not moved on meanwhile.
    private fun fetch(url: String) {
        Thread {
            val bitmap = try {
                URL(url).openStream().use(BitmapFactory::decodeStream)
            } catch (e: Exception) {
                null
            }
            main.post {
                if (bitmap != null && track?.artwork == url) {
                    artwork = bitmap
                    session?.setMetadata(metadata())
                    post()
                }
            }
        }.start()
    }

    // The service has to put its notification up itself. Posting it here too
    // updates it in place, and still shows the controls when the service
    // could not start.
    private fun post() {
        if (!running) {
            running = true
            try {
                context.startForegroundService(Intent(context, PlaybackService::class.java))
            } catch (e: IllegalStateException) {
                // Asked from the background, which Android 12 refuses. The
                // next update tries again, by then the app may be in front.
                running = false
            }
        }
        context.getSystemService(NotificationManager::class.java).notify(ID, notification())
    }

    fun notification(): Notification {
        val manager = context.getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(CHANNEL) == null) {
            val channel = NotificationChannel(CHANNEL, "Playback", NotificationManager.IMPORTANCE_LOW)
            channel.setShowBadge(false)
            manager.createNotificationChannel(channel)
        }

        // The launcher's own intent, so a tap brings the running activity
        // back rather than starting a second one.
        val open = PendingIntent.getActivity(
            context,
            0,
            context.packageManager.getLaunchIntentForPackage(context.packageName),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val track = track
        val builder = Notification.Builder(context, CHANNEL)
            .setSmallIcon(android.R.drawable.ic_media_play)
            .setContentTitle(track?.title)
            .setContentText(track?.artist)
            .setLargeIcon(artwork)
            .setContentIntent(open)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .setOngoing(playing)
            .addAction(button(android.R.drawable.ic_media_previous, "Previous", "previous"))
            .addAction(
                if (playing) {
                    button(android.R.drawable.ic_media_pause, "Pause", "pause")
                } else {
                    button(android.R.drawable.ic_media_play, "Play", "play")
                },
            )
            .addAction(button(android.R.drawable.ic_media_next, "Next", "next"))
        session?.let {
            builder.setStyle(
                Notification.MediaStyle()
                    .setMediaSession(it.sessionToken)
                    .setShowActionsInCompactView(0, 1, 2),
            )
        }
        return builder.build()
    }

    // Android 13 and later draw the buttons from the session's actions; these
    // are for the versions before.
    private fun button(icon: Int, title: String, action: String): Notification.Action {
        val intent = Intent(context, PlaybackService::class.java).setAction(action)
        val pending = PendingIntent.getService(context, action.hashCode(), intent, PendingIntent.FLAG_IMMUTABLE)
        return Notification.Action.Builder(Icon.createWithResource(context, icon), title, pending).build()
    }
}

class PlaybackService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val action = intent?.action
        if (action != null) {
            Playback.transport(action, 0.0)
            return START_NOT_STICKY
        }
        val notification = Playback.notification()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(Playback.ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK)
        } else {
            startForeground(Playback.ID, notification)
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        stopForeground(STOP_FOREGROUND_REMOVE)
        super.onDestroy()
    }
}
