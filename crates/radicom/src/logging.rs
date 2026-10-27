//! Logging facade (INF-03; docs/08-serialization-media.md §7).
//!
//! A thin `tracing` facade so the whole library logs through one dependency with
//! consistent, categorized targets. Subscribers filter by target
//! (e.g. `radicom::io=debug`). Also exposes dataset dump-to-log helpers that sit
//! on top of the INF-04 formatter.

use crate::dump::{dump_dataset, DumpOptions};
use crate::model::dataset::Dataset;

/// Categorized log targets (§7). Use these in `tracing` macros via `target:`.
pub mod targets {
    pub const NETWORK: &str = "radicom::network";
    pub const CODEC: &str = "radicom::codec";
    pub const IO: &str = "radicom::io";
    pub const ENCODING: &str = "radicom::encoding";
    pub const PRINT: &str = "radicom::print";
}

/// Log level for [`Dataset::write_to_log`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Dataset {
    /// Renders this dataset with the default [`DumpOptions`] (INF-03/04).
    pub fn to_dump_string(&self) -> String {
        dump_dataset(self, &DumpOptions::default())
    }

    /// Renders this dataset with explicit [`DumpOptions`] (INF-03/04).
    pub fn to_dump_string_with(&self, options: &DumpOptions) -> String {
        dump_dataset(self, options)
    }

    /// Logs the dataset dump at `level` on the `radicom::io` target (INF-03).
    ///
    /// One event carries the whole multi-line dump; subscribers that want it
    /// split can format the returned [`to_dump_string`](Dataset::to_dump_string)
    /// themselves.
    pub fn write_to_log(&self, level: LogLevel) {
        let text = self.to_dump_string();
        match level {
            LogLevel::Trace => tracing::trace!(target: targets::IO, "{text}"),
            LogLevel::Debug => tracing::debug!(target: targets::IO, "{text}"),
            LogLevel::Info => tracing::info!(target: targets::IO, "{text}"),
            LogLevel::Warn => tracing::warn!(target: targets::IO, "{text}"),
            LogLevel::Error => tracing::error!(target: targets::IO, "{text}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tag::Tag;
    use crate::model::vr::Vr;

    fn sample() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Doe^John");
        ds.insert_str(Tag::new(0x0008, 0x0060), Vr::Cs, "CT");
        ds
    }

    #[test]
    fn to_dump_string_renders_elements() {
        let text = sample().to_dump_string();
        assert!(text.contains("(0010,0010)"), "dump shows the tag: {text}");
        assert!(text.contains("Doe^John"), "dump shows the value: {text}");
    }

    #[test]
    fn to_dump_string_with_options_respects_truncation() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Averyveryverylongvalue");
        let options = DumpOptions { max_value_chars: 4, ..Default::default() };
        let text = ds.to_dump_string_with(&options);
        assert!(!text.contains("Averyveryverylongvalue"), "value must be elided: {text}");
    }

    #[test]
    fn write_to_log_does_not_panic() {
        // with no subscriber installed the event is a no-op; the call must be safe
        sample().write_to_log(LogLevel::Debug);
    }

    #[test]
    fn target_constants_are_stable() {
        assert_eq!(targets::NETWORK, "radicom::network");
        assert_eq!(targets::CODEC, "radicom::codec");
        assert_eq!(targets::IO, "radicom::io");
        assert_eq!(targets::ENCODING, "radicom::encoding");
        assert_eq!(targets::PRINT, "radicom::print");
    }
}
