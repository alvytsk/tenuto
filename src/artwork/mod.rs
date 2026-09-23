//! Local artwork lookup, bounded decode and a single contained background
//! worker (design doc M5 §9, decision 24: "Artwork preparation is contained
//! too"). Resolving a candidate cover, reading it within fixed byte and
//! pixel limits, and decoding it are kept independent of the terminal:
//! nothing in this module draws anything or knows about Ratatui, so it can
//! be exercised from plain unit and integration tests.

pub mod decode;
pub mod default;
pub mod resolve;
pub mod svg;
pub mod worker;
