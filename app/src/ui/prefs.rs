//! What this device remembers between runs, besides the pairing: the stream
//! quality, grid or list for each kind of page, whether mixed pages split
//! by kind, and what to say about new releases.
//!
//! Its own file next to `server.json` rather than inside it: forgetting the
//! pairing should not also forget how the lists were laid out.

use super::library::{Grouping, TracksView, ViewKind};
use crate::client_data_dir;
use crate::qobuz::{FORMAT_FLAC_CD, FORMAT_FLAC_HIRES, FORMAT_MP3_320};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Every field defaults, so a file from an older build, or a hand-edited one
/// missing a key, still loads the rest.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Prefs {
    pub quality: Option<u32>,
    pub layouts: HashMap<ViewKind, TracksView>,
    /// Mixed views in one list, or split by kind.
    pub grouping: Grouping,
    /// Off stops asking GitHub about releases at all. `None` is on.
    pub release_checks: Option<bool>,
    /// The release whose notice was closed. Only that one: the next release
    /// is announced again.
    pub dismissed_release: Option<String>,
}

impl Prefs {
    fn path() -> PathBuf {
        client_data_dir().join("prefs.json")
    }

    /// Missing or unreadable is the same as never having changed anything.
    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// The stored quality, if it is one this build can still ask for.
    pub fn quality(&self) -> u32 {
        self.quality
            .filter(|q| [FORMAT_MP3_320, FORMAT_FLAC_CD, FORMAT_FLAC_HIRES].contains(q))
            .unwrap_or(FORMAT_MP3_320)
    }

    pub fn checks_releases(&self) -> bool {
        self.release_checks.unwrap_or(true)
    }

    fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// Change one thing and write the file back. Read fresh each time, so two
/// settings changed from different screens cannot overwrite each other. A
/// failed write only costs the setting on the next start, so it is logged
/// rather than shown.
pub fn update(change: impl FnOnce(&mut Prefs)) {
    let mut prefs = Prefs::load();
    change(&mut prefs);
    if let Err(err) = prefs.save() {
        eprintln!("could not save preferences: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let mut prefs = Prefs {
            quality: Some(FORMAT_FLAC_CD),
            ..Prefs::default()
        };
        prefs.layouts.insert(ViewKind::Mixed, TracksView::List);
        prefs.layouts.insert(ViewKind::Artist, TracksView::Grid);
        prefs.grouping = Grouping::Split;

        let text = serde_json::to_string(&prefs).unwrap();
        assert_eq!(serde_json::from_str::<Prefs>(&text).unwrap(), prefs);
    }

    #[test]
    fn partial_file_keeps_defaults() {
        let prefs: Prefs = serde_json::from_str(r#"{"layouts":{"albums":"list"}}"#).unwrap();
        assert_eq!(prefs.quality(), FORMAT_MP3_320);
        assert_eq!(prefs.layouts.get(&ViewKind::Albums), Some(&TracksView::List));
        assert_eq!(prefs.grouping, Grouping::Mixed);
    }

    #[test]
    fn unknown_quality_falls_back() {
        let prefs = Prefs {
            quality: Some(99),
            ..Prefs::default()
        };
        assert_eq!(prefs.quality(), FORMAT_MP3_320);
    }
}
