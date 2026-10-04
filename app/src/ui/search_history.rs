//! What was searched for, shown in place of results while the box is empty.
//!
//! Kept on this device, in `prefs.json`, newest first. A query is kept when
//! it was used: Enter pressed on it, or the search screen left with it
//! showing, by opening something it found or going elsewhere. Not on every
//! search the debounce fires, or typing "radiohead" would leave "ra",
//! "radi" and "radioh" behind it.

use super::{icons, prefs, Library, Search};
use dioxus::prelude::*;

/// How many are kept. Enough to find last week's, few enough to read down.
const KEPT: usize = 20;

/// `query` at the front of `list`, once. Two spellings that differ only in
/// case or spacing are one search, and the newer spelling is the one kept.
fn remembered(list: &[String], query: &str) -> Vec<String> {
    let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.is_empty() {
        return list.to_vec();
    }
    let key = query.to_lowercase();
    std::iter::once(query)
        .chain(list.iter().filter(|kept| kept.to_lowercase() != key).cloned())
        .take(KEPT)
        .collect()
}

impl Library {
    /// Keep a query that was used, here and in later sessions.
    pub(crate) fn remember_search(&self, query: &str) {
        let list = remembered(&self.searches.peek(), query);
        if *self.searches.peek() == list {
            return;
        }
        let mut searches = self.searches;
        searches.set(list.clone());
        prefs::update(|prefs| prefs.searches = list);
    }

    fn forget_search(&self, query: &str) {
        let mut searches = self.searches;
        searches.write().retain(|kept| kept != query);
        let list = searches.peek().clone();
        prefs::update(|prefs| prefs.searches = list);
    }

    fn forget_searches(&self) {
        let mut searches = self.searches;
        searches.set(Vec::new());
        prefs::update(|prefs| prefs.searches.clear());
    }
}

/// Recent searches, one tap to run again. Each one leaves by the cross that
/// shows on hover, as a queue row does; all of them by the button above,
/// after a second, explicit yes.
#[component]
pub(crate) fn SearchHistory() -> Element {
    let library = use_context::<Library>();
    let search = use_context::<Search>();
    let mut confirming = use_signal(|| false);
    let searches = library.searches.read().clone();

    if searches.is_empty() {
        return rsx! {
            p { class: "muted", "Type to search." }
        };
    }

    // Straight to the results, past the debounce: the query is whole.
    let run = move |query: String| {
        let mut search = search;
        search.text.set(query.clone());
        search.submitted.set(query.clone());
        library.remember_search(&query);
        library.search_to(query);
    };

    rsx! {
        section { class: "search-history",
            div { class: "search-history-head",
                h3 { "Recent searches" }
                span { class: "spacer" }
                if !confirming() {
                    button {
                        class: "chip danger",
                        title: "forget every recent search",
                        onclick: move |_| confirming.set(true),
                        "clear all"
                    }
                }
            }

            if confirming() {
                div { class: "server-confirm",
                    p { "Forget all {searches.len()} recent searches on this device? This cannot be undone." }
                    div { class: "pair-actions",
                        button { onclick: move |_| confirming.set(false), "Cancel" }
                        button {
                            class: "danger",
                            onclick: move |_| {
                                library.forget_searches();
                                confirming.set(false);
                            },
                            "Clear all"
                        }
                    }
                }
            }

            ul { class: "search-history-list",
                for query in searches {
                    li {
                        key: "{query}",
                        class: "search-history-row",
                        onclick: {
                            let query = query.clone();
                            move |_| run(query.clone())
                        },
                        span { class: "search-history-icon", {icons::history()} }
                        span { class: "title ellipsis", "{query}" }
                        button {
                            class: "queue-remove search-history-remove",
                            title: "forget this search",
                            onclick: {
                                let query = query.clone();
                                move |event: Event<MouseData>| {
                                    event.stop_propagation();
                                    library.forget_search(&query);
                                }
                            },
                            {icons::close()}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn newest_goes_first() {
        assert_eq!(remembered(&list(&["a", "b"]), "c"), list(&["c", "a", "b"]));
    }

    #[test]
    fn a_repeat_moves_up_rather_than_doubling() {
        assert_eq!(remembered(&list(&["a", "Radiohead", "b"]), "radiohead"), list(&["radiohead", "a", "b"]));
    }

    #[test]
    fn spacing_is_tidied_and_blank_is_ignored() {
        assert_eq!(remembered(&list(&["a"]), "  massive   attack "), list(&["massive attack", "a"]));
        assert_eq!(remembered(&list(&["a"]), "   "), list(&["a"]));
    }

    #[test]
    fn the_oldest_falls_off() {
        let full: Vec<String> = (0..KEPT).map(|i| i.to_string()).collect();
        let next = remembered(&full, "new");
        assert_eq!(next.len(), KEPT);
        assert_eq!(next[0], "new");
        assert_eq!(next.last().unwrap(), &(KEPT - 2).to_string());
    }
}
