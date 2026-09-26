//! Entry point for every platform with a screen.
//!
//! Deliberately thin, everything else lives in the library, because Android
//! has no `main` of its own: dioxus-desktop's JNI trampoline dlsym's this one.

fn main() {
    // Paired through TWO_KHZ_SERVER/TWO_KHZ_TOKEN or a stored pairing. Without
    // either, or with a server that does not answer, the window opens on the
    // setup screen rather than refusing to start.
    if let Err(err) = two_khz::app::bootstrap() {
        #[cfg(not(feature = "mobile"))]
        eprintln!("not connected yet: {err:#}");
        #[cfg(feature = "mobile")]
        let _ = err;
    }

    // Release builds of dioxus-desktop cancel every `contextmenu` from the
    // document. On Android that is also the long press that brings up a text
    // field's Paste, so a token could only be typed out by hand. Rows lose
    // nothing: their menus come from their own handlers and long-press.js,
    // which still see the event.
    #[cfg(feature = "mobile")]
    dioxus::LaunchBuilder::mobile()
        .with_cfg(dioxus::mobile::Config::new().with_disable_context_menu(false))
        .launch(two_khz::app::App);

    // Three columns plus a map need room; the default window collapses them.
    // TWO_KHZ_WINDOW=WxH overrides it, mostly to check the responsive layout
    // at phone width without a phone.
    #[cfg(not(feature = "mobile"))]
    {
        let (width, height) = window_size();
        dioxus::LaunchBuilder::desktop()
            .with_cfg(
                dioxus::desktop::Config::new().with_window(
                    dioxus::desktop::WindowBuilder::new()
                        .with_title("2kHz")
                        .with_inner_size(dioxus::desktop::LogicalSize::new(width, height)),
                ),
            )
            .launch(two_khz::app::App);
    }
}

#[cfg(not(feature = "mobile"))]
fn window_size() -> (f64, f64) {
    std::env::var("TWO_KHZ_WINDOW")
        .ok()
        .and_then(|spec| {
            let (w, h) = spec.split_once(['x', 'X'])?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((1500.0, 950.0))
}
