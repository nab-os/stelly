//! The root component, and the startup it needs.
//!
//! In the library rather than `main.rs` because an Android build has no
//! `main`: the APK loads the .so and calls `start_app`.

use crate::api::{BlockedArtist, Device, Scope, SpaceIndex, SpaceInfo, SpaceSort};
use crate::backend::{self, backend};
use crate::catalogue::RemoteTrack;
use crate::ui::{
    icons, Blocklist, ContextMenu, ContextMenuView, Crawler, Family, Generator,
    Library, LocalIds, MainScreen, MapView, Me, PathPill, Provider, Pipeline, Player, PlayerBar, QueueView,
    ReleaseNotice, Releases, Search, Selection, Space, SpaceMatches, SpaceReach, SpaceRow, TitleBar,
    View, Weights,
};
use crate::Wiring;
use crate::ui::library::Sort;
use crate::ui::tour::{Tour, TourView};
use crate::platform::wry::http::Response;
use dioxus::core::Task;
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
    let epoch = use_signal(|| 0u64);
    use_context_provider(|| Pairing { phase, epoch });

    rsx! {
        style { {include_str!("../assets/style.css")} }
        // The desktop window has no system title bar, so its own sits above
        // whichever screen is showing, the setup one included.
        div { class: "frame",
            TitleBar {}
            div { class: "frame-body",
                match phase() {
                    // A list of one, keyed by the pairing: a key on a lone
                    // component is not diffed, a key in a list is, so moving
                    // to another server drops every screen, signal and task
                    // the old one fed and starts the shell afresh.
                    Phase::Ready => rsx! {
                        for epoch in [epoch()] {
                            Shell { key: "{epoch}" }
                        }
                    },
                    Phase::Setup(error) => rsx! { Setup { phase, error } },
                }
            }
        }
    }
}

/// Moving this device to another server, or off any, without a restart.
#[derive(Clone, Copy)]
pub(crate) struct Pairing {
    phase: Signal<Phase>,
    epoch: Signal<u64>,
}

impl Pairing {
    /// Save the pairing and start talking to it. The caller has already
    /// checked the server answers: this cannot fail short of the disk.
    pub(crate) fn switch(mut self, config: crate::ServerConfig) -> anyhow::Result<()> {
        config.save()?;
        wire(Wiring::remote(config.base, config.token));
        forget_server_state();
        *self.epoch.write() += 1;
        Ok(())
    }

    /// Forget the pairing and go back to the setup screen.
    pub(crate) fn forget(mut self) -> anyhow::Result<()> {
        crate::ServerConfig::clear()?;
        forget_server_state();
        self.phase.set(Phase::Setup(None));
        Ok(())
    }
}

