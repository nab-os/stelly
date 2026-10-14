package dev.dioxus.main

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageInstaller
import android.content.pm.ServiceInfo
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.drawable.Icon
import android.media.AudioAttributes
import android.media.AudioManager
import android.media.MediaMetadata
import android.media.VolumeProvider
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.provider.Settings
import android.view.KeyEvent
import android.widget.Toast
import java.io.File
import java.net.URL

typealias BuildConfig = fr.glargh.stelly.BuildConfig

class MainActivity : WryActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // Before super, which starts the Rust side.
        Playback.context = applicationContext
        Updater.context = applicationContext
        super.onCreate(savedInstanceState)
    }

    // While another device is the output, the volume keys turn its volume.
    // The session's remote volume does the same once the app is in the
    // background, but in front Android keeps the keys for its own streams
    // whenever the session is not playing.
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        return Playback.volumeKey(event) || super.dispatchKeyEvent(event)
    }
}

// The lock screen's and the shade's "now playing", for whatever this device's
// <audio> element holds. Its service is also what keeps the process running
// once the screen locks: without one, Android freezes the app and the sound
// stops with it.
//
// It also stands for another device's sound: while one is the output, the
// session plays remotely, and the volume keys turn that device's volume
// rather than this one's.
//
// Rust calls `show`, `hide` and the volume calls from its own thread, see
// src/ui/now_playing.rs. Everything else happens on the main one.
object Playback {
    const val CHANNEL = "playback"
    const val ID = 1

    // How finely the keys turn another device's volume: 5% a press.
    private const val STEPS = 20

