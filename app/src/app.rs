//! The root component, and the startup it needs.
//!
//! In the library rather than `main.rs` because an Android build has no
//! `main`: the APK loads the .so and calls `start_app`.

use crate::api::{Device, Scope, SpaceIndex, SpaceInfo, SpaceSort};
use crate::backend::{self, backend};
use crate::ui::{
    icons, Blocklist, ContextMenu, ContextMenuView, Cover, Crawler, GeneratePanel, Generator,
    Library, LocalIds, MainScreen, MapView, Me, PathPill, Pipeline, Player, PlayerBar, QueueView,
    ReleaseNotice, Releases, Search, Selection, Space, SpaceMatches, SpaceReach, SpaceRow, View,
    Weights,
};
use crate::Wiring;
use crate::ui::library::Sort;
use crate::platform::wry::http::Response;
use dioxus::prelude::*;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Mutex;

/// Wire up the backend from the environment or a stored pairing. Fallible but
/// not fatal: an unpaired client opens on the setup screen. Nothing here
/// touches the network, so a paired client opens straight onto the app.
pub fn bootstrap() -> anyhow::Result<()> {
    crate::cache::forget_synced_space();
    wire(Wiring::from_env()?);
    Ok(())
}

fn wire(wiring: Wiring) {
    backend::init(wiring.into_backend());
}

/// Where the root is: asking for a pairing, or the app.
#[derive(Clone, PartialEq)]
enum Phase {
    /// With why the last attempt failed, if there was one.
    Setup(Option<String>),
    Ready,
}

/// The root: either the app, or the screen that gets you to it.
///
/// Separate components rather than an early return, Dioxus counts hooks per
/// scope, and changing that count between renders panics.
#[component]
pub fn App() -> Element {
    let phase = use_signal(|| {
        if backend::wired() {
            Phase::Ready
        } else {
            Phase::Setup(None)
        }
    });
    use_context_provider(|| phase);

    rsx! {
        style { {include_str!("../assets/style.css")} }
        match phase() {
            Phase::Ready => rsx! { Shell {} },
            Phase::Setup(error) => rsx! { Setup { phase, error } },
        }
    }
}

/// The server not answering, bottom left of the main area, opposite the
/// buttons. The app carries on from what it has cached; this says why
/// nothing new is arriving.
#[component]
fn OfflineNotice() -> Element {
    let offline = use_context::<Space>().offline;
    let Some(reason) = offline() else {
        return rsx! {};
    };
    rsx! {
        div { class: "sync-notice muted", title: "{reason}", "server unreachable, retrying…" }
    }
}

