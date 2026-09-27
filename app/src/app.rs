//! The root component, and the startup it needs.
//!
//! In the library rather than `main.rs` because an Android build has no
//! `main`: the APK loads the .so and calls `start_app`.

use crate::backend::{self, backend};
use crate::ui::{
    icons, space_track, Blocklist, ContextMenu, ContextMenuView, Cover, Crawler, GeneratePanel,
    Generator, Library, LocalIds, MainScreen, MapView, PathPill, Pipeline, Player, PlayerBar,
    QueueView, Search, Selection, SpaceMatches, SpaceReach, SpaceRow, View, Weights,
};
use crate::{engine, map, Wiring};
use crate::ui::library::{names_track, Sort};
use crate::platform::wry::http::Response;
use dioxus::prelude::*;
use std::rc::Rc;
use std::sync::OnceLock;

/// A track's searchable text, lowercased once. Fields stay separate rather
/// than being joined: terms are split on whitespace, so no term can span a
/// field boundary, which makes "matches any field" and "matches the joined
/// string" the same test, without the join's allocation.
#[derive(PartialEq)]
struct Haystack {
    artist: String,
    title: String,
    album: String,
}

#[cfg(test)]
impl Haystack {
    fn contains(&self, term: &str) -> bool {
        self.artist.contains(term) || self.title.contains(term) || self.album.contains(term)
    }
}

/// Where the engine loads the space from: the synced copy. Fixed by
/// `bootstrap`, or by the setup screen on a first pairing.
static DATA_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
static DB_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

pub(crate) fn data_dir() -> &'static std::path::Path {
    DATA_DIR.get().expect("set by bootstrap")
}

pub(crate) fn db_path() -> &'static std::path::Path {
    DB_PATH.get().expect("set by bootstrap")
}

/// Wire up the backend, sync, and load the space. Fallible but not fatal: an
/// unpaired or unreachable client opens on the setup screen.
pub fn bootstrap() -> anyhow::Result<()> {
    let wiring = Wiring::from_env()?;

    let data_dir = wiring.data_dir().to_path_buf();
    let db_path = wiring.db_path();
    let _ = DATA_DIR.set(data_dir.clone());
    let _ = DB_PATH.set(db_path.clone());

    backend::init(wiring.into_backend());

    // Sync before the engine loads: it memory-maps what it finds and will not
    // look again until told to.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(backend().sync_space())?;

    crate::init_engine(&data_dir, &db_path)
}

/// Re-run the parts of `bootstrap` a freshly paired device needs, against
/// the backend the setup screen has just installed.
pub async fn sync_and_load() -> anyhow::Result<()> {
    backend().sync_space().await?;
    let data_dir = crate::client_data_dir();
    let db_path = data_dir.join("catalog.db");
    let _ = DATA_DIR.set(data_dir.clone());
    let _ = DB_PATH.set(db_path.clone());
    crate::init_engine(&data_dir, &db_path)
}


/// The root: either the app, or the screen that gets you to the app.
///
/// Two components rather than an early return, Dioxus counts hooks per
/// scope, and changing that count between renders panics.
#[component]
pub fn App() -> Element {
    let ready = use_signal(crate::engine_ready);

    rsx! {
        style { {include_str!("../assets/style.css")} }
        if ready() {
            Shell {}
        } else {
            Setup { ready }
        }
    }
}

