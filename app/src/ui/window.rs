//! Long lists drawn a window at a time: their first rows, then more as the
//! end comes near. A favourites page holds up to 500 tracks, 500 albums and
//! 500 artists, and drawing every one of them to show the first screenful was
//! most of what opening it cost.
//!
//! The window is always the list's first rows, never a slice from the
//! middle: a row's place among the covers on screen is then the same however
//! far the list has grown, which is what transitions.js finds a card by on
//! the way back. Everything that acts on the list ("play all", a menu's
//! index) reads the whole of it, not what is drawn.

use super::library::Library;
use dioxus::prelude::*;
use std::collections::HashSet;

/// How many rows a list opens with. Enough to fill the column on a tall
/// screen, in tiles or in rows; each growth triples it.
pub(crate) const FIRST_SHOWN: usize = 80;

/// The least height a row takes, in pixels, its share of a line of tiles
/// included. What a page gone back to opens with enough rows for: too many is
/// only slower, too few is caught up by `MoreRows`.
const ROW_FLOOR: f64 = 20.0;

/// One arrival on a page, by `go`, `back` or `forward`, and how far down it
/// lands. Refining a search or narrowing it with a chip is the same visit.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub(crate) struct Visit {
    number: u64,
    scroll: f64,
}

impl Visit {
    /// The next arrival, landing `scroll` pixels down.
    pub(crate) fn next(self, scroll: f64) -> Visit {
        Visit { number: self.number + 1, scroll }
    }
}

/// How many rows of a list are drawn, and on which visit. A window from an
/// earlier visit no longer counts: the page that grew it has been left.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Window {
    visit: u64,
    shown: usize,
}

impl Window {
    /// What a list opens with on `visit`: the first rows, or as many as the
    /// place it lands on needs, so a page gone back to deep down is long
    /// enough to scroll there at once.
    fn opening(visit: Visit) -> Window {
        let below = (visit.scroll.max(0.0) / ROW_FLOOR).ceil() as usize;
        Window { visit: visit.number, shown: FIRST_SHOWN + below }
    }

    fn shown(self, visit: Visit) -> usize {
        if self.visit == visit.number {
            self.shown
        } else {
            Window::opening(visit).shown
        }
    }

    fn grown(self, visit: Visit) -> Window {
        Window { visit: visit.number, shown: self.shown(visit) * 3 }
    }
}

/// What a list's last draw drew, on which visit: how long the list was,
/// the rows drawn by key, and the least to draw from now on. See
/// `Rows::shown_keeping`.
#[derive(Default)]
struct Drawn {
    visit: u64,
    rows: usize,
    keys: HashSet<String>,
    least: usize,
}

impl Drawn {
    /// How many rows to draw on `visit`, of a list whose rows have `keys`
    /// in page order, for a window of `shown`; kept for the next draw.
    fn draw<'k>(&mut self, visit: u64, shown: usize, keys: impl Iterator<Item = &'k str> + Clone) -> usize {
        if self.visit != visit {
            *self = Drawn { visit, ..Drawn::default() };
        }
        let rows = keys.clone().count();
        if rows > self.rows {
            let last = keys.clone().enumerate().filter(|(_, key)| self.keys.contains(*key)).map(|(at, _)| at).last();
            if let Some(last) = last {
                self.least = self.least.max(last + 1);
            }
        }
        let shown = shown.max(self.least);
        self.rows = rows;
        self.keys = keys.take(shown).map(str::to_string).collect();
        shown
    }
}

/// A list's window. `Copy`, so a row's handler or `MoreRows` can hold it.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Rows {
    window: Signal<Window>,
    visit: CopyValue<Visit>,
    drawn: CopyValue<Drawn>,
}

impl Rows {
    /// How many rows to draw. Read, so growing draws the list again.
    pub(crate) fn shown(&self) -> usize {
        self.window.read().shown(*self.visit.peek())
    }

    /// How many rows to draw of a list whose rows, in page order, have
    /// `keys`: `shown`, or down to the last row drawn before when the list
    /// has grown since. An artist's whole discography comes in where its
    /// first page was, much of it in sections above rows that may be the
    /// ones looked at, and those stay drawn rather than fall out of the
    /// window under them. For the rest of the visit, too: a later draw for
    /// anything else would drop them again.
    pub(crate) fn shown_keeping<'k>(&self, keys: impl Iterator<Item = &'k str> + Clone) -> usize {
        let shown = self.shown();
        // Not something a draw reacts to: written on every one.
        self.drawn.write_unchecked().draw(self.visit.peek().number, shown, keys)
    }

    fn grow(mut self) {
        let visit = *self.visit.peek();
        let grown = self.window.peek().grown(visit);
        self.window.set(grown);
    }
}

/// The window of a list in the main area, opened for the page's visit.
pub(crate) fn use_rows() -> Rows {
    let visit = use_context::<Library>().visit;
    let window = use_signal(|| Window::opening(*visit.peek()));
    let drawn = use_hook(|| CopyValue::new(Drawn::default()));
    Rows { window, visit, drawn }
}

/// How many of each section's rows a window of `shown` draws, sections
/// drawn in order: a later one starts only once those before it are drawn
/// in full.
pub(crate) fn split_window(lengths: &[usize], shown: usize) -> Vec<usize> {
    let mut left = shown;
    lengths
        .iter()
        .map(|&length| {
            let drawn = length.min(left);
            left -= drawn;
            drawn
        })
        .collect()
}

