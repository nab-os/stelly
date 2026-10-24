//! The tour: a few cards, each pointing at the part of the app it is about,
//! shown once on the first start and again from settings whenever asked.
//!
//! It points at the real thing rather than at a picture of it: each step
//! names the elements it is about, and `tour.js` rings the first of them
//! that is on screen, dimming the rest, and sets the card beside it. A list
//! rather than one, because not all of them are always showing: a track's
//! page may not be open, and then the queue button is what to point at.

use super::prefs;
use dioxus::prelude::*;

struct Step {
    title: &'static str,
    body: &'static str,
    /// Tried in order; none on screen, or none at all, centres the card.
    targets: &'static [&'static str],
}

const STEPS: &[Step] = &[
    Step {
        title: "Welcome to Stelly",
        body: "Every track becomes a point in a space where close means similar-sounding. \
               This tour shows where everything is. It takes a minute, and Settings can \
               bring it back any time.",
        targets: &[],
    },
    Step {
        title: "Home and search",
        body: "Home is everything you have liked. The magnifier searches the whole \
               catalogue, or only what is already in your space, and keeps your recent \
               searches for when the box is empty.",
        targets: &[".topbar"],
    },
    Step {
        title: "Generate",
        body: "Open a track from your space and start something from it: a radio that \
               keeps going, its nearest neighbours, a path to it from another track, or a \
               drift towards a phrase. Describe a mood in the search box. Whatever you \
               start joins the queue as its own section, to reroll, save or stop.",
        targets: &[".start-box", ".queue-btn"],
    },
    Step {
        title: "The map",
        body: "The whole space drawn as a night sky, coloured by genre. Zoom into a crowded \
               patch to pull it apart, and pick a star to see which track it is.",
        targets: &[".fab-map"],
    },
    Step {
        title: "Player and queue",
        body: "What is playing, and what follows. Open the queue to drag tracks into a new \
               order, or sort what is left by the shortest route through the space.",
        targets: &[".player"],
    },
    Step {
        title: "More on every row",
        body: "Right-click a track, album or artist, or long-press it on a phone, to play it \
               next, queue it, add it to the space, start a radio from it or find it on \
               the map.",
        targets: &[],
    },
    Step {
        title: "Settings",
        body: "Your server, stream quality, family devices, hidden artists and the pipeline \
               that builds the space. The tour is in there too, for another look.",
        targets: &[".topbar button[title=settings]"],
    },
];

/// Whether the tour is showing, and which card. A context, so settings can
/// start it without the shell handing it down.
#[derive(Clone, Copy)]
pub struct Tour {
    open: Signal<bool>,
    step: Signal<usize>,
}

impl Tour {
    /// Open on the first start, and on the first start after an update that
    /// brought the tour, then not again until asked.
    pub fn new() -> Self {
        Self {
            open: Signal::new(!prefs::Prefs::load().toured),
            step: Signal::new(0),
        }
    }

    pub fn start(mut self) {
        self.step.set(0);
        self.open.set(true);
    }

    /// Finished or skipped, it is the same: seen.
    fn close(mut self) {
        self.open.set(false);
        prefs::update(|prefs| prefs.toured = true);
    }

    fn go(mut self, step: usize) {
        if step < STEPS.len() {
            self.step.set(step);
        }
    }
}

impl Default for Tour {
    fn default() -> Self {
        Self::new()
    }
}

#[component]
pub fn TourView() -> Element {
    let tour = use_context::<Tour>();
    let open = *tour.open.read();
    let at = (*tour.step.read()).min(STEPS.len() - 1);

    // After the card for this step is in the DOM, so it can be measured.
    use_effect(move || {
        if !*tour.open.read() {
            return;
        }
        let step = (*tour.step.read()).min(STEPS.len() - 1);
        let targets = serde_json::to_string(STEPS[step].targets).unwrap_or_else(|_| "[]".into());
        document::eval(&format!("{}\nwindow.stellyTour({targets});", include_str!("../../assets/tour.js")));
    });

    if !open {
        return rsx! {};
    }

    let step = &STEPS[at];
    let last = at + 1 == STEPS.len();

    rsx! {
        div {
            class: "tour",
            tabindex: "-1",
            onkeydown: move |event| match event.key() {
                Key::Escape => tour.close(),
                Key::ArrowRight | Key::Enter if last => tour.close(),
                Key::ArrowRight | Key::Enter => tour.go(at + 1),
                Key::ArrowLeft if at > 0 => tour.go(at - 1),
                _ => {}
            },
            div { class: "tour-ring" }
            div {
                class: "tour-card",
                role: "dialog",
                aria_label: "{step.title}",
                div { class: "tour-head",
                    div { class: "tour-dots",
                        for index in 0..STEPS.len() {
                            button {
                                key: "{index}",
                                class: if index == at { "tour-dot active" } else { "tour-dot" },
                                title: "{STEPS[index].title}",
                                onclick: move |_| tour.go(index),
                            }
                        }
                    }
                    span { class: "muted", "{at + 1} of {STEPS.len()}" }
                }
                h2 { "{step.title}" }
                p { "{step.body}" }
                div { class: "tour-actions",
                    if !last {
                        button { class: "link", onclick: move |_| tour.close(), "Skip the tour" }
                    }
                    span { class: "spacer" }
                    if at > 0 {
                        button { onclick: move |_| tour.go(at - 1), "Back" }
                    }
                    if last {
                        button { class: "primary", onclick: move |_| tour.close(), "Done" }
                    } else {
                        button { class: "primary", onclick: move |_| tour.go(at + 1), "Next" }
                    }
                }
            }
        }
    }
}

/// The settings row that brings the tour back.
#[component]
pub fn TourRow() -> Element {
    let tour = use_context::<Tour>();

    rsx! {
        section { class: "panel setting",
            h2 { "Tour" }
            div { class: "setting-fields",
                span { class: "muted", "A minute's walk through the main parts of the app." }
                span { class: "spacer" }
                button { onclick: move |_| tour.start(), "Take the tour" }
            }
        }
    }
}
