//! Settings: one row per group, top to bottom, the server this device talks
//! to, the version it runs, how it plays, which devices may talk to that server, who is hidden,
//! and the pipeline that builds the space, with its output last.

use super::pipeline::{Devices, PipelineControls, PipelineLog};
use super::player::{quality_label, Player};
use super::prefs;
use super::release::ReleaseRow;
use super::Blocklist;
use crate::qobuz::{FORMAT_FLAC_CD, FORMAT_FLAC_HIRES, FORMAT_MP3_320};
use crate::ServerConfig;
use dioxus::prelude::*;

#[component]
pub fn SettingsScreen() -> Element {
    rsx! {
        div { class: "page settings",
            ServerRow {}
            ReleaseRow {}
            PlaybackRow {}
            Devices {}
            HiddenArtists {}
            PipelineControls {}
            PipelineLog {}
        }
    }
}

/// Where the server address and token can be changed after the first run.
///
/// Saves and asks for a restart rather than reconnecting in place: the
/// backend could be swapped, but the space the engine has mapped, the loaded
/// catalogue and every screen's state all came from the old server.
#[component]
fn ServerRow() -> Element {
    let stored = use_signal(ServerConfig::load);
    let mut address = use_signal(|| {
        stored
            .peek()
            .as_ref()
            .map(|c| c.base.clone())
            .unwrap_or_else(|| "http://".into())
    });
    let mut token = use_signal(|| {
        stored
            .peek()
            .as_ref()
            .map(|c| c.token.clone())
            .unwrap_or_default()
    });
    let mut status = use_signal(|| None::<String>);

    let save = move |_| {
        let base = address.peek().trim().trim_end_matches('/').to_string();
        let secret = token.peek().trim().to_string();
        if base.is_empty() || secret.is_empty() {
            status.set(Some("Both the address and the token are needed.".into()));
            return;
        }

        let config = ServerConfig { base, token: secret };
        match config.save() {
            Ok(()) => status.set(Some(
                "Saved. Restart 2kHz to connect to it, the running process keeps the \
                 server it started with."
                    .into(),
            )),
            Err(err) => status.set(Some(format!("could not save: {err:#}"))),
        }
    };

    let forget = move |_| match ServerConfig::clear() {
        Ok(()) => {
            address.set("http://".into());
            token.set(String::new());
            status.set(Some("Pairing forgotten. Restart 2kHz to pair with a server again.".into()));
        }
        Err(err) => status.set(Some(format!("could not clear the pairing: {err:#}"))),
    };

    rsx! {
        section { class: "panel setting",
            h2 { "Server" }
            div { class: "setting-fields",
                input {
                    class: "search",
                    placeholder: "http://nas.tailnet.ts.net:7700",
                    value: "{address}",
                    oninput: move |event| address.set(event.value()),
                }
                input {
                    class: "search",
                    r#type: "password",
                    placeholder: "device token",
                    value: "{token}",
                    oninput: move |event| token.set(event.value()),
                }
                button { class: "primary", onclick: save, "Save" }
                button { onclick: forget, "Forget" }
            }
            if let Some(message) = status.read().clone() {
                p { class: "muted", "{message}" }
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
                for format_id in [FORMAT_MP3_320, FORMAT_FLAC_CD, FORMAT_FLAC_HIRES] {
                    button {
                        key: "{format_id}",
                        class: if quality == format_id { "chip active" } else { "chip" },
                        onclick: move |_| {
                            player.quality.set(format_id);
                            prefs::update(|prefs| prefs.quality = Some(format_id));
                        },
                        "{quality_label(format_id)}"
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
            if hidden.is_empty() {
                p { class: "muted", "Nobody. Hide an artist from their page or any row's menu." }
            }
            if open() {
                ul { class: "list hidden-artists",
                    for entry in hidden {
                        li { key: "{entry.artist_id}", class: "row",
                            span { class: "title", "{entry.name}" }
                            if let Some(reason) = entry.reason.clone() {
                                span { class: "muted", "{reason}" }
                            }
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
