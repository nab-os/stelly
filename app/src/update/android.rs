//! Handing a downloaded APK to Android's package installer, through the
//! `Updater` object in android/MainActivity.kt.

use super::Done;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::{JNIEnv, JavaVM};
use std::path::Path;

pub(super) fn install(apk: &Path) -> anyhow::Result<Done> {
    let context = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { JObject::from_raw(context.context().cast()) };

    let answer = match call(&mut env, &activity, apk) {
        Ok(answer) => answer,
        Err(err) => {
            // A Java exception stays pending until cleared, and the next JNI
            // call would abort on it.
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            return Err(err.into());
        }
    };
    match answer.as_str() {
        "" => Ok(Done::HandedOff),
        "permission" => Ok(Done::NeedsPermission),
        error => anyhow::bail!("{error}"),
    }
}

/// `Updater.install`'s answer: empty once the installer has the APK,
/// `permission` when the user was sent to allow installs, an error otherwise.
fn call(env: &mut JNIEnv, activity: &JObject, apk: &Path) -> jni::errors::Result<String> {
    // Through the activity, not `FindClass`: on a thread Rust attached, that
    // only sees the system's classes.
    let name = JObject::from(env.new_string("dev.dioxus.main.Updater")?);
    let class = env
        .call_method(
            activity,
            "getAppClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    let path = JObject::from(env.new_string(apk.to_string_lossy())?);
    let answer = env
        .call_static_method(
            JClass::from(class),
            "install",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[JValue::Object(&path)],
        )?
        .l()?;
    Ok(env.get_string(&JString::from(answer))?.into())
}
