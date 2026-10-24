//! Settings: one row per group, top to bottom, the server this device talks
//! to, the version it runs, how it plays, the tour again, who uses it and from which
//! devices, who is hidden, and the pipeline that builds the space, with its output last.

use super::family::FamilyRow;
use super::pipeline::{MixQueuePanel, PipelineControls, PipelineLog};
use super::player::{quality_label, Player};
use super::prefs;
use super::release::ReleaseRow;
use super::tour::TourRow;
use super::{icons, Blocklist, Me, Space};
use crate::app::{normalise_address, pinned_by_env, Pairing};
use crate::backend::{backend, is_bad_address, is_unauthorised, is_unreachable, Remote};
use crate::catalogue::Quality;
use crate::ServerConfig;
use dioxus::prelude::*;

#[component]
pub fn SettingsScreen() -> Element {
    rsx! {
        div { class: "page settings",
            ServerRow {}
            ReleaseRow {}
            PlaybackRow {}
            TourRow {}
            FamilyRow {}
            HiddenArtists {}
            PipelineControls {}
            MixQueuePanel {}
            PipelineLog {}
        }
    }
}

/// The server this device talks to: whether it answers, as whom, and a way
/// to move to another one or off it altogether.
///
/// Moving is tried before anything is kept: the new address and token are
/// asked one cheap question first, and only an answer saves them and
/// reconnects, in place. A typo costs nothing, the current pairing stays.
#[component]
fn ServerRow() -> Element {
    let pairing = use_context::<Pairing>();
    let offline = use_context::<Space>().offline;
    let me = use_context::<Me>().0;
    let mut editing = use_signal(|| false);
    let mut confirming = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);

    let address = backend().base().to_string();
    let shown = address
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string();
    let reachable = offline.read().is_none();

    rsx! {
        section { class: "panel setting server",
            h2 { "Server" }

            div { class: "server-card",
                span { class: "server-icon", {icons::server()} }
                div { class: "server-text",
                    span { class: "server-address", title: "{address}", "{shown}" }
                    if let Some(device) = me.read().as_ref() {
                        span { class: "muted",
                            "This device is “{device.name}”"
                            if !device.user.is_empty() { ", {device.user}’s" }
                        }
                    }
                }
                span { class: "spacer" }
                if let Some(device) = me.read().as_ref() {
                    span { class: "access {device.scope.as_str()}", "{device.scope.as_str()}" }
                }
                span {
                    class: if reachable { "server-state ok" } else { "server-state down" },
                    title: offline.read().clone().unwrap_or_default(),
                    span { class: "server-dot" }
                    if reachable { "connected" } else { "unreachable" }
                }
            }

            if pinned_by_env() {
                p { class: "muted",
                    "Set by STELLY_SERVER in the environment, which wins again at the next start. \
                     A change made here lasts until then."
                }
            }

            if editing() {
                ServerForm { current: address.clone(), editing }
            } else if confirming() {
                div { class: "server-confirm",
                    p { "This device forgets the server and its token, and goes back to the setup screen. \
                         Pairing again needs a token." }
                    div { class: "pair-actions",
                        button { onclick: move |_| confirming.set(false), "Cancel" }
                        button {
                            class: "danger",
                            onclick: move |_| {
                                if let Err(err) = pairing.forget() {
                                    failure.set(Some(format!("could not forget the pairing: {err:#}")));
                                    confirming.set(false);
                                }
                            },
                            "Disconnect"
                        }
                    }
                }
            } else {
                div { class: "setting-fields",
                    button {
                        onclick: move |_| {
                            failure.set(None);
                            editing.set(true);
                        },
                        "Change server or token…"
                    }
                    span { class: "spacer" }
                    button {
                        class: "chip danger",
                        onclick: move |_| {
                            failure.set(None);
                            confirming.set(true);
                        },
                        "Disconnect"
                    }
                }
            }

            if let Some(message) = failure.read().clone() {
                p { class: "setup-error", "{message}" }
            }
        }
    }
}