/// Pairing, for a device not configured through the environment. The address
/// and token are typed once and written beside the synced space.
#[component]
fn Setup(ready: Signal<bool>) -> Element {
    let mut address = use_signal(|| {
        crate::ServerConfig::load()
            .map(|c| c.base)
            .unwrap_or_else(|| "http://".into())
    });
    let mut token = use_signal(String::new);
    let mut status = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);

    let connect = move |_| {
        let base = address.peek().trim().trim_end_matches('/').to_string();
        let secret = token.peek().trim().to_string();
        if base.is_empty() || secret.is_empty() {
            status.set(Some("Both the address and the token are needed.".into()));
            return;
        }

        busy.set(true);
        status.set(Some("connecting…".into()));

        spawn(async move {
            let config = crate::ServerConfig {
                base: base.clone(),
                token: secret.clone(),
            };

            // Save first, so the next launch starts from what was typed here
            // rather than the pairing it just failed to reach.
            if let Err(err) = config.save() {
                status.set(Some(format!("could not save the pairing: {err:#}")));
                busy.set(false);
                return;
            }

            backend::init(Wiring::remote(base, secret).into_backend());

            match crate::app::sync_and_load().await {
                Ok(()) => {
                    status.set(None);
                    ready.set(true);
                }
                Err(err) => {
                    status.set(Some(format!(
                        "{err:#}\n\nIf the address and token are right, the server may not have \
                         built a space yet, run build-space and layout on it."
                    )));
                    busy.set(false);
                }
            }
        });
    };

    rsx! {
        div { class: "setup",
            h1 { "Connect to your server" }
            p { class: "muted",
                "This device navigates the space on its own, but the catalogue, playback and \
                 the pipeline live on a machine that can host them. Pair one with "
                code { "two-khz-server pair --name phone --scope play" }
                "."
            }

            label { "Server address" }
            input {
                class: "search",
                placeholder: "http://nas.tailnet.ts.net:7700",
                value: "{address}",
                oninput: move |event| address.set(event.value()),
            }

            label { "Device token" }
            input {
                class: "search",
                placeholder: "what `pair` printed",
                value: "{token}",
                oninput: move |event| token.set(event.value()),
            }

            div { class: "actions",
                button {
                    class: "primary",
                    disabled: busy(),
                    onclick: connect,
                    if busy() { "connecting…" } else { "connect" }
                }
            }

            if let Some(message) = status.read().clone() {
                pre { class: "log", "{message}" }
            }
        }
    }
}

/// The selected point's card on the map: cover, title, album, artist, and the
/// way to the track's page. map.js pins it over the point on every frame, so
/// it rides along with a pan or a zoom at the same size on screen.
///
/// Rendered whenever the map is, empty when nothing is selected, because
/// map.js looks it up by id and owns its position and visibility; a card
/// that came and went would lose both between frames.
#[component]
fn MapCard() -> Element {
    let library = use_context::<Library>();
    let map = use_context::<MapView>();
    let selected = use_context::<Selection>().0;

    let track = selected().and_then(space_track);
    let target = track.clone();

    rsx! {
        div {
            id: "map-card",
            class: "map-card",
            onclick: move |_| {
                if let Some(track) = target.clone() {
                    map.close();
                    library.go(View::Track(track));
                }
            },
            if let Some(track) = track {
                Cover { url: track.image.clone(), class: "thumb eager" }
                div { class: "map-card-text",
                    span { class: "title", "{track.title}" }
                    span { class: "muted ellipsis", "{track.album}" }
                    span { class: "muted ellipsis", "{track.artist}" }
                }
            }
        }
    }
}

