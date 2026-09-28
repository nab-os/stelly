//! Line icons, drawn inline rather than as emoji: the desktop webview and an
//! Android phone render the same codepoint as two different pictures, and a
//! globe or a magnifier on a round button has to look like one on both.
//!
//! 24px grid, stroked with `currentColor`, so a button's colour is the icon's.
//! Sizing is the caller's, through CSS on `.icon`.

use dioxus::prelude::*;

fn stroked(body: Element) -> Element {
    rsx! {
        svg {
            class: "icon",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            {body}
        }
    }
}

fn filled(body: Element) -> Element {
    rsx! {
        svg {
            class: "icon",
            view_box: "0 0 24 24",
            fill: "currentColor",
            {body}
        }
    }
}

pub fn globe() -> Element {
    stroked(rsx! {
        circle { cx: "12", cy: "12", r: "10" }
        path { d: "M2 12h20" }
        path { d: "M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z" }
    })
}

pub fn search() -> Element {
    stroked(rsx! {
        circle { cx: "11", cy: "11", r: "7" }
        path { d: "M20 20l-4.2-4.2" }
    })
}

/// Three bars, the tracklist.
pub fn queue() -> Element {
    stroked(rsx! {
        path { d: "M4 6h16M4 12h16M4 18h16" }
    })
}

/// A speaker cabinet, where the sound comes out.
pub fn speaker() -> Element {
    stroked(rsx! {
        rect { x: "5", y: "2", width: "14", height: "20", rx: "2" }
        circle { cx: "12", cy: "14", r: "4" }
        path { d: "M12 6h.01" }
    })
}

pub fn volume(muted: bool) -> Element {
    stroked(rsx! {
        path { d: "M11 5L6 9H2v6h4l5 4V5z" }
        if muted {
            path { d: "M22 9l-6 6M16 9l6 6" }
        } else {
            path { d: "M15.5 8.5a5 5 0 0 1 0 7" }
            path { d: "M19 5a10 10 0 0 1 0 14" }
        }
    })
}

pub fn play() -> Element {
    filled(rsx! {
        path { d: "M7 4.5v15a1 1 0 0 0 1.5.86l12.5-7.5a1 1 0 0 0 0-1.72L8.5 3.64A1 1 0 0 0 7 4.5z" }
    })
}

pub fn pause() -> Element {
    filled(rsx! {
        rect { x: "6", y: "4", width: "4", height: "16", rx: "1" }
        rect { x: "14", y: "4", width: "4", height: "16", rx: "1" }
    })
}

pub fn previous() -> Element {
    filled(rsx! {
        path { d: "M19 5.5v13a1 1 0 0 1-1.55.83L8 13v6H6V5h2v6l9.45-6.33A1 1 0 0 1 19 5.5z" }
    })
}

pub fn next() -> Element {
    filled(rsx! {
        path { d: "M5 5.5v13a1 1 0 0 0 1.55.83L16 13v6h2V5h-2v6L6.55 4.67A1 1 0 0 0 5 5.5z" }
    })
}

pub fn back() -> Element {
    stroked(rsx! {
        path { d: "M19 12H5M12 19l-7-7 7-7" }
    })
}

pub fn close() -> Element {
    stroked(rsx! {
        path { d: "M18 6L6 18M6 6l12 12" }
    })
}

/// Home, which is the favourites.
pub fn home() -> Element {
    stroked(rsx! {
        path { d: "M3 10.5L12 3l9 7.5" }
        path { d: "M5 9v11a1 1 0 0 0 1 1h4v-6h4v6h4a1 1 0 0 0 1-1V9" }
    })
}

pub fn heart(full: bool) -> Element {
    rsx! {
        svg {
            class: "icon",
            view_box: "0 0 24 24",
            fill: if full { "currentColor" } else { "none" },
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linejoin: "round",
            path { d: "M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.7l-1-1.1a5.5 5.5 0 0 0-7.8 7.8l1 1.1L12 21.2l7.8-7.7 1-1.1a5.5 5.5 0 0 0 0-7.8z" }
        }
    }
}

pub fn settings() -> Element {
    stroked(rsx! {
        circle { cx: "12", cy: "12", r: "3" }
        path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" }
    })
}

/// The generate panel's own mark, on the phone's button that slides it in.
pub fn spark() -> Element {
    stroked(rsx! {
        path { d: "M12 3l1.9 5.8L20 11l-6.1 2.2L12 19l-1.9-5.8L4 11l6.1-2.2z" }
    })
}

pub fn grid() -> Element {
    stroked(rsx! {
        rect { x: "4", y: "4", width: "7", height: "7", rx: "1" }
        rect { x: "13", y: "4", width: "7", height: "7", rx: "1" }
        rect { x: "4", y: "13", width: "7", height: "7", rx: "1" }
        rect { x: "13", y: "13", width: "7", height: "7", rx: "1" }
    })
}

pub fn list() -> Element {
    stroked(rsx! {
        path { d: "M9 6h11M9 12h11M9 18h11" }
        circle { cx: "4.5", cy: "6", r: "1" }
        circle { cx: "4.5", cy: "12", r: "1" }
        circle { cx: "4.5", cy: "18", r: "1" }
    })
}

/// Round marks for artists, square for albums, taken in turn: one list with
/// every kind of thing in it.
pub fn mixed() -> Element {
    stroked(rsx! {
        g { fill: "currentColor", stroke: "none",
            circle { cx: "5", cy: "4.5", r: "2" }
            rect { x: "3", y: "7.5", width: "4", height: "4", rx: "0.5" }
            circle { cx: "5", cy: "14.5", r: "2" }
            rect { x: "3", y: "17.5", width: "4", height: "4", rx: "0.5" }
        }
        path { d: "M10 4.5h10M10 9.5h10M10 14.5h10M10 19.5h10" }
    })
}

/// The same marks as `mixed`, gathered by kind.
pub fn split() -> Element {
    stroked(rsx! {
        g { fill: "currentColor", stroke: "none",
            circle { cx: "5", cy: "3.5", r: "2" }
            circle { cx: "5", cy: "8.5", r: "2" }
            rect { x: "3", y: "13.5", width: "4", height: "4", rx: "0.5" }
            rect { x: "3", y: "18.5", width: "4", height: "4", rx: "0.5" }
        }
        path { d: "M10 3.5h10M10 8.5h10M10 15.5h10M10 20.5h10" }
    })
}
