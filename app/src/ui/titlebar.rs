//! The window's own title bar, in place of the system's.
//!
//! The desktop window is opened without decorations (see `main.rs`), so the
//! bar along the top is the app's colour rather than a light strip from the
//! window manager. Everything the system bar did is redone here: dragging
//! moves the window, a double click maximises it, and the three buttons on
//! the right minimise, maximise or restore, and close.
//!
//! Without decorations the system's resize border is gone too, so the edges
//! and corners get thin handles that hand the drag back to the window
//! manager. They are taken away while maximised, where there is nothing to
//! resize.

use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::tao::window::ResizeDirection;
use dioxus::desktop::{use_window, use_wry_event_handler};
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;

use super::icons;

#[component]
pub fn TitleBar() -> Element {
    let window = use_window();
    let mut maximized = use_signal(|| window.is_maximized());

    // Maximising from the keyboard, a double click on the bar or a tiling
    // window manager all end in a resize, so the button's icon follows what
    // the window is rather than what was last clicked.
    let watched = window.clone();
    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent { event: WindowEvent::Resized(_), window_id, .. } = event {
            if *window_id == watched.id() {
                let now = watched.is_maximized();
                if maximized() != now {
                    maximized.set(now);
                }
            }
        }
    });

    let (drag, toggle, minimize, maximize, close) =
        (window.clone(), window.clone(), window.clone(), window.clone(), window.clone());

    rsx! {
        header { class: "titlebar",
            div {
                class: "titlebar-drag",
                onmousedown: move |evt| {
                    if evt.trigger_button() == Some(MouseButton::Primary) {
                        drag.drag();
                    }
                },
                ondoubleclick: move |_| toggle.toggle_maximized(),
                span { class: "titlebar-mark", {icons::constellation()} }
                span { class: "titlebar-title", "Stelly" }
            }
            div { class: "titlebar-controls",
                button {
                    class: "titlebar-btn",
                    title: "minimise",
                    aria_label: "minimise",
                    onclick: move |_| minimize.set_minimized(true),
                    {icons::window_minimize()}
                }
                button {
                    class: "titlebar-btn",
                    title: if maximized() { "restore" } else { "maximise" },
                    aria_label: if maximized() { "restore" } else { "maximise" },
                    onclick: move |_| maximize.toggle_maximized(),
                    if maximized() {
                        {icons::window_restore()}
                    } else {
                        {icons::window_maximize()}
                    }
                }
                button {
                    class: "titlebar-btn titlebar-close",
                    title: "close",
                    aria_label: "close",
                    onclick: move |_| close.close(),
                    {icons::window_close()}
                }
            }
        }

        if !maximized() {
            for (class, direction) in RESIZE_HANDLES {
                div {
                    key: "{class}",
                    class: "resize-handle {class}",
                    onmousedown: {
                        let window = window.clone();
                        move |evt: MouseEvent| {
                            if evt.trigger_button() == Some(MouseButton::Primary) {
                                let _ = window.drag_resize_window(direction);
                            }
                        }
                    },
                }
            }
        }
    }
}

const RESIZE_HANDLES: [(&str, ResizeDirection); 8] = [
    ("n", ResizeDirection::North),
    ("s", ResizeDirection::South),
    ("e", ResizeDirection::East),
    ("w", ResizeDirection::West),
    ("ne", ResizeDirection::NorthEast),
    ("nw", ResizeDirection::NorthWest),
    ("se", ResizeDirection::SouthEast),
    ("sw", ResizeDirection::SouthWest),
];
