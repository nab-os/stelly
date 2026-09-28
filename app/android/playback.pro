# Only ever called from Rust, see src/ui/now_playing.rs.
-keep class dev.dioxus.main.Playback {
    public static void show(...);
    public static void hide();
    native <methods>;
}