/// Pairing, for a device not configured through the environment. The address
/// and token are typed once and kept in the client's data directory.
#[component]
fn Setup(phase: Signal<Phase>, error: Option<String>) -> Element {
    let mut address = use_signal(|| {
        crate::ServerConfig::load()
            .map(|c| c.base)
            .unwrap_or_else(|| "http://".into())
    });
    let mut token = use_signal(String::new);
    let mut status = use_signal(|| error);
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

            wire(Wiring::remote(base, secret));
            // One cheap request, so a wrong address or token is said here
            // rather than by every panel of an app that cannot reach anything.
            match backend().space_info().await {
                Ok(_) => phase.set(Phase::Ready),
                Err(err) => {
                    status.set(Some(format!("{err:#}")));
                    busy.set(false);
                }
            }
        });
    };

    rsx! {
        div { class: "setup",
            h1 { "Connect to your server" }
            p { class: "muted",
                "The space, the catalogue, playback and the pipeline live on a machine that \
                 can host them, and this device asks it for everything. Pair one with "
                code { "stelly-server pair --name phone --scope play" }
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

    let track = selected()
        .and_then(crate::ui::space_row)
        .map(|meta| crate::ui::generate::as_remote(&meta));
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
    use_context_provider(|| Weights(generator.weights));
    let crawler = use_context_provider(Crawler::new);
    let pipeline = use_context_provider(Pipeline::new);

    // What is known of the space: what the disk had at once, then whatever
    // the server says behind the window. Nothing waits on the network to open.
    let space = use_context_provider(|| {
        let cached = crate::cache::read::<CachedSpace>(SPACE_CACHE).unwrap_or_default();
        load_cached_points();
        Space {
            info: Signal::new(cached.info),
            index: Signal::new(Rc::new(cached.index)),
            offline: Signal::new(None),
            seen: Signal::new(0),
        }
    });

    // The blocks decide the sliders, and the server whether steering is on.
    use_effect(move || {
        let info = space.info.read();
        let mut generator = generator;
        let mut pipeline = pipeline;
        generator.tower.set(info.can_steer);
        pipeline.space_counts.set((info.in_space, info.on_map));

        // Kept across a rebuild with the same blocks, so a tuned space stays
        // tuned; reset when the blocks themselves changed.
        let defaults: std::collections::HashMap<String, f32> = info
            .blocks
            .iter()
            .map(|block| (block.name.clone(), block.default_weight))
            .collect();
        let same_blocks = {
            let current = generator.weights.peek();
            current.len() == defaults.len() && current.keys().all(|name| defaults.contains_key(name))
        };
        if !same_blocks {
            generator.weights.set(defaults);
        }
    });

    // The crawl and pipeline publish status rather than writing these signals,
    // a server cannot reach into a Dioxus scope.
    crate::ui::crawler::use_crawl_status(crawler);
    crate::ui::pipeline::use_pipeline_watch(pipeline);

    // One menu for every row in the window. Rows open it; it performs the
    // action itself, so no row has to carry a popup or a set of callbacks.
    use_context_provider(|| ContextMenu(Signal::new(None)));
    Releases::provide();

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

    // Asked for again whenever the pipeline says the space moved, an artist
    // is hidden, or the server comes back. Only the stamp is asked first: the
    // index costs more, and only a changed stamp is worth it.
    let mut nudge = use_signal(|| 0u64);
    let mut phase = use_context::<Signal<Phase>>();
    use_future(move || async move {
        let mut space = space;
        let mut done: Option<(u64, u64)> = None;
        let mut wait = std::time::Duration::from_secs(2);
        loop {
            let want = (*pipeline.generation.peek(), *nudge.peek());
            if done != Some(want) {
                match refresh_space(space, selected, generator).await {
                    Ok(()) => {
                        done = Some(want);
                        wait = std::time::Duration::from_secs(2);
                        if space.offline.peek().is_some() {
                            space.offline.set(None);
                        }
                    }
                    Err(err) if backend::is_unauthorised(&err) => {
                        phase.set(Phase::Setup(Some(format!("{err:#}"))));
                        return;
                    }
                    Err(err) => {
                        space.offline.set(Some(format!("{err:#}")));
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(std::time::Duration::from_secs(30));
                        continue;
                    }
                }
            }
            tokio::time::sleep(crate::ui::POLL).await;
        }
    });

    // The map's points, fetched the first time the map is opened on a stamp
    // they do not match rather than with every refresh: on a phone that is
    // a megabyte most sessions never look at.
    let mut fetching_points = use_signal(|| false);
    use_effect(move || {
        let stamp = space.info.read().stamp.clone();
        if !map_open() || stamp.is_empty() || points_stamp().as_deref() == Some(stamp.as_str()) {
            return;
        }
        if *fetching_points.peek() {
            return;
        }
        fetching_points.set(true);
        spawn(async move {
            let fetched = tokio::try_join!(backend().space_points(false), backend().space_points(true));
            if let Ok((data, meta)) = fetched {
                crate::cache::write_bytes(POINTS_CACHE, &data);
                crate::cache::write_bytes(POINTS_META_CACHE, &meta);
                crate::cache::write_bytes(POINTS_STAMP_CACHE, stamp.as_bytes());
                *POINTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(Points { stamp, data, meta });
                document::eval("window.stellyReloadPoints && window.stellyReloadPoints();");
            }
            fetching_points.set(false);
        });
    });

    // Write, then ask the space again: hiding moves its stamp, which brings
    // down the index and the map without the hidden artist.
    let blocked = use_signal(Vec::new);

    use_future(move || async move {
        let mut blocked = blocked;
        if let Ok(found) = backend().blocked_artists().await {
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
                blocked.set(found);
            }
            *nudge.write() += 1;
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
                blocked.set(found);
            }
            *nudge.write() += 1;
        });
    });

    // From the disk first, like the favourites, so the controls a `play`
    // device is refused are not drawn while the server is asked again.
    let me = use_signal(|| crate::cache::read::<Device>(ME_CACHE));
    use_context_provider(|| Me(me));
    use_future(move || async move {
        let mut me = me;
        if let Ok(found) = backend().me().await {
            crate::cache::write(ME_CACHE, &found);
            me.set(Some(found));
        }
    });
    // A server older than users answers no `me`, and let any device hide.
    let editable = use_memo(move || me.read().as_ref().is_none_or(|device| device.scope == Scope::Pipeline));

    use_context_provider(|| Blocklist {
        artists: blocked,
        block: apply_block,
        unblock: lift_block,
        editable,
    });

    // Follows the index, which the server rebuilds when an artist is hidden:
    // a hidden track should stop offering to locate itself on the map.
    let local_ids = use_memo(move || Rc::new(space.index.read().ids.iter().copied().collect::<HashSet<i64>>()));
    use_context_provider(|| LocalIds(local_ids));
    let space_reach = use_memo(move || {
        let index = space.index.read();
        Rc::new((
            index.albums.iter().cloned().collect::<HashSet<String>>(),
            index.artists.iter().copied().collect::<HashSet<i64>>(),
        ))
    });
    use_context_provider(|| SpaceReach(space_reach));

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

    // The cover flying between a tile and the page it opens. Watches the
    // document on its own; nothing here tells it when a page changes.
    use_future(move || async move {
        let mut handle = document::eval(include_str!("../assets/transitions.js"));
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

    // Back and forward, from Android's back key and the mouse's side buttons.
    // Back closes whatever is over the page before it leaves the page. The
    // key only reaches here while back.js is armed, so it is armed whenever
    // there is something back would do, and after each press the answer is
    // given again: a back that leaves more to go back to changes nothing
    // the effect below would notice.
    let can_back = move || {
        menu.read().is_some()
            || *player.queue_open.read()
            || *generator.panel_open.read()
            || map_open()
            || !library.history.read().is_empty()
    };
    let arm_back = |want: bool| {
        document::eval(&format!("window.stellyBackArm && window.stellyBackArm({want});"));
    };
    use_effect(move || arm_back(can_back()));
    use_future(move || async move {
        let mut handle = document::eval(include_str!("../assets/back.js"));
        arm_back(can_back());

        let mut queue_open = player.queue_open;
        let mut panel_open = generator.panel_open;
        let map = MapView { map_open, map_route };
        // What each back did, latest last, so forward can undo it: step back
        // into the page, or open again what back closed.
        let mut undone: Vec<Undone> = Vec::new();
        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            let narrow = message.get("narrow").and_then(|v| v.as_bool()).unwrap_or(false);
            match message.get("type").and_then(|v| v.as_str()) {
                Some("back") => {
                    // The menu is not reopened: it belongs to where it was
                    // opened from, and is quicker opened again than found.
                    if menu.peek().is_some() {
                        menu.set(None);
                    } else if *queue_open.peek() {
                        queue_open.set(false);
                        undone.push(Undone::Queue);
                    } else if *panel_open.peek() && narrow {
                        panel_open.set(false);
                        undone.push(Undone::Panel);
                    } else if *map_open.peek() {
                        undone.push(Undone::Map { route: *map_route.peek() });
                        map.close();
                    } else {
                        // Docked on a wide screen, the panel was never in
                        // the way; left open it would keep the key armed.
                        panel_open.set(false);
                        if !library.history.peek().is_empty() {
                            library.back();
                            undone.push(Undone::Page);
                        }
                    }
                }
                Some("forward") => match undone.pop() {
                    Some(Undone::Queue) => queue_open.set(true),
                    Some(Undone::Panel) => panel_open.set(true),
                    Some(Undone::Map { route: true }) => map.show_route(),
                    Some(Undone::Map { route: false }) => map.browse(),
                    // Going somewhere new since forgets the way forward, as a
                    // browser does, and what back closed on the way there.
                    Some(Undone::Page) if !library.can_forward() => undone.clear(),
                    Some(Undone::Page) => {
                        map.close();
                        library.forward();
                    }
                    // Nothing back closed: the page history, which the top
                    // bar's arrow also steps back through.
                    None if library.can_forward() => {
                        map.close();
                        library.forward();
                    }
                    None => {}
                },
                _ => {}
            }
            arm_back(can_back());
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

    // Bulk point data crosses as binary here, never through eval. Answered
    // from what is held, never by waiting on the server: until the points
    // arrive the map is empty, and `stellyReloadPoints` draws them when they do.
    crate::platform::use_asset_handler("points", move |request, responder| {
        let path = request.uri().path().to_string();
        let held = POINTS.lock().unwrap_or_else(|e| e.into_inner());

        let (content_type, body) = if path.ends_with("/meta") {
            (
                "application/json",
                held.as_ref().map(|points| points.meta.clone()).unwrap_or_else(|| EMPTY_META.as_bytes().to_vec()),
            )
        } else {
            (
                "application/octet-stream",
                held.as_ref().map(|points| points.data.clone()).unwrap_or_default(),
            )
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
            "window.stellySelected = {0};\n\
             window.stellySetSelected && window.stellySetSelected({0});",
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
            "window.stellySetRoute && window.stellySetRoute({});",
            serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())
        );
        document::eval(&script);
    });

    /// The most rows the scan will hand over. The list shows fewer to begin
    /// with and grows on request; this is the ceiling on what is kept ready.
    const SHOWN: usize = 720;

    // The space's tracks matching the search box, asked of the server once
    // the text stops moving, and only while a search is showing: the list is
    // the search screen's, and nothing else needs it fetched.
    let mut filtered = use_signal(|| (Vec::<SpaceRow>::new(), 0usize));
    use_future(move || async move {
        let mut asked: Option<(String, Sort, String)> = None;
        loop {
            tokio::time::sleep(crate::ui::POLL).await;
            if !matches!(&*library.view.peek(), View::Search { .. }) {
                continue;
            }
            let text = query.peek().trim().to_lowercase();
            let wanted = (text.clone(), *library.sort.peek(), space.info.peek().stamp.clone());
            if asked.as_ref() == Some(&wanted) {
                continue;
            }
            // Stable for two ticks, i.e. the user has stopped typing.
            tokio::time::sleep(crate::ui::POLL).await;
            if query.peek().trim().to_lowercase() != text {
                continue;
            }

            let (sort, descending) = space_sort(wanted.1);
            match backend().space_search(&text, sort, descending, SHOWN).await {
                Ok(found) => {
                    crate::ui::remember(&found.tracks);
                    let rows = found
                        .tracks
                        .iter()
                        .map(|t| SpaceRow {
                            track_id: t.track_id,
                            artist: t.artist.clone(),
                            artist_id: Some(t.artist_id).filter(|id| *id >= 0),
                            title: t.title.clone(),
                            album: t.album.clone(),
                            album_id: t.album_id.clone(),
                        })
                        .collect();
                    filtered.set((rows, found.total));
                    asked = Some(wanted);
                }
                // Offline, or no space yet: the refresh loop says so, and
                // this tries again once things have had time to change.
                Err(_) => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
            }
        }
    });

    use_context_provider(|| SpaceMatches(filtered));

    // Whether the layout is the phone's, where the generate panel slides over
    // the main area rather than sitting docked beside it. `panel_open` alone
    // cannot say: generating opens it on any screen.
    let mut narrow = use_signal(|| false);
    use_future(move || async move {
        let mut handle = document::eval(
            "const query = window.matchMedia('(max-width: 900px)');
             dioxus.send(query.matches);
             query.addEventListener('change', (event) => dioxus.send(event.matches));
             await new Promise(() => {});",
        );
        while let Ok(matches) = handle.recv::<bool>().await {
            narrow.set(matches);
        }
    });

    let map = MapView { map_open, map_route };
    let mut panel_open = generator.panel_open;
    let covered = narrow() && panel_open();

    rsx! {
        div { class: "app",
            ReleaseNotice {}

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

                    OfflineNotice {}

                    // Bottom right of the main area, over the map as well,
                    // since its own button is how you close it again. On a
                    // phone, over the generate panel too: the map is the main
                    // area's, so from there it slides the panel away first,
                    // and answers to what is under it only once it can be
                    // seen.
                    div { class: "fabs",
                        // Phone only: the panel is always showing otherwise.
                        button {
                            class: if covered { "fab fab-generate active" } else { "fab fab-generate" },
                            title: if covered { "close generate" } else { "generate" },
                            onclick: move |_| panel_open.set(!covered),
                            {icons::spark()}
                        }
                        button {
                            class: if map_open() && !covered { "fab fab-map active" } else { "fab fab-map" },
                            title: if map_open() && !covered { "close the map" } else { "the map" },
                            onclick: move |_| {
                                if covered {
                                    panel_open.set(false);
                                    if !map_open() {
                                        map.browse();
                                    }
                                } else {
                                    map.toggle();
                                }
                            },
                            {icons::constellation()}
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

/// The space as last heard of, for the next start to open with.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct CachedSpace {
    info: SpaceInfo,
    index: SpaceIndex,
}

const SPACE_CACHE: &str = "space.json";
const POINTS_CACHE: &str = "points.bin";
const POINTS_META_CACHE: &str = "points-meta.json";
const POINTS_STAMP_CACHE: &str = "points.stamp";
const ME_CACHE: &str = "me.json";

/// The map's points as `map.js` reads them, and the stamp they were cut at.
/// A global because the asset handler answers outside any scope.
struct Points {
    stamp: String,
    data: Vec<u8>,
    meta: Vec<u8>,
}

static POINTS: Mutex<Option<Points>> = Mutex::new(None);

/// What `/points/meta` says before there are any points: nothing to draw.
const EMPTY_META: &str = r#"{"n":0,"genres":[],"labels":[],"bounds":{"min_x":0,"max_x":0,"min_y":0,"max_y":0},"has_layout":false}"#;

/// Last session's points, so the map draws at once even offline.
fn load_cached_points() {
    let cached = (|| {
        Some(Points {
            stamp: String::from_utf8(crate::cache::read_bytes(POINTS_STAMP_CACHE)?).ok()?,
            data: crate::cache::read_bytes(POINTS_CACHE)?,
            meta: crate::cache::read_bytes(POINTS_META_CACHE)?,
        })
    })();
    if cached.is_some() {
        *POINTS.lock().unwrap_or_else(|e| e.into_inner()) = cached;
    }
}

fn points_stamp() -> Option<String> {
    POINTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|points| points.stamp.clone())
}

/// Ask what the space looks like now, and bring the index down if it moved.
/// A rebuild keeps the selection and the result where it can: both are track
/// ids, so only a track the rebuild dropped has to go, and the result is kept
/// but marked out of date, the distances behind it having moved.
async fn refresh_space(
    space: Space,
    mut selected: Signal<Option<i64>>,
    generator: Generator,
) -> anyhow::Result<()> {
    let mut space = space;
    let info = backend().space_info().await?;
    let previous = space.info.peek().stamp.clone();

    if info.stamp != space.index.peek().stamp {
        let index = if info.stamp.is_empty() {
            SpaceIndex::default()
        } else {
            backend().space_index().await?
        };
        if selected.peek().is_some_and(|id| !index.ids.contains(&id)) {
            selected.set(None);
        }
        space.index.set(Rc::new(index));
    }

    if info.stamp != previous {
        crate::ui::forget_known();
        if !previous.is_empty() {
            generator.invalidate();
        }
    }
    if *space.info.peek() != info {
        space.info.set(info);
    }

    crate::cache::write(
        SPACE_CACHE,
        &CachedSpace {
            info: space.info.peek().clone(),
            index: (**space.index.peek()).clone(),
        },
    );
    Ok(())
}

/// The library's sort, as the space search on the server understands it.
/// Only title, artist and album mean anything for a space track; "original
/// order" is the server's own, reversed when asked.
fn space_sort(sort: Sort) -> (SpaceSort, bool) {
    use crate::ui::library::SortKey;
    match sort.key {
        SortKey::Title => (SpaceSort::Title, sort.descending),
        SortKey::Artist => (SpaceSort::Artist, sort.descending),
        SortKey::Album => (SpaceSort::Album, sort.descending),
        SortKey::Default => (SpaceSort::Rank, sort.descending),
        _ => (SpaceSort::Rank, false),
    }
}

/// Something a back took away, for forward to give back.
enum Undone {
    Page,
    Queue,
    Panel,
    Map { route: bool },
}
