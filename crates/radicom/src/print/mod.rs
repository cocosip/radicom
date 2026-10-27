//! Printing data models and layout rendering (PRINT-01/02;
//! docs/08-serialization-media.md §5).
//!
//! This module carries the film-layout maths: `ImageDisplayFormat` parsing,
//! per-cell layout rectangles and film-size conversion. `render_to_image`
//! (PRINT-02) needs the REND-07 render backend, which is not built yet, so it
//! is deliberately absent rather than stubbed.

pub mod layout;
pub mod models;

pub use layout::{FilmSize, ImageDisplayFormat, LayoutCell};
pub use models::{FilmBox, FilmSession, ImageBox};
