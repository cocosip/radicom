//! Printing data models and layout rendering (PRINT-01/02;
//! docs/08-serialization-media.md §5).
//!
//! This module carries the film-layout maths (`ImageDisplayFormat`, per-cell
//! layout rectangles, film-size conversion) and, when the `render` feature is
//! enabled, `FilmBox::render_to_image` (PRINT-02) built on the REND-07 backend.

pub mod layout;
pub mod models;
#[cfg(feature = "render")]
mod render;

pub use layout::{FilmSize, ImageDisplayFormat, LayoutCell};
pub use models::{FilmBox, FilmSession, ImageBox};
#[cfg(feature = "render")]
pub use render::{EmptyDensity, RenderScale};
