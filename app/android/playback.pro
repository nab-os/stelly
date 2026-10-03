# Only ever called from Rust, see src/ui/now_playing.rs.
-keep class dev.dioxus.main.Playback {
    public static void show(...);
    public static void hide();
    native <methods>;
}

# Likewise, see src/update/android.rs.
-keep class dev.dioxus.main.Updater {
    public static java.lang.String install(java.lang.String);
}
