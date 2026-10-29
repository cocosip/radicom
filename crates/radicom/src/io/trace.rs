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

/// A [`ParseObserver`] that logs each event at debug level (§7).
#[derive(Debug, Clone, Copy, Default)]
pub struct TraceObserver;

impl TraceObserver {
    pub fn new() -> Self {
        TraceObserver
    }
}

impl ParseObserver for TraceObserver {
    fn on_element(&mut self, ctx: &ParseContext, tag: &Tag, vr: Vr, _buffer: SharedBuffer) -> Result<(), Error> {
        tracing::debug!(
            target: targets::IO,
            position = ctx.position,
            depth = ctx.depth,
            group = tag.group(),
            element = tag.element(),
            vr = ?vr,
            "element"
        );
        Ok(())
    }

    fn on_begin_sequence(&mut self, ctx: &ParseContext, tag: &Tag, defined_length: Option<u32>) -> Result<(), Error> {
        tracing::debug!(
            target: targets::IO,
            position = ctx.position,
            depth = ctx.depth,
            group = tag.group(),
            element = tag.element(),
            defined_length = ?defined_length,
            "begin sequence"
        );
        Ok(())
    }

    fn on_begin_sequence_item(&mut self, ctx: &ParseContext, defined_length: Option<u32>) -> Result<(), Error> {
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
            group = tag.group(), element = tag.element(), vr = ?vr,
            offsets = offset_table.len(),
            "begin fragment sequence"
        );
        Ok(())
    }

    fn on_fragment(&mut self, ctx: &ParseContext, _buffer: SharedBuffer) -> Result<(), Error> {
        tracing::debug!(target: targets::IO, position = ctx.position, depth = ctx.depth, "fragment");
        Ok(())
    }

    fn on_end_fragment(&mut self, ctx: &ParseContext) -> Result<(), Error> {
        tracing::debug!(target: targets::IO, position = ctx.position, depth = ctx.depth, "end fragment sequence");
        Ok(())
    }
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

    #[test]
    fn trace_parses_without_error() {
        // exercises every observer callback path through a real parse; the
        // debug events are a no-op when no subscriber is installed
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        trace(&pn_element(), &syntax).expect("trace parse succeeds");
    }

    #[test]
    fn trace_emits_debug_events() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Once};

        #[derive(Clone)]
        struct Counter(Arc<AtomicUsize>);
        impl tracing::Subscriber for Counter {
            fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
            fn event(&self, _event: &tracing::Event<'_>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn enter(&self, _span: &tracing::span::Id) {}
            fn exit(&self, _span: &tracing::span::Id) {}
        }

        // The callsite interest cache is process-global, so a thread-scoped
        // subscriber races other parallel tests that touch tracing callsites
        // (they can pin ours to `never`). Install one counting subscriber as the
        // *global* default once, force one cache rebuild, then emit. The counter
        // is shared, so concurrent tracing elsewhere only inflates it, never
        // starves our assertion.
        static INSTALL: Once = Once::new();
        static COUNT: std::sync::OnceLock<Arc<AtomicUsize>> = std::sync::OnceLock::new();
        let count = COUNT.get_or_init(|| Arc::new(AtomicUsize::new(0))).clone();
        INSTALL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Counter(count.clone()));
            tracing_core::callsite::rebuild_interest_cache();
        });

        let before = count.load(Ordering::Relaxed);
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        trace(&pn_element(), &syntax).expect("trace parse succeeds");
        assert!(
            count.load(Ordering::Relaxed) > before,
            "the observer must emit at least one debug event"
        );
    }

    #[test]
    fn trace_visits_sequences_and_fragments() {
        use crate::model::dataset::Dataset;
        use crate::model::vr::Vr;

        let mut ds = Dataset::new();
        let mut item = Dataset::new();
        item.insert_str(Tag::PATIENT_NAME, Vr::Pn, "DOE^JOHN");
        ds.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE).push(item);
        ds.insert_fragments(Tag::FLOAT_PIXEL_DATA, Vr::Ob)
            .push_fragment(std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![1, 2, 3])));

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