/// The address and token, as on the setup screen. The token may be left
/// empty to keep the current one, the usual case being a server that moved.
#[component]
fn ServerForm(current: String, editing: Signal<bool>) -> Element {
    let pairing = use_context::<Pairing>();
    let mut address = use_signal(|| current.clone());
    let mut token = use_signal(String::new);
    let mut reveal = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);

    let mut connect = move || {
        if *busy.peek() {
            return;
        }
        let base = normalise_address(&address.peek());
        if base.is_empty() {
            failure.set(Some("The server's address is needed.".into()));
            return;
        }
        let typed = token.peek().trim().to_string();
        let changed_token = !typed.is_empty();
        let secret = if changed_token { typed } else { backend().token().to_string() };
        if base == backend().base() && !changed_token {
            editing.set(false);
            return;
        }
        address.set(base.clone());
        busy.set(true);
        failure.set(None);

        spawn(async move {
            // Asked through a backend of its own, so the app keeps talking to
            // the current server until this one has answered.
            let probe = Remote::new(base.clone(), secret.clone());
            match probe.space_info().await {
                Ok(_) => {
                    if let Err(err) = pairing.switch(ServerConfig { base, token: secret }) {
                        failure.set(Some(format!("could not save the pairing: {err:#}")));
                        busy.set(false);
                    }
                    // Otherwise the whole shell, this with it, starts again.
                }
                Err(err) => {
                    failure.set(Some(if is_unauthorised(&err) {
                        if changed_token {
                            "That server refused this token.".to_string()
                        } else {
                            "That server refused this device's current token. Paste one it issued.".to_string()
                        }
                    } else if is_bad_address(&err) {
                        format!("“{base}” is not an address a request can go to. It looks like nas.lan:7700 or https://music.example.org.")
                    } else if is_unreachable(&err) {
                        format!("Nothing answered at {base}. Check the address, and that stelly-server is running there.")
                    } else {
                        format!("{err:#}")
                    }));
                    busy.set(false);
                }
            }
        });
    };
    let on_key = move |event: KeyboardEvent| {
        if event.key() == Key::Enter {
            connect();
        } else if event.key() == Key::Escape {
            editing.set(false);
        }
    };

    rsx! {
        div { class: "server-form",
            div { class: "setup-field",
                label { r#for: "server-address", "Server address" }
                input {
                    id: "server-address",
                    class: "search",
                    placeholder: "nas.tailnet.ts.net:7700",
                    value: "{address}",
                    autofocus: true,
                    oninput: move |event| address.set(event.value()),
                    onkeydown: on_key,
                }
            }
            div { class: "setup-field",
                label { r#for: "server-token", "Device token" }
                div { class: "secret",
                    input {
                        id: "server-token",
                        class: "search",
                        r#type: if reveal() { "text" } else { "password" },
                        placeholder: "leave empty to keep the current one",
                        value: "{token}",
                        oninput: move |event| token.set(event.value()),
                        onkeydown: on_key,
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
            div { class: "pair-actions",
                span { class: "muted pair-hint",
                    "Tried before it is kept: if it does not answer, nothing changes."
                }
                button { disabled: busy(), onclick: move |_| editing.set(false), "Cancel" }
                button {
                    class: "primary",
                    disabled: busy(),
                    onclick: move |_| connect(),
                    if busy() { "Connecting…" } else { "Connect" }
                }
            }
        }
    }
}

/// Stream quality. Asked for per track, so a change applies from the next
/// one. Saved, so it is also what the next session starts with.
#[component]
fn PlaybackRow() -> Element {
    let mut player = use_context::<Player>();
    let quality = *player.quality.read();

    rsx! {
        section { class: "panel setting",
            h2 { "Playback" }
            div { class: "setting-fields",
                for choice in Quality::ALL {
                    button {
                        key: "{choice.as_str()}",
                        class: if quality == choice { "chip active" } else { "chip" },
                        onclick: move |_| {
                            player.quality.set(choice);
                            prefs::update(|prefs| prefs.quality = Some(choice));
                        },
                        "{quality_label(choice)}"
                    }
                }
                span { class: "muted", "applies from the next track" }
            }
        }
    }
}

/// The block list, with a way out of it. Collapsed by default: it grows
/// without bound, 115 entries on a well-used corpus.
#[component]
fn HiddenArtists() -> Element {
    let blocklist = use_context::<Blocklist>();
    let hidden = blocklist.artists.read().clone();
    let mut open = use_signal(|| false);

    rsx! {
        section { class: "panel setting",
            h2 {
                "Hidden artists"
                span { class: "muted", "{hidden.len()}" }
                span { class: "spacer" }
                if !hidden.is_empty() {
                    button {
                        class: "chip",
                        onclick: move |_| {
                            let next = !open();
                            open.set(next);
                        },
                        if open() { "hide list" } else { "show" }
                    }
                }
            }
            if hidden.is_empty() && *blocklist.editable.read() {
                p { class: "muted", "Nobody. Hide an artist from their page or any row's menu." }
            } else if hidden.is_empty() {
                p { class: "muted", "Nobody." }
            } else if !*blocklist.editable.read() {
                p { class: "muted", "Hidden from the whole family's space. Only an admin device can change this." }
            }
            if open() {
                ul { class: "list hidden-artists",
                    for entry in hidden {
                        li { key: "{entry.artist_id}", class: "row",
                            span { class: "title", "{entry.name}" }
                            if let Some(reason) = entry.reason.clone() {
                                span { class: "muted", "{reason}" }
                            }
                            if *blocklist.editable.read() {
                                button {
                                    class: "chip",
                                    onclick: move |_| blocklist.unblock.call(entry.artist_id),
                                    "unhide"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
