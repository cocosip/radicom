//! Parser trace observer (INF-03; docs/01-architecture.md §12).
//!
//! [`TraceObserver`] implements [`ParseObserver`] and logs every parse event at
//! debug level on the `radicom::io` target, so a subscriber like
//! `tracing_subscriber` with `radicom::io=debug` can watch a parse step by step.

use crate::error::Error;
use crate::io::byte_source::ByteSourceReader;
use crate::io::reader::{DicomReader, ParseContext, ParseObserver};
use crate::logging::targets;
use crate::model::buffer::SharedBuffer;
use crate::model::tag::Tag;
use crate::model::transfer_syntax::TransferSyntax;
use crate::model::vr::Vr;

/// A [`ParseObserver`] that logs each event at debug level.
#[derive(Debug, Clone, Copy, Default)]
pub struct TraceObserver;

impl TraceObserver {
    pub fn new() -> Self {
        TraceObserver
    }
}

impl ParseObserver for TraceObserver {
    fn on_element(
        &mut self,
        ctx: &ParseContext,
        tag: &Tag,
        vr: Vr,
        buffer: SharedBuffer,
    ) -> Result<(), Error> {
        // fo-dicom's DicomParserLogger prints the tag, VR code, dictionary name
        // and value size per element (Log/DicomParserLogger.cs:29-38); without
        // the size and name a trace says nothing about what was parsed (L20).
        tracing::debug!(
            target: targets::IO,
            position = ctx.position,
            depth = ctx.depth,
            group = tag.group(),
            element = tag.element(),
            name = dictionary_name(tag),
            vr = ?vr,
            size = buffer.size(),
            "element"
        );
        Ok(())
    }

    fn on_begin_sequence(
        &mut self,
        ctx: &ParseContext,
        tag: &Tag,
        defined_length: Option<u32>,
    ) -> Result<(), Error> {
        tracing::debug!(
            target: targets::IO,
            position = ctx.position,
            depth = ctx.depth,
            group = tag.group(),
            element = tag.element(),
            name = dictionary_name(tag),
            defined_length = ?defined_length,
            "begin sequence"
        );
        Ok(())
    }

    fn on_begin_sequence_item(
        &mut self,
        ctx: &ParseContext,
        defined_length: Option<u32>,
    ) -> Result<(), Error> {
        tracing::debug!(
            target: targets::IO,
            position = ctx.position, depth = ctx.depth, defined_length = ?defined_length,
            "begin sequence item"
        );
        Ok(())
    }

    fn on_end_sequence_item(&mut self, ctx: &ParseContext) -> Result<(), Error> {
        tracing::debug!(target: targets::IO, position = ctx.position, depth = ctx.depth, "end sequence item");
        Ok(())
    }

    fn on_end_sequence(&mut self, ctx: &ParseContext) -> Result<(), Error> {
        tracing::debug!(target: targets::IO, position = ctx.position, depth = ctx.depth, "end sequence");
        Ok(())
    }

    fn on_begin_fragment(
        &mut self,
        ctx: &ParseContext,
        tag: &Tag,
        vr: Vr,
        offset_table: Vec<u32>,
    ) -> Result<(), Error> {
        tracing::debug!(
            target: targets::IO,
            position = ctx.position, depth = ctx.depth,
            group = tag.group(), element = tag.element(),
            name = dictionary_name(tag),
            vr = ?vr,
            offsets = offset_table.len(),
            "begin fragment sequence"
        );
        Ok(())
    }

    fn on_fragment(&mut self, ctx: &ParseContext, buffer: SharedBuffer) -> Result<(), Error> {
        tracing::debug!(
            target: targets::IO,
            position = ctx.position, depth = ctx.depth, size = buffer.size(),
            "fragment"
        );
        Ok(())
    }

    fn on_end_fragment(&mut self, ctx: &ParseContext) -> Result<(), Error> {
        tracing::debug!(target: targets::IO, position = ctx.position, depth = ctx.depth, "end fragment sequence");
        Ok(())
    }
}

/// Dictionary name for a tag, or `Private` for an unknown/private one.
fn dictionary_name(tag: &Tag) -> &'static str {
    crate::model::dictionary::dictionary().entry(tag).name
}

