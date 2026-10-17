//! The catalogue's items as the app shows them, whichever provider they
//! come from: `stelly-provider`'s types, under the names Stelly has always
//! used for them. Remote, as against the space's own rows: a remote item
//! links to the space by track id or not at all.
//!
//! Only types live here. The provider, and whatever account is behind it,
//! is the server's; a device never talks to one.

pub use stelly_provider::{recent_releases, Capabilities, Credit, Quality, ReleaseKind, SearchResults, TrackIdentity};
pub use stelly_provider::{Album as RemoteAlbum, Artist as RemoteArtist, Playlist as RemotePlaylist, Track as RemoteTrack};
