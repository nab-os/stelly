//! The media notification on Android, and the lock screen's controls with it.
//!
//! The webview's `navigator.mediaSession` reaches nothing there (see
//! `player.js`), so `android/MainActivity.kt` keeps a MediaSession of its own,
//! and a foreground service to go with it. The service is the part that
//! matters most: without one, Android freezes the app soon after the screen
//! locks, and the sound stops.
//!
//! While another device is the output, the notification stands for its sound
//! instead, and the volume keys turn that device's volume.
//!
//! Calls go out to the activity's `Playback` object through JNI; its buttons
//! come back through `transport`, bound with `RegisterNatives` so the linker
//! cannot drop it for want of a caller.

use crate::catalogue::RemoteTrack;
use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::jdouble;
use jni::{JNIEnv, JavaVM, NativeMethod};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// How far the notification's own clock may drift from the element's before
/// it is corrected. A seek jumps well past it; ordinary jitter does not.
const DRIFT: f64 = 2.0;

pub enum Button {
    Play,
    Pause,
    Next,
    Previous,
    Seek(f64),
    /// The volume keys, or the system's slider, turning the output's volume
    /// while it is another device.
    Volume(f64),
    /// This device's own media volume, which moved.
    SystemVolume(f64),
}

struct Shown {
    id: i64,
    playing: bool,
    position: f64,
    at: Instant,
    remote: Option<f64>,
}

/// What the notification last said. It runs its own clock from there, so a
/// track playing on steadily needs no further calls.
static SHOWN: Mutex<Option<Shown>> = Mutex::new(None);

static BUTTONS: LazyLock<(UnboundedSender<Button>, tokio::sync::Mutex<UnboundedReceiver<Button>>)> =
    LazyLock::new(|| {
        let (sender, receiver) = unbounded_channel();
        (sender, tokio::sync::Mutex::new(receiver))
    });

/// Put up, update or take down the notification. `None` when there is no
/// track to show. `remote` is the output's volume when the output is another
/// device.
pub fn show(track: Option<&RemoteTrack>, playing: bool, position: f64, remote: Option<f64>) {
    let mut shown = SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(track) = track else {
        if shown.take().is_some() {
            call(|env, class| env.call_static_method(class, "hide", "()V", &[]).map(drop));
        }
        return;
    };

    let same = shown.as_mut().filter(|last| {
        last.id == track.id && last.playing == playing && last.remote.is_some() == remote.is_some()
    });
    if let Some(last) = same {
        let moved = if playing { last.at.elapsed().as_secs_f64() } else { 0.0 };
        if (last.position + moved - position).abs() < DRIFT {
            // Only the volume moved, which needs no new notification.
            if let Some(level) = remote.filter(|&level| last.remote != Some(level)) {
                last.remote = remote;
                call(|env, class| {
                    env.call_static_method(class, "remoteVolume", "(D)V", &[JValue::Double(level)])
                        .map(drop)
                });
            }
            return;
        }
    }
    *shown = Some(Shown {
        id: track.id,
        playing,
        position,
        at: Instant::now(),
        remote,
    });
    drop(shown);

    call(|env, class| {
        let title = JObject::from(env.new_string(&track.title)?);
        let artist = JObject::from(env.new_string(&track.artist)?);
        let album = JObject::from(env.new_string(&track.album)?);
        let image = match &track.image {
            Some(url) => JObject::from(env.new_string(url)?),
            None => JObject::null(),
        };
        let duration = track.duration.unwrap_or(0) * 1000;
        env.call_static_method(
            class,
            "show",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;JJZD)V",
            &[
                JValue::Object(&title),
                JValue::Object(&artist),
                JValue::Object(&album),
                JValue::Object(&image),
                JValue::Long(duration),
                JValue::Long((position * 1000.0) as i64),
                JValue::Bool(playing.into()),
                JValue::Double(remote.unwrap_or(-1.0)),
            ],
        )
        .map(drop)
    });
}

/// Start reporting this device's media volume, as `Button::SystemVolume`,
/// beginning with where it is now.
pub fn watch_volume() {
    call(|env, class| env.call_static_method(class, "watchVolume", "()V", &[]).map(drop));
}

/// Set this device's media volume, 0.0 to 1.0. It lands on the nearest of the
/// system's steps, and comes back through `watch_volume` as that.
pub fn set_volume(level: f64) {
    call(|env, class| {
        env.call_static_method(class, "setVolume", "(D)V", &[JValue::Double(level)])
            .map(drop)
    });
}

/// The notification's and the lock screen's buttons, for as long as the
/// caller holds on. One listener at a time.
pub async fn buttons() -> tokio::sync::MutexGuard<'static, UnboundedReceiver<Button>> {
    BUTTONS.1.lock().await
}

extern "system" fn transport(mut env: JNIEnv, _this: JObject, action: JString, value: jdouble) {
    let Ok(action) = env.get_string(&action) else {
        return;
    };
    let button = match action.to_str() {
        Ok("play") => Button::Play,
        Ok("pause") => Button::Pause,
        Ok("next") => Button::Next,
        Ok("previous") => Button::Previous,
        Ok("seek") => Button::Seek(value),
        Ok("volume") => Button::Volume(value),
        Ok("system-volume") => Button::SystemVolume(value),
        _ => return,
    };
    let _ = BUTTONS.0.send(button);
}

struct Bridge {
    vm: JavaVM,
    playback: GlobalRef,
}

/// Found once, while the activity is certainly there: `ndk_context` panics
/// once tao has released it.
fn bridge() -> Option<&'static Bridge> {
    static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();
    BRIDGE
        .get_or_init(|| match connect() {
            Ok(bridge) => Some(bridge),
            Err(err) => {
                log::error!("no playback notification: {err}");
                None
            }
        })
        .as_ref()
}

fn connect() -> jni::errors::Result<Bridge> {
    let context = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }?;
    let mut env = vm.attach_current_thread_permanently()?;
    let activity = unsafe { JObject::from_raw(context.context().cast()) };

    // Through the activity, not `FindClass`: on a thread Rust attached, that
    // only sees the system's classes.
    let name = JObject::from(env.new_string("dev.dioxus.main.Playback")?);
    let class = env
        .call_method(
            &activity,
            "getAppClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    let class = JClass::from(class);
    env.register_native_methods(
        &class,
        &[NativeMethod {
            name: "transport".into(),
            sig: "(Ljava/lang/String;D)V".into(),
            fn_ptr: transport as *mut _,
        }],
    )?;
    let playback = env.new_global_ref(class)?;
    Ok(Bridge { vm, playback })
}

fn call(f: impl FnOnce(&mut JNIEnv, &JClass) -> jni::errors::Result<()>) {
    let Some(bridge) = bridge() else {
        return;
    };
    let Ok(mut env) = bridge.vm.attach_current_thread_permanently() else {
        return;
    };
    let class = <&JClass>::from(bridge.playback.as_obj());
    let result = env.with_local_frame(8, |env| f(env, class));
    if let Err(err) = result {
        // A Java exception stays pending until cleared, and the next JNI call
        // would abort on it.
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        log::error!("playback notification: {err}");
    }
}