/// Parses `bytes` with a [`TraceObserver`], logging every event at debug level.
///
/// The dataset itself is discarded — use [`crate::io::dataset_builder::parse_dataset_bytes`]
/// when you need the result. Intended for tracing/debugging parse behavior.
pub fn trace(bytes: &[u8], syntax: &TransferSyntax) -> Result<(), Error> {
    let mut source = ByteSourceReader::new(std::io::Cursor::new(bytes.to_vec()), syntax.endian());
    let mut observer = TraceObserver::new();
    DicomReader::default().read(&mut source, syntax, &mut observer, None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transfer_syntax::syntaxes;
    use std::sync::{Arc, Mutex, Once, OnceLock};

    /// A minimal Explicit VR Little Endian element: (0010,0010) PN "DOE^JOHN"
    /// (PN is a short VR: 2-byte length, no reserved field).
    fn pn_element() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"PN");
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(b"DOE^JOHN");
        out
    }

    type Events = Arc<Mutex<Vec<Vec<(String, String)>>>>;

    /// Records every event's fields. One global subscriber serves all tests in
    /// this module: the tracing interest cache is process-global, so a second
    /// `set_global_default` would silently lose events (and parallel tests that
    /// touch tracing would race a thread-local one).
    #[derive(Clone)]
    struct Capture(Events);

    struct FieldVisitor<'a>(&'a mut Vec<(String, String)>);

    impl tracing::field::Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .push((field.name().to_string(), format!("{value:?}")));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = Vec::new();
            event.record(&mut FieldVisitor(&mut fields));
            self.0.lock().expect("capture lock").push(fields);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn capture() -> Events {
        static INSTALL: Once = Once::new();
        static EVENTS: OnceLock<Events> = OnceLock::new();
        let events = EVENTS.get_or_init(Default::default).clone();
        INSTALL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Capture(events.clone()));
            tracing_core::callsite::rebuild_interest_cache();
        });
        events
    }

    fn field<'a>(event: &'a [(String, String)], name: &str) -> Option<&'a str> {
        event
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn trace_parses_without_error() {
        // exercises every observer callback path through a real parse; the
        // debug events are a no-op when no subscriber is installed
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        trace(&pn_element(), &syntax).expect("trace parse succeeds");
    }

    #[test]
    fn trace_emits_debug_events() {
        let events = capture();
        let before = events.lock().expect("capture lock").len();
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        trace(&pn_element(), &syntax).expect("trace parse succeeds");
        assert!(
            events.lock().expect("capture lock").len() > before,
            "the observer must emit at least one debug event"
        );
    }

    #[test]
    fn trace_reports_element_size_and_dictionary_name() {
        // L20: fo-dicom's parser logger prints "... {Tag} {Vr} {Name} [{Size}]"
        let events = capture();
        let before = events.lock().expect("capture lock").len();
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        trace(&pn_element(), &syntax).expect("trace parse succeeds");

        let captured = events.lock().expect("capture lock");
        let element = captured[before..]
            .iter()
            .find(|event| field(event, "size") == Some("8"))
            .expect("an element event carries the value size");
        assert_eq!(
            field(element, "name"),
            Some(
                crate::model::dictionary::dictionary()
                    .entry(&Tag::PATIENT_NAME)
                    .name
            ),
            "the dictionary name is logged"
        );
        assert_eq!(field(element, "vr"), Some("Pn"));
    }

    #[test]
    fn trace_visits_sequences_and_fragments() {
        use crate::model::dataset::Dataset;
        use crate::model::vr::Vr;

        let mut ds = Dataset::new();
        let mut item = Dataset::new();
        item.insert_str(Tag::PATIENT_NAME, Vr::Pn, "DOE^JOHN");
        ds.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE)
            .push(item);
        ds.insert_fragments(Tag::FLOAT_PIXEL_DATA, Vr::Ob)
            .push_fragment(std::sync::Arc::new(
                crate::model::buffer::MemoryBuffer::from(vec![1, 2, 3]),
            ));

        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let mut bytes = Vec::new();
        crate::io::writer::DicomWriter::new(crate::io::writer::WriteOptions::default())
            .write_dataset(&ds, &syntax, &mut bytes)
            .expect("encode");
        trace(&bytes, &syntax).expect("trace parses sequences and fragments");
    }

    #[test]
    fn trace_reports_malformed_input() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        // tag promises a 9999-byte value but no bytes follow
        let mut bad = Vec::new();
        bad.extend_from_slice(&0x0010u16.to_le_bytes());
        bad.extend_from_slice(&0x0010u16.to_le_bytes());
        bad.extend_from_slice(b"PN");
        bad.extend_from_slice(&9999u16.to_le_bytes());
        assert!(trace(&bad, &syntax).is_err(), "truncated value is an error");
    }
}
