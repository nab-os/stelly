//! Whether a newer release than this build is out, for the shell to say so.
//!
//! Asks GitHub directly rather than the server: the server is whatever version
//! its owner deployed, and says nothing about what is published.
//!
//! A release only counts once this platform's installer is attached to it. The
//! release job creates the release and then uploads a dozen files into it, and
//! announcing it in between would send people to a page with nothing on it
//! for them yet.

use serde::Deserialize;

const LATEST: &str = "https://api.github.com/repos/nab-os/Stelly/releases/latest";

/// How often a long-running window asks again. Unauthenticated, GitHub allows
/// 60 requests an hour per address, which this is nowhere near.
pub const RECHECK: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// This build's version, as the release tags spell it minus the `v`.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// A published release newer than this build, with somewhere to get it.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// Without the tag's `v`: `0.12.0`.
    pub version: String,
    /// The release page, notes and every download.
    pub url: String,
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    /// `uploaded` once the file can be downloaded.
    #[serde(default)]
    state: String,
}

/// The end of the file name that installs this build, as the release job
/// names them. Linux looks for the AppImage because it is the one file built
/// for every Ubuntu leg; the .debs and tarballs land alongside it. `None` on a
/// platform nothing is published for, where anyone running it built it
/// themselves and only the version is worth comparing.
fn installer() -> Option<&'static str> {
    if cfg!(target_os = "android") {
        Some("_arm64.apk")
    } else if cfg!(target_os = "windows") {
        Some("_x64-setup.exe")
    } else if cfg!(target_os = "linux") {
        Some(".AppImage")
    } else {
        None
    }
}

/// The newest release, if it is newer than this build and ready to download.
/// `Ok(None)` for "nothing to announce", including a repository with no
/// releases at all.
pub async fn check() -> anyhow::Result<Option<Release>> {
    let response = crate::backend::remote::http_client()
        .get(LATEST)
        // GitHub refuses a request without one.
        .header(
            reqwest::header::USER_AGENT,
            concat!("stelly/", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let latest: Latest = response.error_for_status()?.json().await?;
    Ok(announce(CURRENT, &latest, installer()))
}

fn announce(current: &str, latest: &Latest, installer: Option<&str>) -> Option<Release> {
    if latest.draft || latest.prerelease {
        return None;
    }
    let version = latest.tag_name.trim().trim_start_matches('v');
    if parse(version)? <= parse(current)? {
        return None;
    }
    if let Some(suffix) = installer {
        let ready = latest.assets.iter().any(|asset| {
            asset.name.starts_with("stelly_")
                && asset.name.ends_with(suffix)
                && (asset.state.is_empty() || asset.state == "uploaded")
        });
        if !ready {
            return None;
        }
    }
    Some(Release {
        version: version.to_string(),
        url: latest.html_url.clone(),
    })
}

/// `major.minor.patch`, with a missing patch read as 0: every tag up to v0.6
/// had none. Build metadata is dropped, so a CI build of `0.11.0+build42`
/// compares as 0.11.0. A pre-release suffix is refused outright rather than
/// ordered, since nothing here publishes one as the latest release.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split('+').next()?;
    if core.contains('-') {
        return None;
    }
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[&str]) -> Latest {
        Latest {
            tag_name: tag.into(),
            html_url: format!("https://github.com/nab-os/Stelly/releases/tag/{tag}"),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|name| Asset {
                    name: (*name).into(),
                    state: "uploaded".into(),
                })
                .collect(),
        }
    }

    const APK: Option<&str> = Some("_arm64.apk");

    #[test]
    fn versions_compare_numerically() {
        assert!(parse("0.10.0") > parse("0.9.3"));
        assert_eq!(parse("0.6"), Some((0, 6, 0)));
        assert_eq!(parse("0.11.0+build42.gabc123"), Some((0, 11, 0)));
        assert_eq!(parse("0.12.0-rc1"), None);
        assert_eq!(parse("1.2.3.4"), None);
        assert_eq!(parse("nightly"), None);
    }

    #[test]
    fn a_newer_release_with_the_installer_is_announced() {
        let latest = release("v0.12.0", &["stelly_0.12.0_arm64.apk"]);
        let found = announce("0.11.0", &latest, APK).unwrap();
        assert_eq!(found.version, "0.12.0");
        assert!(found.url.ends_with("/v0.12.0"));
    }

    #[test]
    fn the_same_or_an_older_release_is_not() {
        assert_eq!(
            announce(
                "0.11.0",
                &release("v0.11.0", &["stelly_0.11.0_arm64.apk"]),
                APK
            ),
            None
        );
        assert_eq!(
            announce(
                "0.11.0",
                &release("v0.10.2", &["stelly_0.10.2_arm64.apk"]),
                APK
            ),
            None
        );
    }

    #[test]
    fn a_release_still_uploading_is_not() {
        // The server's tarball is there, this platform's installer is not.
        let mut latest = release("v0.12.0", &["stelly-server_0.12.0_ubuntu-24.04.tar.gz"]);
        assert_eq!(announce("0.11.0", &latest, APK), None);

        latest.assets.push(Asset {
            name: "stelly_0.12.0_arm64.apk".into(),
            state: "starter".into(),
        });
        assert_eq!(announce("0.11.0", &latest, APK), None);
    }

    #[test]
    fn the_server_package_does_not_stand_in_for_the_app() {
        let latest = release("v0.12.0", &["stelly-server_0.12.0_amd64.ubuntu-24.04.deb"]);
        assert_eq!(announce("0.11.0", &latest, Some(".deb")), None);
    }

    #[test]
    fn drafts_and_prereleases_are_not() {
        let mut latest = release("v0.12.0", &["stelly_0.12.0_arm64.apk"]);
        latest.prerelease = true;
        assert_eq!(announce("0.11.0", &latest, APK), None);
    }

    #[test]
    fn with_no_installer_to_look_for_the_version_decides() {
        assert!(announce("0.11.0", &release("v0.12.0", &[]), None).is_some());
    }

    #[test]
    fn reads_what_github_sends() {
        let body = r#"{
            "tag_name": "v0.12.0",
            "html_url": "https://github.com/nab-os/Stelly/releases/tag/v0.12.0",
            "draft": false,
            "prerelease": false,
            "assets": [{ "name": "stelly_0.12.0_x64-setup.exe", "state": "uploaded", "size": 1 }],
            "body": "notes"
        }"#;
        let latest: Latest = serde_json::from_str(body).unwrap();
        assert!(announce("0.11.0", &latest, Some("_x64-setup.exe")).is_some());
    }
}
