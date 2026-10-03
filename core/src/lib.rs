//! What the Stelly app and server share: the wire types and the session rules
//! both sides run. Behind `engine`, what only the server loads: the catalogue
//! and its schema, the vector space, and navigation over it.

pub mod api;
pub mod logbuffer;
pub mod qobuz;
pub mod session;

#[cfg(feature = "engine")]
pub mod db;
#[cfg(feature = "engine")]
pub mod engine;
#[cfg(feature = "engine")]
pub mod map;
#[cfg(feature = "engine")]
pub mod paths;
#[cfg(feature = "engine")]
pub mod schema;
#[cfg(feature = "engine")]
pub mod space;