/// What the process keeps outside any component about the server it talks
/// to. The disk caches are per pairing already and need nothing.
fn forget_server_state() {
    crate::ui::forget_known();
    *POINTS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `nas:7700` is how an address is typed more often than not.
pub(crate) fn normalise_address(typed: &str) -> String {
    let base = typed.trim().trim_end_matches('/');
    if base.is_empty() || base.contains("://") {
        base.to_string()
    } else {
        format!("http://{base}")
    }
}

/// Whether the environment names the server, which then wins over whatever
/// is saved at the next start.
pub(crate) fn pinned_by_env() -> bool {
    std::env::var("STELLY_SERVER").is_ok()
}

/// The server not answering, top centre of the main area. The app carries on
/// from what it has cached; this says why nothing new is arriving, loudly,
/// since generating, searching and playing all stop with it.
#[component]
fn OfflineNotice() -> Element {
    let offline = use_context::<Space>().offline;
    let Some(reason) = offline() else {
        return rsx! {};
    };
    rsx! {
        div { class: "sync-notice", title: "{reason}", role: "alert",
            span { class: "sync-notice-dot" }
            "Server unreachable"
            span { class: "muted", "retrying…" }
        }
    }
}

/// Pairing, for a device not configured through the environment. The address
/// and token are typed once and kept in the client's data directory.
#[component]
fn Setup(phase: Signal<Phase>, error: Option<String>) -> Element {
    let mut address = use_signal(|| crate::ServerConfig::load().map(|c| c.base).unwrap_or_default());
    let mut token = use_signal(String::new);
    let mut reveal = use_signal(|| false);
    let mut failure = use_signal(|| error);
    let mut busy = use_signal(|| false);

    let mut connect = move || {
        if *busy.peek() {
            return;
        }
        let base = normalise_address(&address.peek());
        let secret = token.peek().trim().to_string();
        if base.is_empty() || secret.is_empty() {
            failure.set(Some("Both the address and the token are needed.".into()));
            return;
        }
        address.set(base.clone());

        busy.set(true);
        failure.set(None);

        spawn(async move {
            let config = crate::ServerConfig {
                base: base.clone(),
                token: secret.clone(),
            };

            // Save first, so the next launch starts from what was typed here
            // rather than the pairing it just failed to reach.
            if let Err(err) = config.save() {
                failure.set(Some(format!("could not save the pairing: {err:#}")));
                busy.set(false);
                return;
            }

            wire(Wiring::remote(base, secret));
            // One cheap request, so a wrong address or token is said here
            // rather than by every panel of an app that cannot reach anything.
            match backend().space_info().await {
                Ok(_) => phase.set(Phase::Ready),
                Err(err) => {
                    failure.set(Some(format!("{err:#}")));
                    busy.set(false);
                }
            }
        });
    };
    let on_enter = move |event: KeyboardEvent| {
        if event.key() == Key::Enter {
            connect();
        }
    };

    rsx! {
        div { class: "setup",
            div { class: "setup-card",
                div { class: "setup-brand",
                    div { class: "setup-mark", {icons::constellation()} }
                    h1 { "Stelly" }
                    p { class: "muted", "Connect this device to your server." }
                }

                div { class: "setup-field",
                    label { r#for: "setup-address", "Server address" }
                    input {
                        id: "setup-address",
                        class: "search",
                        placeholder: "nas.tailnet.ts.net:7700",
                        value: "{address}",
                        autofocus: true,
                        oninput: move |event| address.set(event.value()),
                        onkeydown: on_enter,
                    }
                    span { class: "muted", "Where stelly-server listens." }
                }

                div { class: "setup-field",
                    label { r#for: "setup-token", "Device token" }
                    div { class: "secret",
                        input {
                            id: "setup-token",
                            class: "search",
                            r#type: if reveal() { "text" } else { "password" },
                            placeholder: "paste the token",
                            value: "{token}",
                            oninput: move |event| token.set(event.value()),
                            onkeydown: on_enter,
                        }
                        button {
                            class: "reveal",
                            onclick: move |_| reveal.set(!reveal()),
                            if reveal() { "hide" } else { "show" }
                        }
                    }
                }

                if let Some(message) = failure.read().clone() {
                    div { class: "setup-error", "{message}" }
                }

                button {
                    class: "primary setup-connect",
                    disabled: busy(),
                    onclick: move |_| connect(),
                    if busy() { "Connecting…" } else { "Connect" }
                }

                details { class: "setup-help",
                    summary { "No token yet?" }
                    p { class: "muted",
                        "Another of your devices, or an admin one, can pair this one from Settings, under Devices. \
                         Or, on the server:"
                    }
                    code { "stelly-server pair --name phone --scope play" }
                }
            }
        }
    }
}

/// The selected point's card on the map: cover, title, album, artist, and the
/// way to the track's page. map.js pins it over the point on every frame, so
/// it rides along with a pan or a zoom at the same size on screen.
///
/// Rendered once, empty, and never again: map.js fills it, from the point's
/// label the moment it is clicked, and completes it with the row the
/// selection effect in `Shell` hands over. Drawn from the selection here, it
/// showed a round trip to Rust late, and for a point not seen yet a fetch
/// later still. map.js also looks it up by id and owns its position and
/// visibility; a card that came and went would lose both between frames.
#[component]
fn MapCard() -> Element {
    let library = use_context::<Library>();
    let map = use_context::<MapView>();
    let selected = use_context::<Selection>().0;

    rsx! {
        div {
            id: "map-card",
            class: "map-card",
            // Peeked: the card is for whatever is selected when it is
            // clicked, and must not be drawn again when that changes.
            onclick: move |_| {
                if let Some(id) = *selected.peek() {
                    map.close();
                    crate::ui::open_track(library, selected, id);
                }
            },
            div { class: "cover thumb" }
            div { class: "map-card-text",
                span { class: "title" }
                span { class: "muted ellipsis" }
                span { class: "muted ellipsis" }
            }
        }
    }
}

/// The selected track's row as map.js wants it for the card: left where the
/// map can find it, as the selection is, and announced in case the card is
/// waiting for it.
fn card_row(track: &RemoteTrack) -> String {
    // Quotes and parens would break out of `url('…')`, see `Cover`.
    let image = track.image.as_deref().filter(|url| !url.contains(['\'', '(', ')']));
    let row = serde_json::json!({
        "track_id": track.id,
        "title": track.title,
        "album": track.album,
        "artist": track.artist,
        "image": image,
    });
    format!("window.stellyCard = {row};\nwindow.stellyCardArrived && window.stellyCardArrived();\n")
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
        volume: Signal::new(None),
        queue_open: Signal::new(false),
        devices: Signal::new(Vec::new()),
        output: Signal::new(None),
        me: Signal::new(None),
    });

    let library = use_context_provider(Library::new);
    use_context_provider(Tour::new);

    let generator = use_context_provider(|| Generator::new(player));
    use_context_provider(|| Weights(generator.weights));
    let crawler = use_context_provider(Crawler::new);
    let pipeline = use_context_provider(Pipeline::new);
    use_context_provider(Family::new);

    // What is known of the space: nothing for the first frame, then what the
    // disk had, then whatever the server says behind the window. Nothing
    // waits on the network to open, nor on two megabytes of JSON to paint.
    let space = use_context_provider(|| Space {
        info: Signal::new(SpaceInfo::default()),
        index: Signal::new(Rc::new(SpaceIndex::default())),
        offline: Signal::new(None),
        seen: Signal::new(0),
    });

    // This account's "tune the space", as kept on the server: asked once, and
    // again until it answers. A server from before tunings refuses the route,
    // and the weights are then this device's for the session, as they were.
    let mut saved_tuning = use_signal(|| None::<std::collections::HashMap<String, f32>>);
    let mut tuning_applied = use_signal(|| false);
    use_future(move || async move {
        loop {
            match backend().tuning().await {
                Ok(saved) => {
                    saved_tuning.set(Some(saved));
                    return;
                }
                Err(err) if backend::is_unreachable(&err) => {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                Err(_) => return,
            }
        }
    });

    // The blocks decide the sliders, and the server whether steering is on.
    use_effect(move || {
        let info = space.info.read();
        let saved = saved_tuning.read();
        let mut generator = generator;
        let mut pipeline = pipeline;
        generator.tower.set(info.can_steer);
        pipeline.space_counts.set((info.in_space, info.on_map));

        // Kept across a rebuild with the same blocks, so a tuned space stays
        // tuned; set again from the defaults and the account's own tuning
        // when the blocks themselves changed, or that tuning first arrives.
        let defaults: std::collections::HashMap<String, f32> = info
            .blocks
            .iter()
            .map(|block| (block.name.clone(), block.default_weight))
            .collect();
        let same_blocks = {
            let current = generator.weights.peek();
            current.len() == defaults.len() && current.keys().all(|name| defaults.contains_key(name))
        };
        let arrived = saved.is_some() && !*tuning_applied.peek();
        if !same_blocks || arrived {
            generator.weights.set(crate::ui::tuned(&defaults, saved.as_ref()));
        }
        if arrived {
            tuning_applied.set(true);
        }
    });

    // The crawl and pipeline publish status rather than writing these signals,
    // a server cannot reach into a Dioxus scope.
    crate::ui::crawler::use_crawl_status(crawler);
    crate::ui::pipeline::use_pipeline_watch(pipeline, space);

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
        // The disk's copy, read and parsed off the UI thread, before the
        // server is asked so its answer is never overwritten by an older one.
        if let Ok(Some(cached)) = tokio::task::spawn_blocking(|| crate::cache::read::<CachedSpace>(SPACE_CACHE)).await {
            space.index.set(Rc::new(cached.index));
            space.info.set(cached.info);
        }
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

    // The map's points, read and parsed only once the map is first opened:
    // the disk's copy at once, then the server's if the stamp moved. Ten
    // megabytes on disk and a few hundred ms of the webview's own thread,
    // which most sessions never look at and the favourites are painting on.
    let mut fetching_points = use_signal(|| false);
    // Whether the disk was read, and whether map.js has drawn any points yet.
    let mut points_read = use_signal(|| false);
    let mut points_shown = use_signal(|| false);
    use_effect(move || {
        let stamp = space.info.read().stamp.clone();
        if !map_open() || *fetching_points.peek() {
            return;
        }
        let current = stamp.is_empty() || points_stamp().as_deref() == Some(stamp.as_str());
        if *points_shown.peek() && current {
            return;
        }
        fetching_points.set(true);
        spawn(async move {
            if !*points_read.peek() {
                points_read.set(true);
                if let Ok(Some(cached)) = tokio::task::spawn_blocking(read_cached_points).await {
                    *POINTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(cached);
                    show_points().await;
                    points_shown.set(true);
                }
            }
            let stamp = space.info.peek().stamp.clone();
            let (held, binary) = held_points().unwrap_or_default();
            if !stamp.is_empty() && held != stamp {
                let fetched = tokio::try_join!(
                    backend().space_points(&held),
                    backend().space_meta(&held, binary)
                );
                // Either body still current means the stamp moved back to
                // the held points: they stay, as on a failure.
                if let Ok((Some(data), Some(meta))) = fetched {
                    // Ten megabytes, written off the UI thread, and before
                    // they are held: a second request for a body reads it
                    // back from here.
                    let points = Points::new(stamp, data, meta);
                    let points = tokio::task::spawn_blocking(move || {
                        write_points(&points);
                        points
                    })
                    .await;
                    let Ok(points) = points else {
                        fetching_points.set(false);
                        return;
                    };
                    *POINTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(points);
                    show_points().await;
                    points_shown.set(true);
                }
            }
            // Nothing on disk and nothing from the server: the map says so
            // rather than reading forever.
            if !*points_shown.peek() {
                show_points().await;
                points_shown.set(true);
            }
            fetching_points.set(false);
        });
    });

    // Write, then ask the space again: hiding moves its stamp, which brings
    // down the index and the map without the hidden artist.
    //
    // From the disk first, a few entries, so a hidden artist does not flash
    // into the favourites while the server is asked again.
    let blocked = use_signal(|| crate::cache::read::<Vec<BlockedArtist>>(BLOCKED_CACHE).unwrap_or_default());
    let heard_blocked = move |found: Vec<BlockedArtist>| {
        let mut blocked = blocked;
        crate::cache::write(BLOCKED_CACHE, &found);
        if *blocked.peek() != found {
            blocked.set(found);
        }
    };

    use_future(move || async move {
        if let Ok(found) = backend().blocked_artists().await {
            heard_blocked(found);
        }
    });

    let apply_block = use_callback(move |(artist_id, name): (i64, String)| {
        spawn(async move {
            if let Err(err) = backend().block_artist(artist_id, &name).await {
                eprintln!("could not hide artist {artist_id}: {err:#}");
                return;
            }
            if let Ok(found) = backend().blocked_artists().await {
                heard_blocked(found);
            }
            *nudge.write() += 1;
        });
    });

    let lift_block = use_callback(move |artist_id: i64| {
        spawn(async move {
            if let Err(err) = backend().unblock_artist(artist_id).await {
                eprintln!("could not unhide artist {artist_id}: {err:#}");
                return;
            }
            if let Ok(found) = backend().blocked_artists().await {
                heard_blocked(found);
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
    let provider = use_signal(|| crate::cache::read::<crate::catalogue::Capabilities>(PROVIDER_CACHE));
    let providers = use_signal(|| crate::cache::read::<Vec<crate::catalogue::Capabilities>>(PROVIDERS_CACHE).unwrap_or_default());
    use_context_provider(|| Provider(provider, providers));
    use_future(move || async move {
        let (mut provider, mut providers) = (provider, providers);
        // Together, as neither answer waits on the other.
        let (capabilities, found) = tokio::join!(backend().capabilities(), backend().providers());
        if let Ok(found) = capabilities {
            crate::cache::write(PROVIDER_CACHE, &found);
            provider.set(Some(found));
        }
        if let Ok(found) = found {
            crate::cache::write(PROVIDERS_CACHE, &found);
            providers.set(found);
        }
    });
    // A server older than users answers no `me`, and let any device hide.
    let editable = use_memo(move || me.read().as_ref().is_none_or(|device| device.scope == Scope::Admin));

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
    // Before the first frame, so it already has last time's favourites in it
    // rather than an empty home page; a read of a few hundred KB, and the
    // fetch behind it is `LibraryPanel`'s, after the window is up.
    use_hook(|| crate::ui::open_initial(library));

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
                    } else if *queue_open.peek() && narrow {
                        // A drawer over the page on a phone. Docked beside
                        // the page on a wide screen it is never in the way.
                        queue_open.set(false);
                        undone.push(Undone::Queue);
                    } else if *map_open.peek() {
                        undone.push(Undone::Map { route: *map_route.peek() });
                        map.close();
                    } else {
                        if !library.history.peek().is_empty() {
                            library.back();
                            undone.push(Undone::Page);
                        }
                    }
                }
                Some("forward") => match undone.pop() {
                    Some(Undone::Queue) => queue_open.set(true),
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
    // and must not, so each keystroke starts a short timer and cancels the
    // one before it: only the last keystroke asks. Enter bypasses it.
    let mut typed = use_hook(|| CopyValue::new(None::<Task>));
    use_effect(move || {
        let mut search = search;
        let current = search.text.read().trim().to_string();
        if let Some(task) = typed.take() {
            task.cancel();
        }
        // Emptying the box empties the results at once; no round trip.
        if current.is_empty() {
            if !search.submitted.peek().is_empty() {
                search.submitted.set(String::new());
                library.clear_search();
            }
            return;
        }
        if current.chars().count() < 2 || current == *search.submitted.peek() {
            return;
        }
        typed.set(Some(spawn(async move {
            tokio::time::sleep(crate::ui::DEBOUNCE).await;
            search.submitted.set(current.clone());
            library.search_to(current);
        })));
    });

    // Bulk point data crosses as binary here, never through eval. Answered
    // from what is held, never by waiting on the server: until the points
    // arrive the map is empty, and `stellyReloadPoints` draws them when they do.
    crate::platform::use_asset_handler("points", move |request, responder| {
        let path = request.uri().path().to_string();
        let mut held = POINTS.lock().unwrap_or_else(|e| e.into_inner());

        let (content_type, body) = if path.ends_with("/meta") {
            let body = serve(held.as_mut(), |points| &mut points.meta, POINTS_META_CACHE, EMPTY_META.as_bytes());
            let binary = crate::api::is_binary_map_meta(&body);
            (if binary { "application/octet-stream" } else { "application/json" }, body)
        } else {
            ("application/octet-stream", serve(held.as_mut(), |points| &mut points.data, POINTS_CACHE, &[]))
        };

        responder.respond(
            Response::builder()
                .header("Content-Type", content_type)
                .body(body)
                .unwrap(),
        );
    });

    // What the map itself last selected, so the effect below does not hand
    // the map back its own click.
    let mut from_map = use_hook(|| CopyValue::new(None::<Option<i64>>));
    // The same point again, a second click on it, changes nothing, and wakes
    // nothing.
    let mut select_from_map = move |id: Option<i64>| {
        if *selected.peek() != id {
            from_map.set(Some(id));
            selected.set(id);
        }
    };

    // Long-lived eval: runs the map, then waits for selection messages.
    use_future(move || async move {
        let script = format!("{}{}", crate::timings::script_flag(), include_str!("../assets/map.js"));
        let mut handle = document::eval(&script);
        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            // Only sent with timings on: the points read, and the first frame
            // drawn after the map was opened.
            if message.get("type").and_then(|v| v.as_str()) == Some("timing") {
                let name = message.get("name").and_then(|v| v.as_str()).unwrap_or("map");
                let detail = message.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                if name == "map-drawn" {
                    crate::timings::end(name, "map", detail);
                } else {
                    crate::timings::span(name, "map", detail);
                }
                continue;
            }

            // Match on `type` first. A menu message carries a `track_id` too,
            // so the selection arm below would otherwise swallow it and the
            // menu would never open.
            if message.get("type").and_then(|v| v.as_str()) == Some("menu") {
                if let Some(id) = message.get("track_id").and_then(|v| v.as_f64()) {
                    select_from_map(Some(id as i64));
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
                select_from_map(value.as_f64().map(|id| id as i64));
            }
        }
    });

    // Mirror the selection onto the map, it used to travel one way only,
    // canvas outward, and onto the rows showing it. The rows read it without
    // subscribing: a click on the map redrew every list holding a space
    // track, hidden under the map or not, for one class on one row. So this
    // moves the class itself, on whatever rows carry the track's
    // `data-space`, as `use_playing_highlight` does for the playing track.
    use_effect(move || {
        let id = selected();
        let shown = id.map(|id| id.to_string());
        // Recorded as well as called: map.js is installed by a `use_future`,
        // so for the first few hundred ms this does not exist yet and a bare
        // call would be swallowed by the `&&`. Not called for the map's own
        // click, which it has already drawn.
        let mut script = format!("window.stellySelected = {};\n", shown.as_deref().unwrap_or("null"));
        if from_map.take() != Some(id) {
            script.push_str("window.stellySetSelected && window.stellySetSelected(window.stellySelected);\n");
        }
        script.push_str(&format!(
            r#"(() => {{
                const id = "{}";
                for (const row of document.querySelectorAll("[data-space].selected")) {{
                    if (row.dataset.space !== id) row.classList.remove("selected");
                }}
                if (id) {{
                    for (const row of document.querySelectorAll(`[data-space="${{id}}"]`)) {{
                        row.classList.add("selected");
                    }}
                }}
            }})();
"#,
            shown.as_deref().unwrap_or_default()
        ));
        // The card's cover and album, which the map's labels do not carry.
        // A row not seen yet is asked for, and handed over if the track is
        // still the one selected when it arrives.
        if let Some(id) = id {
            match crate::ui::space_track(id) {
                Some(track) => script.push_str(&card_row(&track)),
                None => {
                    spawn(async move {
                        if let Some(meta) = crate::ui::space_meta(id).await {
                            if *selected.peek() == Some(id) {
                                document::eval(&card_row(&crate::ui::generate::as_remote(&meta)));
                            }
                        }
                    });
                }
            }
        }
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
    // the search screen's, and nothing else needs it fetched. A keystroke
    // cancels the ask before it, timer and request both.
    let mut filtered = use_signal(|| (Vec::<SpaceRow>::new(), 0usize));
    let on_search = use_memo(move || matches!(&*library.view.read(), View::Search { .. }));
    // What was last answered, and what is being asked now with the task
    // asking it. Neither is drawn from, so neither is a signal.
    let mut answered = use_hook(|| CopyValue::new(None::<(String, Sort, String)>));
    let mut asking = use_hook(|| CopyValue::new(None::<((String, Sort, String), Task)>));
    use_effect(move || {
        let text = query.read().trim().to_lowercase();
        let wanted = (text.clone(), *library.sort.read(), space.info.read().stamp.clone());
        let searching = on_search();
        if searching && asking.peek().as_ref().is_some_and(|(asked, _)| *asked == wanted) {
            return;
        }
        if let Some((_, task)) = asking.take() {
            task.cancel();
        }
        if !searching || answered.peek().as_ref() == Some(&wanted) {
            return;
        }
        let asked = wanted.clone();
        let task = spawn(async move {
            tokio::time::sleep(crate::ui::DEBOUNCE).await;
            let (sort, descending) = space_sort(asked.1);
            loop {
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
                                image: t.image.clone(),
                            })
                            .collect();
                        filtered.set((rows, found.total));
                        answered.set(Some(asked));
                        asking.set(None);
                        return;
                    }
                    // Offline, or no space yet: the refresh loop says so, and
                    // this tries again once things have had time to change.
                    Err(_) => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
                }
            }
        });
        asking.set(Some((wanted, task)));
    });

    use_context_provider(|| SpaceMatches(filtered));

    let map = MapView { map_open, map_route };

    // Left where map.js reads it on every rebuild of the cloud, so it holds
    // whether the points are read before this runs or after.
    let mut every_dot = use_signal(|| crate::ui::prefs::Prefs::load().every_dot);
    use_effect(move || {
        let on = every_dot();
        document::eval(&format!(
            "window.stellyEveryDot = {on}; window.stellySetEveryDot && window.stellySetEveryDot();"
        ));
    });

    // Once, after the first render: the window has the app in it now.
    use_effect(|| crate::timings::mark("shell-rendered", ""));

    rsx! {
        div { class: "app",
            ReleaseNotice {}

            // Everything above the player: the main area and, beside it, the
            // queue, when it is open. On a phone the queue is a drawer over
            // the main area instead.
            div { class: "workspace",
                main { class: "main",
                    MainScreen {}

                    // Over the main area only, never the queue beside it.
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
                        button {
                            class: if every_dot() { "chip active map-every-dot" } else { "chip map-every-dot" },
                            title: if every_dot() {
                                "group crowded patches again"
                            } else {
                                "draw every track, without thinning crowded patches"
                            },
                            onclick: move |_| {
                                let on = !every_dot();
                                every_dot.set(on);
                                crate::ui::prefs::update(|prefs| prefs.every_dot = on);
                            },
                            "every dot"
                        }
                    }

                    OfflineNotice {}

                    // Bottom right of the main area, over the map as well,
                    // since its own button is how you close it again.
                    div { class: "fabs",
                        button {
                            class: if map_open() { "fab fab-map active" } else { "fab fab-map" },
                            title: if map_open() { "close the map" } else { "the map" },
                            onclick: move |_| map.toggle(),
                            {icons::constellation()}
                        }
                    }
                }

                QueueView {}
            }

            // A path's start, while its end is looked for page to page.
            PathPill {}

            PlayerBar {}

            // Last, so it paints over everything it can be opened from.
            ContextMenuView {}

            // Over even that: while it shows, the app is only something to
            // point at.
            TourView {}
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
/// `map::meta_binary`'s, or JSON from a server before it. Named for the JSON.
const POINTS_META_CACHE: &str = "points-meta.json";
const POINTS_STAMP_CACHE: &str = "points.stamp";
const ME_CACHE: &str = "me.json";
const BLOCKED_CACHE: &str = "blocked.json";
const PROVIDER_CACHE: &str = "provider.json";
const PROVIDERS_CACHE: &str = "providers.json";

/// The map's points as `map.js` reads them, and the stamp they were cut at.
/// A global because the asset handler answers outside any scope.
///
/// The bodies are handed to the webview rather than copied for it, so each is
/// held only until map.js asks for it, which it does once per set of points.
struct Points {
    stamp: String,
    data: Option<Vec<u8>>,
    meta: Option<Vec<u8>>,
    /// Whether `meta` is `map::meta_binary`'s rather than JSON, which a
    /// server from before it sends. Kept for once `meta` is handed over.
    binary_meta: bool,
}

impl Points {
    fn new(stamp: String, data: Vec<u8>, meta: Vec<u8>) -> Self {
        let binary_meta = crate::api::is_binary_map_meta(&meta);
        Points { stamp, data: Some(data), meta: Some(meta), binary_meta }
    }
}

static POINTS: Mutex<Option<Points>> = Mutex::new(None);

/// What `/points/meta` says before there are any points: nothing to draw.
const EMPTY_META: &str = r#"{"n":0,"genres":[],"labels":[],"bounds":{"min_x":0,"max_x":0,"min_y":0,"max_y":0},"has_layout":false}"#;

/// Last session's points, so the map draws at once even offline. Ten
/// megabytes, so read off the UI thread.
fn read_cached_points() -> Option<Points> {
    Some(Points::new(
        String::from_utf8(crate::cache::read_bytes(POINTS_STAMP_CACHE)?).ok()?,
        crate::cache::read_bytes(POINTS_CACHE)?,
        crate::cache::read_bytes(POINTS_META_CACHE)?,
    ))
}

fn write_points(points: &Points) {
    let bodies = [(POINTS_CACHE, &points.data), (POINTS_META_CACHE, &points.meta)];
    for (name, body) in bodies {
        if let Some(body) = body {
            crate::cache::write_bytes(name, body);
        }
    }
    crate::cache::write_bytes(POINTS_STAMP_CACHE, points.stamp.as_bytes());
}

/// One of the held bodies, moved out for the webview. Asked for a second
/// time, which map.js does not do, it is read back from the disk the points
/// were written to; with no points at all, `empty`.
fn serve(
    held: Option<&mut Points>,
    body: impl FnOnce(&mut Points) -> &mut Option<Vec<u8>>,
    cached: &str,
    empty: &[u8],
) -> Vec<u8> {
    match held {
        Some(points) => body(points)
            .take()
            .or_else(|| crate::cache::read_bytes(cached))
            .unwrap_or_else(|| empty.to_vec()),
        None => empty.to_vec(),
    }
}

/// Have map.js read the points held now, and wait until it has: a second
/// set must not be swapped in while it is between its two requests.
async fn show_points() {
    let _ = document::eval("if (window.stellyReloadPoints) await window.stellyReloadPoints(); return true;").await;
}

fn points_stamp() -> Option<String> {
    held_points().map(|(stamp, _)| stamp)
}

/// The held points' stamp, and whether their meta is binary.
fn held_points() -> Option<(String, bool)> {
    POINTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|points| (points.stamp.clone(), points.binary_meta))
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

    let held = space.index.peek().stamp.clone();
    if info.stamp != held {
        // `None` is the held index still being current, the stamp having
        // moved back to it since `info` was asked.
        let index = if info.stamp.is_empty() {
            Some(SpaceIndex::default())
        } else {
            backend().space_index(&held).await?
        };
        if let Some(index) = index {
            if selected.peek().is_some_and(|id| !index.ids.contains(&id)) {
                selected.set(None);
            }
            space.index.set(Rc::new(index));
        }
    }

    if info.stamp != previous {
        crate::ui::forget_known();
        if !previous.is_empty() {
            generator.invalidate();
        }
    }
    if *space.info.peek() == info {
        return Ok(());
    }
    space.info.set(info.clone());

    // Only when something moved, which is what the stamp is for; most
    // refreshes are the same answer again. Two megabytes, so serialised and
    // written off the UI thread, and waited on so two never cross.
    let cached = CachedSpace { info, index: (**space.index.peek()).clone() };
    let _ = tokio::task::spawn_blocking(move || crate::cache::write(SPACE_CACHE, &cached)).await;
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
    Map { route: bool },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_is_handed_over_not_copied() {
        let mut points = Points::new("s".into(), vec![1, 2, 3], b"{}".to_vec());
        let data = serve(Some(&mut points), |points| &mut points.data, POINTS_CACHE, &[]);
        assert_eq!(data, vec![1, 2, 3]);
        assert!(points.data.is_none());
        assert_eq!(points.meta.as_deref(), Some(&b"{}"[..]));
    }

    #[test]
    fn no_points_serve_the_empty_body() {
        let meta = serve(None, |points| &mut points.meta, POINTS_META_CACHE, EMPTY_META.as_bytes());
        assert_eq!(meta, EMPTY_META.as_bytes());
    }
}