#[component]
fn Shell() -> Element {
    let search = use_context_provider(Search::new);
    // Read-only here: the box that writes it is `SearchBar`, kept separate so
    // the shell's renders cannot clobber what is being typed.
    let query = search.text;
    use_context_provider(|| {
        Weights(Signal::new(engine().lock().unwrap().space.default_weights()))
    });

    // The track in hand, for the generate panel and the map.
    let selection = use_context_provider(|| Selection(Signal::new(None)));
    let mut selected = selection.0;

    let player = use_context_provider(|| Player {
        queue: Signal::new(Vec::new()),
        index: Signal::new(0),
        playing: Signal::new(false),
        position: Signal::new((0.0, 0.0)),
        quality: Signal::new(crate::ui::prefs::Prefs::load().quality()),
        status: Signal::new(None),
        volume: Signal::new(1.0),
        muted: Signal::new(false),
        queue_open: Signal::new(false),
        devices: Signal::new(Vec::new()),
        output: Signal::new(None),
        me: Signal::new(None),
    });

    let library = use_context_provider(Library::new);

    let generator = use_context_provider(Generator::new);
    let crawler = use_context_provider(Crawler::new);
    let pipeline = use_context_provider(Pipeline::new);

    // The crawl and pipeline publish status rather than writing these signals,
    // a server cannot reach into a Dioxus scope.
    crate::ui::crawler::use_crawl_status(crawler);
    crate::ui::pipeline::use_pipeline_watch(pipeline);

    // One menu for every row in the window. Rows open it; it performs the
    // action itself, so no row has to carry a popup or a set of callbacks.
    use_context_provider(|| ContextMenu(Signal::new(None)));

    // The map, opened on purpose, over the main area, by its floating button.
    // Most of the time you know what you are looking for and type it; the map
    // is for the times you do not.
    let mut map_open = use_signal(|| false);

    // Whether the map is showing a route, which is also what decides the
    // dimming. Opened from the toolbar it is a place to browse and every
    // point stays lit; opened *by* a result it is there to show that result's
    // shape, and everything else drops back so the line can be followed.
    let mut map_route = use_signal(|| false);
    use_context_provider(|| MapView { map_open, map_route });

    // A rebuilt space means the loaded one is stale. Remotely the bytes have
    // to come down first; `sync_space` is a no-op locally, so this stays one
    // path.
    use_effect(move || {
        let generation = *pipeline.generation.read();

        spawn(async move {
            if generation > 0 {
                match backend().sync_space().await {
                    Ok(_) => match crate::reload_engine(data_dir(), db_path()) {
                        Ok(()) => {
                            selected.set(None);
                            generator.clear();
                            document::eval(
                                "window.twoKhzReloadPoints && window.twoKhzReloadPoints();",
                            );
                        }
                        Err(err) => eprintln!("could not reload the rebuilt space: {err:#}"),
                    },
                    Err(err) => eprintln!("could not sync the rebuilt space: {err:#}"),
                }
            }

            // Only the shell sees the engine, and "how many points are drawn"
            // is about the loaded space, not the database.
            let (in_space, on_map) = {
                let guard = engine().lock().unwrap();
                let catalog = &guard.navigator.catalog;
                (
                    catalog.visible().count() as i64,
                    catalog
                        .visible()
                        .filter(|&i| catalog.get(i).x.is_some())
                        .count() as i64,
                )
            };
            let mut pipeline = pipeline;
            pipeline.space_counts.set((in_space, on_map));
        });
    });

    // Write, refresh the engine's filter, redraw, all three, or the halves
    // disagree. The engine is told the ids because remotely there is no local
    // database to re-read.
    let blocked = use_signal(Vec::new);

    use_future(move || async move {
        let mut blocked = blocked;
        if let Ok(found) = backend().blocked_artists().await {
            engine()
                .lock()
                .unwrap()
                .set_blocked(found.iter().map(|a| a.artist_id).collect());
            blocked.set(found);
        }
    });

    let apply_block = use_callback(move |(artist_id, name): (i64, String)| {
        spawn(async move {
            let mut blocked = blocked;
            if let Err(err) = backend().block_artist(artist_id, &name).await {
                eprintln!("could not hide artist {artist_id}: {err:#}");
                return;
            }
            if let Ok(found) = backend().blocked_artists().await {
                engine()
                    .lock()
                    .unwrap()
                    .set_blocked(found.iter().map(|a| a.artist_id).collect());
                blocked.set(found);
            }
            document::eval("window.twoKhzReloadPoints && window.twoKhzReloadPoints();");
        });
    });

    let lift_block = use_callback(move |artist_id: i64| {
        spawn(async move {
            let mut blocked = blocked;
            if let Err(err) = backend().unblock_artist(artist_id).await {
                eprintln!("could not unhide artist {artist_id}: {err:#}");
                return;
            }
            if let Ok(found) = backend().blocked_artists().await {
                engine()
                    .lock()
                    .unwrap()
                    .set_blocked(found.iter().map(|a| a.artist_id).collect());
                blocked.set(found);
            }
            document::eval("window.twoKhzReloadPoints && window.twoKhzReloadPoints();");
        });
    });

    use_context_provider(|| Blocklist {
        artists: blocked,
        block: apply_block,
        unblock: lift_block,
    });

    // Recomputed when the block list changes: a hidden track should stop
    // offering to locate itself on the map.
    let local_ids = use_memo(move || {
        blocked.read();
        pipeline.generation.read();
        Rc::new(engine().lock().unwrap().navigator.catalog.id_set())
    });
    use_context_provider(|| LocalIds(local_ids));
    let space_reach = use_memo(move || {
        blocked.read();
        pipeline.generation.read();
        Rc::new(engine().lock().unwrap().navigator.catalog.reach())
    });
    use_context_provider(|| SpaceReach(space_reach));

    // One lowercased copy of every track's searchable text, built when the
    // space is loaded rather than on every keystroke. The filter below used to
    // `format!` a fresh haystack per track per character typed; at ~28k tracks
    // that is an allocation storm where a scan would do.
    //
    // Indexed by catalog row, so `catalog.visible()` indexes straight into it.
    // Deliberately not filtered by the block list, that changes far more
    // often than the space does, and the filter applies it anyway.
    let haystacks = use_memo(move || {
        pipeline.generation.read();
        let guard = engine().lock().unwrap();
        let catalog = &guard.navigator.catalog;
        let rows: Vec<Haystack> = (0..catalog.len())
            .map(|i| {
                let track = catalog.get(i);
                Haystack {
                    artist: track.artist.to_lowercase(),
                    title: track.title.to_lowercase(),
                    album: track.album.to_lowercase(),
                }
            })
            .collect();
        Rc::new(rows)
    });

    crate::ui::use_transport(player);
    crate::ui::use_session(player);

    // Open on the user's own library, which is also what the crawl seeds from.
    use_future(move || async move {
        crate::ui::open_initial(library);
    });

    // Lazy cover loading, installed once and delegated from the document, so
    // it covers every list in the window including ones not yet rendered.
    use_future(move || async move {
        let mut handle = document::eval(include_str!("../assets/covers.js"));
        // Never resolves; keeps the observers alive for the session.
        let _ = handle.recv::<serde_json::Value>().await;
    });

    // Long press opens the row menu on a touch screen, where there is no
    // right-click to open it with.
    let mut menu = use_context::<ContextMenu>().0;
    use_future(move || async move {
        let mut handle = document::eval(include_str!("../assets/long-press.js"));

        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            if message.get("closeMap").is_some() {
                map_open.set(false);
                continue;
            }
            let Some(tag) = message.get("target").and_then(|v| v.as_str()) else {
                continue;
            };
            // A tag that does not parse means the row's address and this
            // parser have drifted apart; dropping it is better than opening
            // a menu on a guess.
            let Some(target) = crate::ui::MenuTarget::parse(tag) else {
                continue;
            };
            menu.set(Some(crate::ui::MenuState {
                x: message.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                y: message.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
                target,
            }));
        }
    });

    // The remote half of the search box. The local filter above runs on every
    // keystroke because it is a scan of memory; Qobuz is a network round trip
    // and must not, so this waits for the text to stop moving before asking.
    //
    // A polling loop rather than a cancellable timer task: the app already
    // keeps one of these for crawl and pipeline status, and the cancellation
    // semantics of a respawned Dioxus task are subtler than the 200ms of
    // latency this costs. Enter bypasses it entirely.
    use_future(move || async move {
        let mut search = search;
        let mut previous = String::new();

        loop {
            tokio::time::sleep(crate::ui::POLL).await;
            let current = search.text.peek().trim().to_string();

            // Stable for two ticks, i.e. the user has stopped typing.
            if current == previous {
                if current.is_empty() {
                    // Emptying the box empties the results; no round trip.
                    if !search.submitted.peek().is_empty() {
                        search.submitted.set(String::new());
                        library.clear_search();
                    }
                } else if current.chars().count() >= 2 && current != *search.submitted.peek() {
                    search.submitted.set(current.clone());
                    library.search_to(current.clone());
                }
            }

            previous = current;
        }
    });

    // Bulk point data crosses as binary here, never through eval.
    crate::platform::use_asset_handler("points", move |request, responder| {
        let guard = engine().lock().unwrap();
        let catalog = &guard.navigator.catalog;
        let path = request.uri().path().to_string();

        let (content_type, body) = if path.ends_with("/meta") {
            (
                "application/json",
                serde_json::to_vec(&map::meta(catalog)).unwrap_or_default(),
            )
        } else {
            ("application/octet-stream", map::payload(catalog))
        };

        responder.respond(
            Response::builder()
                .header("Content-Type", content_type)
                .body(body)
                .unwrap(),
        );
    });

    // Long-lived eval: runs the map, then waits for selection messages.
    use_future(move || async move {
        let mut handle = document::eval(include_str!("../assets/map.js"));
        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            // Match on `type` first. A menu message carries a `track_id` too,
            // so the selection arm below would otherwise swallow it and the
            // menu would never open.
            if message.get("type").and_then(|v| v.as_str()) == Some("menu") {
                if let Some(id) = message.get("track_id").and_then(|v| v.as_f64()) {
                    selected.set(Some(id as i64));
                    menu.set(Some(crate::ui::MenuState {
                        x: message.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        y: message.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        target: crate::ui::MenuTarget::SpaceTrack(id as i64),
                    }));
                }
                continue;
            }

            // An explicit null is the map saying the background was clicked.
            // Matched on the key being present rather than on the value
            // parsing, so a future message without one cannot clear the
            // selection by accident. Selecting stays on the map: the point's
            // card is the way on to its page.
            if let Some(value) = message.get("track_id") {
                selected.set(value.as_f64().map(|id| id as i64));
            }
        }
    });

    // Mirror the selection onto the map, it used to travel one way only,
    // canvas outward.
    use_effect(move || {
        // Recorded as well as called: map.js is installed by a `use_future`,
        // so for the first few hundred ms this does not exist yet and a bare
        // call would be swallowed by the `&&`.
        let script = format!(
            "window.twoKhzSelected = {0};\n\
             window.twoKhzSetSelected && window.twoKhzSetSelected({0});",
            selected()
                .map(|id| id.to_string())
                .unwrap_or_else(|| "null".into())
        );
        document::eval(&script);
    });

    // Push the generated route to the map. Ids only, which is what eval is
    // sized for.
    use_effect(move || {
        // A route dims every point that is not on it, which was fine while a
        // result lasted only until the next click. Now that results persist,
        // pushing one unconditionally would leave the map dimmed for good,
        // so the route exists on the map only while the map is being used to
        // look at that route.
        let ids: Vec<i64> = if map_route() {
            generator
                .result
                .read()
                .iter()
                .map(|s| s.track.track_id)
                .collect()
        } else {
            Vec::new()
        };
        let script = format!(
            "window.twoKhzSetRoute && window.twoKhzSetRoute({});",
            serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())
        );
        document::eval(&script);
    });

    /// The most rows the scan will hand over. The list shows fewer to begin
    /// with and grows on request; this is the ceiling on what is kept ready.
    const SHOWN: usize = 720;

    // Kept out of the render body: this runs on every keystroke, and inside
    // the body it also re-ran for every unrelated signal the shell touches.
    let filtered = use_memo(move || {
        // Subscribe, so hiding an artist empties them out of this list too.
        blocked.read();
        let terms: Vec<String> = query()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();

        let haystacks = haystacks.read();
        let guard = engine().lock().unwrap();
        let catalog = &guard.navigator.catalog;

        let row = |t: &crate::db::TrackMeta| SpaceRow {
            track_id: t.track_id,
            artist: t.artist.clone(),
            artist_id: Some(t.artist_id).filter(|id| *id >= 0),
            title: t.title.clone(),
            album: t.album.clone(),
            album_id: t.album_id.clone(),
        };

        let sort = *library.sort.read();

        if terms.is_empty() {
            let total = catalog.visible().count();
            // The first `SHOWN` are only the right ones unsorted; any other
            // order needs every row to pick them from.
            let rows: Vec<SpaceRow> = if sort == Sort::default() {
                catalog.visible().map(|i| catalog.get(i)).take(SHOWN).map(row).collect()
            } else {
                let rows = catalog.visible().map(|i| catalog.get(i)).map(row).collect();
                let mut rows = sort.space_rows(rows);
                rows.truncate(SHOWN);
                rows
            };
            (rows, total)
        } else {
            let first = terms[0].as_str();
            let mut found: Vec<(u8, SpaceRow)> = Vec::new();

            for i in catalog.visible() {
                let Some(haystack) = haystacks.get(i) else {
                    // The space was rebuilt under us; the memo is about to
                    // run again with matching rows.
                    continue;
                };
                // Title only: a track found through its artist's or album's
                // name is that artist's or album's tile to show.
                if !names_track(&terms, &haystack.title) {
                    continue;
                }
                // Something starting with what was typed is far likelier to
                // be the thing meant than something merely containing it.
                let rank = if haystack.artist.starts_with(first)
                    || haystack.title.starts_with(first)
                {
                    0
                } else {
                    1
                };
                found.push((rank, row(catalog.get(i))));
            }

            let total = found.len();
            found.sort_by_key(|entry| entry.0);
            let rows = found.into_iter().map(|(_, row)| row).collect();
            let mut rows = sort.space_rows(rows);
            rows.truncate(SHOWN);
            (rows, total)
        }
    });

    use_context_provider(|| SpaceMatches(filtered));

    let map = MapView { map_open, map_route };
    let mut panel_open = generator.panel_open;
    let searching = matches!(&*library.view.read(), View::Search { .. });

    rsx! {
        div { class: "app",
            // Everything above the player: the main area and, beside it, the
            // generate panel. The queue drawer opens over both.
            div { class: "workspace",
                main { class: "main",
                    MainScreen {}

                    // Over the main area only, never the panel beside it.
                    // Hidden, never unmounted, and never moved in this tree:
                    // map.js caches the canvas node, its 2d context, its
                    // listeners and a ResizeObserver at eval time and has no
                    // re-init path, so a conditional render would leave it
                    // drawing into a detached node.
                    div { class: if map_open() { "map-wrap" } else { "map-wrap hidden" },
                        canvas { id: "map" }
                        MapCard {}
                        if map_route() {
                            button {
                                class: "chip active map-route",
                                title: "stop isolating the result",
                                onclick: move |_| map_route.set(false),
                                "showing a route ×"
                            }
                        }
                    }

                    // Bottom right of the main area, over the map as well,
                    // since the map's own is how you close it again.
                    div { class: "fabs",
                        // Phone only: the panel is always showing otherwise.
                        button {
                            class: "fab fab-generate",
                            title: "generate",
                            onclick: move |_| panel_open.set(true),
                            {icons::spark()}
                        }
                        button {
                            class: if searching { "fab fab-search active" } else { "fab fab-search" },
                            title: if searching { "close search" } else { "search" },
                            onclick: move |_| {
                                map.close();
                                library.toggle_search(search.text.peek().trim().to_string());
                            },
                            {icons::search()}
                        }
                        button {
                            class: if map_open() { "fab fab-map active" } else { "fab fab-map" },
                            title: if map_open() { "close the map" } else { "the map" },
                            onclick: move |_| map.toggle(),
                            {icons::globe()}
                        }
                    }
                }

                GeneratePanel {}

                QueueView {}
            }

            // A half-built path survives the panel being slid away on a phone.
            PathPill {}

            PlayerBar {}

            // Last, so it paints over everything it can be opened from.
            ContextMenuView {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Haystack;

    fn haystack(artist: &str, title: &str, album: &str) -> Haystack {
        Haystack {
            artist: artist.to_lowercase(),
            title: title.to_lowercase(),
            album: album.to_lowercase(),
        }
    }

    /// The filter used to test terms against `format!("{artist} {title} {album}")`.
    /// Testing each field separately is the same predicate only because terms
    /// are split on whitespace and so can never span the joins, if that ever
    /// stops being true, this test is where it shows up.
    #[test]
    fn per_field_matches_the_joined_string() {
        let cases = [
            ("Aphex Twin", "Xtal", "Selected Ambient Works"),
            ("Burial", "Near Dark", "Untrue"),
            ("", "Untitled", ""),
        ];

        for (artist, title, album) in cases {
            let subject = haystack(artist, title, album);
            let joined = format!(
                "{} {} {}",
                artist.to_lowercase(),
                title.to_lowercase(),
                album.to_lowercase()
            );

            for term in ["aph", "xtal", "works", "near", "untrue", "zzz", "twin"] {
                assert_eq!(
                    subject.contains(term),
                    joined.contains(term),
                    "{term:?} against {artist:?}/{title:?}/{album:?}"
                );
            }
        }
    }

    /// The one case where the two differ, and why splitting on whitespace
    /// before matching keeps the difference unreachable.
    #[test]
    fn a_term_spanning_two_fields_cannot_be_produced_by_splitting() {
        let subject = haystack("Aphex Twin", "Xtal", "Ambient");
        // The joined string contains "twin xtal"; no single field does.
        assert!(!subject.contains("twin xtal"));
        assert!("aphex twin xtal ambient".contains("twin xtal"));
        // But a query is split first, so "twin xtal" is never one term.
        let terms: Vec<&str> = "twin xtal".split_whitespace().collect();
        assert_eq!(terms, ["twin", "xtal"]);
        assert!(terms.iter().all(|term| subject.contains(term)));
    }
}