/// After the last row drawn, while there are more: grows the window as the
/// end comes near. An empty box reaching a screen and a half back up over the
/// list, so it comes into view well before the end does, and the rows are in
/// by the time it is reached. Also there as a button, should the webview not
/// say.
///
/// Keyed on the window by whoever draws it, so each growth puts up a new one:
/// an observer only speaks of a change, and one still in view after growing,
/// on a tall screen or while a scroll is being put back, would never speak
/// again. A new one is told where it stands as soon as it is watched.
#[component]
pub(crate) fn MoreRows(rows: Rows) -> Element {
    rsx! {
        div { class: "more-rows",
            span {
                class: "rows-ahead",
                onvisible: move |event: Event<VisibleData>| {
                    if event.is_intersecting().unwrap_or(false) {
                        rows.grow();
                    }
                },
            }
            button { class: "chip", onclick: move |_| rows.grow(), "show more" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_opened_at_its_top_draws_the_first_rows() {
        let visit = Visit::default().next(0.0);
        assert_eq!(Window::opening(visit).shown(visit), FIRST_SHOWN);
    }

    #[test]
    fn a_page_gone_back_to_draws_down_to_where_it_lands() {
        // 4,000 pixels down is at most 200 rows of the shortest kind.
        let visit = Visit::default().next(4000.0);
        let shown = Window::opening(visit).shown(visit);
        assert!(shown as f64 * ROW_FLOOR >= 4000.0, "{shown}");
        assert_eq!(shown, FIRST_SHOWN + 200);
    }

    #[test]
    fn growing_triples_and_lasts_the_visit() {
        let visit = Visit::default().next(0.0);
        let window = Window::opening(visit).grown(visit);
        assert_eq!(window.shown(visit), FIRST_SHOWN * 3);
        assert_eq!(window.grown(visit).shown(visit), FIRST_SHOWN * 9);
    }

    #[test]
    fn a_new_visit_starts_the_window_again() {
        let first = Visit::default().next(0.0);
        let grown = Window::opening(first).grown(first).grown(first);
        let then = first.next(0.0);
        assert_eq!(grown.shown(then), FIRST_SHOWN);
        // And grows from there, not from what the last visit had.
        assert_eq!(grown.grown(then).shown(then), FIRST_SHOWN * 3);
        let back = then.next(2000.0);
        assert_eq!(grown.shown(back), FIRST_SHOWN + 100);
    }

    /// A list that grows under the rows drawn keeps them drawn, wherever
    /// they went; one the same length, re-sorted, draws its window.
    #[test]
    fn rows_drawn_stay_drawn_when_the_list_grows_above_them() {
        let mut drawn = Drawn::default();
        let first = ["a1", "a2", "s1", "s2"];
        assert_eq!(drawn.draw(1, 3, first.into_iter()), 3);
        let whole = ["a1", "a2", "a3", "a4", "a5", "s1", "s2", "s3"];
        assert_eq!(drawn.draw(1, 3, whole.into_iter()), 6, "down to s1");
        assert_eq!(drawn.draw(1, 3, whole.into_iter()), 6, "and on every draw after");
        assert_eq!(drawn.draw(1, 3, whole.into_iter().rev()), 6);
        assert_eq!(drawn.draw(2, 3, whole.into_iter()), 3, "a new visit starts again");
    }

    #[test]
    fn sections_fill_in_page_order() {
        assert_eq!(split_window(&[30, 100, 500], 80), [30, 50, 0]);
        assert_eq!(split_window(&[30, 100, 500], 240), [30, 100, 110]);
        assert_eq!(split_window(&[30, 100], 1000), [30, 100]);
        assert_eq!(split_window(&[], 80), Vec::<usize>::new());
    }

    // What `MoreRows` relies on: drawn under a key that changes with the
    // window, it is put up afresh, and so watched afresh, on every growth.
    #[test]
    fn a_keyed_mark_is_put_up_again_when_its_key_changes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SHOWN: AtomicUsize = AtomicUsize::new(80);
        static MOUNTS: AtomicUsize = AtomicUsize::new(0);

        #[component]
        fn Mark(shown: usize) -> Element {
            use_hook(|| MOUNTS.fetch_add(1, Ordering::SeqCst));
            rsx! { span { "{shown}" } }
        }

        fn List() -> Element {
            let shown = SHOWN.load(Ordering::SeqCst);
            rsx! {
                ul {}
                if shown < 500 {
                    Mark { key: "{shown}", shown }
                }
            }
        }

        let mut dom = VirtualDom::new(List);
        dom.rebuild_in_place();
        assert_eq!(MOUNTS.load(Ordering::SeqCst), 1);
        // Drawn again with the same window: the same mark.
        dom.mark_dirty(ScopeId::APP);
        dom.render_immediate_to_vec();
        assert_eq!(MOUNTS.load(Ordering::SeqCst), 1);
        SHOWN.store(240, Ordering::SeqCst);
        dom.mark_dirty(ScopeId::APP);
        dom.render_immediate_to_vec();
        assert_eq!(MOUNTS.load(Ordering::SeqCst), 2);
    }
}