    lateinit var context: Context
    private val main = Handler(Looper.getMainLooper())
    private var session: MediaSession? = null
    private var track: Track? = null
    private var playing = false
    private var artwork: Bitmap? = null
    private var running = false
    private var remote: VolumeProvider? = null
    private var watching = false
    private var level = -1.0

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
        volume: Double,
    ) {
        main.post { update(title, artist, album, artwork, duration, position, playing, volume) }
    }

    private fun update(
        title: String,
        artist: String,
        album: String,
        artwork: String?,
        duration: Long,
        position: Long,
        playing: Boolean,
        volume: Double,
    ) {
        val session = session ?: open()
        playOn(session, volume)
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

    // Negative when the sound is this device's own.
    private fun playOn(session: MediaSession, volume: Double) {
        if (volume < 0) {
            if (remote != null) {
                remote = null
                session.setPlaybackToLocal(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                        .build(),
                )
            }
            return
        }
        val provider = remote ?: object : VolumeProvider(VolumeProvider.VOLUME_CONTROL_ABSOLUTE, STEPS, 0) {
            override fun onSetVolumeTo(volume: Int) = turn(volume)

            // Only up and down: mute and its toggles would need a level to
            // come back to.
            override fun onAdjustVolume(direction: Int) {
                if (direction == AudioManager.ADJUST_RAISE || direction == AudioManager.ADJUST_LOWER) {
                    turn(currentVolume + direction)
                }
            }
        }.also {
            remote = it
            session.setPlaybackToRemote(it)
        }
        provider.currentVolume = Math.round(volume * STEPS).toInt()
    }

    private fun turn(volume: Int) {
        val provider = remote ?: return
        val clamped = volume.coerceIn(0, STEPS)
        provider.currentVolume = clamped
        transport("volume", clamped.toDouble() / STEPS)
    }

    // The other device's volume moved, from its own system or a third one.
    @JvmStatic
    fun remoteVolume(volume: Double) {
        main.post { remote?.currentVolume = Math.round(volume * STEPS).toInt() }
    }

    fun volumeKey(event: KeyEvent): Boolean {
        val direction = when (event.keyCode) {
            KeyEvent.KEYCODE_VOLUME_UP -> 1
            KeyEvent.KEYCODE_VOLUME_DOWN -> -1
            else -> return false
        }
        val provider = remote ?: return false
        // The release too, or it would reach the system's own volume.
        if (event.action == KeyEvent.ACTION_DOWN) turn(provider.currentVolume + direction)
        return true
    }

    // This device's media volume, which is what the app's slider shows while
    // the sound is its own. Reported now and on every change, wherever it
    // comes from: the keys, the shade, another app.
    @JvmStatic
    fun watchVolume() {
        main.post {
            if (watching) return@post
            watching = true
            val receiver = object : BroadcastReceiver() {
                override fun onReceive(context: Context, intent: Intent) = reportVolume()
            }
            // Not in the SDK, but what every volume change broadcasts.
            val filter = IntentFilter("android.media.VOLUME_CHANGED_ACTION")
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
            } else {
                context.registerReceiver(receiver, filter)
            }
            reportVolume()
        }
    }

    private fun reportVolume() {
        val audio = context.getSystemService(AudioManager::class.java)
        val max = audio.getStreamMaxVolume(AudioManager.STREAM_MUSIC)
        if (max <= 0) return
        val now = audio.getStreamVolume(AudioManager.STREAM_MUSIC).toDouble() / max
        if (now != level) {
            level = now
            transport("system-volume", now)
        }
    }

    @JvmStatic
    fun setVolume(volume: Double) {
        main.post {
            val audio = context.getSystemService(AudioManager::class.java)
            val max = audio.getStreamMaxVolume(AudioManager.STREAM_MUSIC)
            audio.setStreamVolume(AudioManager.STREAM_MUSIC, Math.round(volume * max).toInt(), 0)
            // Even when it stayed on the same step, so the slider goes back
            // to it.
            level = -1.0
            reportVolume()
        }
    }

    @JvmStatic
    fun hide() {
        main.post { close() }
    }

    private fun close() {
        track = null
        artwork = null
        remote = null
        session?.release()
        session = null
        if (running) {
            running = false
            context.stopService(Intent(context, PlaybackService::class.java))
        }
        context.getSystemService(NotificationManager::class.java).cancel(ID)
    }

    private fun open(): MediaSession {
        val session = MediaSession(context, "Stelly")
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

// Installs a release the Rust side downloaded from GitHub, see
// src/update/android.rs. A session rather than an ACTION_VIEW intent: that
// would need a FileProvider, and dx has nowhere to declare its paths.
object Updater {
    private const val ACTION = "fr.glargh.stelly.UPDATE_STATUS"

    lateinit var context: Context
    private var listening = false

    // Called on a Rust thread. Empty once the installer has the APK,
    // "permission" when the user has been sent to allow installs from Stelly,
    // and the reason otherwise.
    @JvmStatic
    fun install(path: String): String {
        val packages = context.packageManager
        if (!packages.canRequestPackageInstalls()) {
            val settings = Intent(
                Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                Uri.parse("package:${context.packageName}"),
            ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(settings)
            return "permission"
        }

        return try {
            val apk = File(path)
            val installer = packages.packageInstaller
            val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
            params.setAppPackageName(context.packageName)
            // Only skips the prompt once Stelly is its own installer of
            // record, that is from the second update on.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                params.setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
            }
            val id = installer.createSession(params)
            installer.openSession(id).use { session ->
                session.openWrite("stelly.apk", 0, apk.length()).use { out ->
                    apk.inputStream().use { it.copyTo(out) }
                    session.fsync(out)
                }
                apk.delete()
                listen()
                // Mutable, since the installer fills in the status; explicit,
                // which Android 14 requires of a mutable one.
                val status = Intent(ACTION).setPackage(context.packageName)
                val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
                } else {
                    PendingIntent.FLAG_UPDATE_CURRENT
                }
                session.commit(PendingIntent.getBroadcast(context, id, status, flags).intentSender)
            }
            ""
        } catch (e: Exception) {
            e.message ?: e.toString()
        }
    }

    // The installer reports back here, first with the confirmation screen to
    // show, then with how it went. On success Android has already stopped
    // this process.
    private fun listen() {
        if (listening) return
        listening = true
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                when (intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
                    PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                        val confirm = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                            intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
                        } else {
                            @Suppress("DEPRECATION")
                            intent.getParcelableExtra(Intent.EXTRA_INTENT)
                        }
                        confirm?.let { context.startActivity(it.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
                    }
                    PackageInstaller.STATUS_SUCCESS -> {}
                    PackageInstaller.STATUS_FAILURE_ABORTED -> {}
                    else -> {
                        val reason = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE) ?: "unknown error"
                        Toast.makeText(context, "Update failed: $reason", Toast.LENGTH_LONG).show()
                    }
                }
            }
        }
        val filter = IntentFilter(ACTION)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            context.registerReceiver(receiver, filter)
        }
    }
}
