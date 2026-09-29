//! Telling people a new release is out: a strip over the workspace until it is
//! closed, and a line in Settings that stays.

use super::prefs::{self, Prefs};
use crate::update::{self, Release};
use dioxus::prelude::*;

/// What the shell found out about releases. A context because the strip and
/// the settings row read the same answer, and asking GitHub twice for it would
/// be silly.
#[derive(Clone, Copy)]
pub struct Releases {
    /// Mirrors `Prefs::release_checks`; turning it on asks straight away.
    pub enabled: Signal<bool>,
    /// `None` until the first answer comes back.
    pub latest: Resource<Checked>,
    pub dismissed: Signal<Option<String>>,
}

impl Releases {
    /// Asks once now, then every `RECHECK` for as long as the window is open.
    /// Only the shell calls this.
    pub fn provide() -> Self {
        let stored = Prefs::load();
        let enabled = use_signal(|| stored.checks_releases());
        let dismissed = use_signal(|| stored.dismissed_release.clone());

        let mut tick = use_signal(|| 0u64);
        use_future(move || async move {
            loop {
                tokio::time::sleep(update::RECHECK).await;
                tick += 1;
            }
        });

        let latest = use_resource(move || async move {
            tick.read();
            if !enabled() {
                return Checked::Off;
            }
            match update::check().await {
                Ok(found) => Checked::Answered(found),
                Err(err) => {
                    eprintln!("could not check for a new release: {err:#}");
                    Checked::Failed
                }
            }
        });

        use_context_provider(|| Self {
            enabled,
            latest,
            dismissed,
        })
    }

    fn newer(&self) -> Option<Release> {
        match &*self.latest.read() {
            Some(Checked::Answered(found)) => found.clone(),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum Checked {
    Off,
    /// Offline, or GitHub said no. Logged; the next recheck tries again.
    Failed,
    /// `None` is nothing newer, or nothing downloadable for this platform yet.
    Answered(Option<Release>),
}

/// The strip at the top of the window. Closing it holds for this release
/// only, and across restarts.
#[component]
pub fn ReleaseNotice() -> Element {
    let releases = use_context::<Releases>();
    let mut dismissed = releases.dismissed;

    let Some(release) = releases.newer() else {
        return rsx! {};
    };
    if dismissed.read().as_deref() == Some(release.version.as_str()) {
        return rsx! {};
    }

    rsx! {
        div { class: "release-notice", role: "status",
            span {
                "Stelly {release.version} is out, you have {update::CURRENT}. "
                a { href: "{release.url}", "Get it" }
            }
            button {
                class: "chip",
                title: "hide until the next release",
                aria_label: "hide until the next release",
                onclick: move |_| {
                    let version = release.version.clone();
                    dismissed.set(Some(version.clone()));
                    prefs::update(|prefs| prefs.dismissed_release = Some(version));
                },
                "×"
            }
        }
    }
}

/// This build's version, whether a newer one is out, and the switch for
/// asking at all. Still says so after the strip has been closed.
#[component]
pub fn ReleaseRow() -> Element {
    let releases = use_context::<Releases>();
    let mut enabled = releases.enabled;
    let on = enabled();

    let status = match &*releases.latest.read() {
        Some(Checked::Off) => rsx! {},
        None => rsx! { span { class: "muted", "checking…" } },
        Some(Checked::Failed) => rsx! { span { class: "muted", "could not reach GitHub" } },
        Some(Checked::Answered(None)) => rsx! { span { class: "muted", "up to date" } },
        Some(Checked::Answered(Some(release))) => rsx! {
            span {
                "{release.version} is out. "
                a { href: "{release.url}", "Get it" }
            }
        },
    };

    rsx! {
        section { class: "panel setting",
            h2 { "Version" }
            div { class: "setting-fields",
                span { "Stelly {update::CURRENT}" }
                {status}
                span { class: "spacer" }
                button {
                    class: if on { "chip active" } else { "chip" },
                    title: "ask GitHub for new releases, at start and every few hours",
                    onclick: move |_| {
                        enabled.set(!on);
                        prefs::update(|prefs| prefs.release_checks = Some(!on));
                    },
                    if on { "checking for releases" } else { "not checking for releases" }
                }
            }
        }
    }
}
