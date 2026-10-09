//! Playback logic with no terminal underneath it: resolving what to play -
//! a podcast episode's enclosure, or a `play` argument's source - routing a
//! decoded key command to the engine, coalescing an arrow-key burst into one
//! seek, and deciding what a transport key should do given the queue and
//! playback phase. Nothing here prints, reads a key, or draws a frame, so
//! any front end can call into it and decide for itself how to show the
//! result.

pub mod browse;
pub mod enrich;
pub mod feed_ops;
pub mod podcast;
pub mod profile;
pub mod runtime;
pub mod seek;
pub mod source;
pub mod transport;
pub mod view;
