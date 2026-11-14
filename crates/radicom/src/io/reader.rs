//! Recursive-descent DICOM parser (RD-01..05; docs/architecture.md §7).
//!
//! The parser emits observer events; building, DICOMDIR stitching, dumps
//! and JSON all reuse it. Tolerance rows T1–T10 map to the behaviors in
//! docs/architecture.md §7.
//!
//! Recursion happens only in `Parser::parse_level`, whose stack frame
//! holds nothing but a `Step` value; the per-element work lives in
//! `Parser::step`, which returns before any nested level starts. That
//! keeps the `max_depth = 256` guard meaningful on the default 2 MiB thread
//! stack (docs/architecture.md §7).

use crate::error::{Error, ReaderError};
use crate::io::byte_source::ByteSourceReader;
use crate::model::buffer::{EmptyBuffer, EndianBuffer, EvenLengthBuffer, SharedBuffer};
use crate::model::endian::Endian;
use crate::model::tag::Tag;
use crate::model::transfer_syntax::TransferSyntax;
use crate::model::vr::Vr;
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::sync::Arc;

/// Parse callbacks (RD-02). `ctx.position` is the byte offset of the event
/// trigger; `ctx.depth` the current SQ nesting (0 = dataset level).
///
/// The basic offset table of a fragment sequence is handed to
/// [`ParseObserver::on_begin_fragment`] instead of arriving as a fragment,
/// so observers never special-case fragment index 0.
pub trait ParseObserver {
    fn on_element(
        &mut self,
        ctx: &ParseContext,
        tag: &Tag,
        vr: Vr,
        buffer: SharedBuffer,
    ) -> Result<(), Error>;
    fn on_begin_sequence(
        &mut self,
        ctx: &ParseContext,
        tag: &Tag,
        defined_length: Option<u32>,
    ) -> Result<(), Error>;
    fn on_begin_sequence_item(
        &mut self,
        ctx: &ParseContext,
        defined_length: Option<u32>,
    ) -> Result<(), Error>;
    fn on_end_sequence_item(&mut self, ctx: &ParseContext) -> Result<(), Error>;
    fn on_end_sequence(&mut self, ctx: &ParseContext) -> Result<(), Error>;
    fn on_begin_fragment(
        &mut self,
        ctx: &ParseContext,
        tag: &Tag,
        vr: Vr,
        offset_table: Vec<u32>,
    ) -> Result<(), Error>;
    fn on_fragment(&mut self, ctx: &ParseContext, buffer: SharedBuffer) -> Result<(), Error>;
    /// A fragment was dropped by [`LargeValuePolicy::Skip`] (M1). fo-dicom hands
    /// the observer no buffer and withholds the *whole* fragment sequence, so a
    /// dataset builder must not keep a partial one; the default ignores the
    /// event for observers that only count bytes.
    fn on_fragment_skipped(&mut self, _ctx: &ParseContext) -> Result<(), Error> {
        Ok(())
    }
    fn on_end_fragment(&mut self, ctx: &ParseContext) -> Result<(), Error>;
}

#[derive(Debug, Clone, Copy)]
pub struct ParseContext {
    pub position: u64,
    pub depth: u32,
}

/// Stop-predicate input (FILE-01): the tag about to be parsed.
#[derive(Debug, Clone, Copy)]
pub struct ParseState {
    pub previous_tag: Option<(u16, u16)>,
    pub tag: (u16, u16),
    pub sequence_depth: u32,
}

/// Outcome of a parse pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    Success,
    /// A stop predicate ended the pass (FILE-01).
    Stopped,
    /// The source ended inside a declared value: the dataset holds everything
    /// parsed so far and is marked partial (fo-dicom `DicomReaderResult.Suspended`).
    Suspended,
}

/// How values at or above `large_object_size` are handled (FILE-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LargeValuePolicy {
    /// Read into memory. The only option when no window source is attached.
    #[default]
    Materialize,
    /// Skip the bytes and report an empty placeholder.
    Skip,
    /// Leave the value as a lazy file/stream window when a window source exists,
    /// otherwise fall back to materializing.
    OnDemand,
}

/// Parser options (RD-01).
#[derive(Debug, Clone)]
pub struct DicomReader {
    pub large_object_size: u64,
    pub max_depth: u32,
    pub large_policy: LargeValuePolicy,
    /// Treat a source that ends inside a declared value as a partial success
    /// (`ReadOutcome::Suspended`) instead of an error. Whole-file parsing
    /// enables this so truncated files keep their readable prefix.
    pub partial_on_eof: bool,
}

impl Default for DicomReader {
    fn default() -> Self {
        DicomReader {
            large_object_size: 64 * 1024,
            max_depth: 256,
            large_policy: LargeValuePolicy::Materialize,
            partial_on_eof: false,
        }
    }
}

/// Private creators indexed by group then block number (element & 0xFF).
///
/// A flat 256-slot table per group avoids hashing a `(u16, u8)` key for every
/// private element, and the creator is stored as a shared `Arc` so resolving a
/// private tag clones a pointer instead of allocating a fresh creator.
type PrivateCreators =
    HashMap<u16, Box<[Option<Arc<crate::model::private_creator::PrivateCreator>>; 256]>>;

/// What ends one nesting level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Top-level dataset: ends at EOF.
    Top,
    /// Defined-length value: ends once `limit` bytes are consumed.
    Limited { limit: u64 },
    /// Undefined-length item: ends at the item delimiter `FFFE,E00D`.
    ItemDelimiter,
    /// Undefined-length SQ: ends at the sequence delimiter `FFFE,E0DD`.
    SequenceDelimiter,
}

/// How a level ended, propagated to the enclosing level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelEnd {
    Limit,
    ItemEnd,
    SequenceEnd,
    Eof,
}

/// One iteration of [`Parser::parse_level`]: either a nested level to
/// descend into or a leaf to report. Kept separate so the recursive frame
/// stays small.
enum Step {
    Continue,
    End(LevelEnd),
    Stopped,
    Element {
        raw: (u16, u16),
        tag: Tag,
        vr: Vr,
        pos: u64,
        buffer: SharedBuffer,
    },
    /// A sequence (or a T4 private undefined-length value) to descend into.
    Sequence {
        raw: (u16, u16),
        tag: Tag,
        pos: u64,
        defined: Option<u32>,
        /// Re-probe the subtree's VR explicitness (private sequences only).
        private_probe: bool,
        /// CP-246: a UN-encoded, non-private SQ is read as Implicit VR LE
        /// regardless of the dataset byte order.
        cp246: bool,
    },
    /// A sequence item to descend into.
    Item {
        raw: (u16, u16),
        pos: u64,
        defined: Option<u32>,
    },
    /// T5: an item tag at dataset level with no preceding SQ header.
    Orphan {
        raw: (u16, u16),
        pos: u64,
        defined: Option<u32>,
    },
    /// A fragment sequence to scan into the observer.
    Fragments {
        raw: (u16, u16),
        tag: Tag,
        vr: Vr,
        pos: u64,
    },
}

struct Parser<'a, R: Read + Seek> {
    source: &'a mut ByteSourceReader<R>,
    options: &'a DicomReader,
    depth: u32,
    /// Current VR explicitness: the transfer syntax default, overridden for
    /// one subtree by the T4 private-SQ probe.
    explicit_vr: bool,
    big_endian: bool,
    stopped: bool,
    /// Set when `partial_on_eof` absorbed a truncated value.
    suspended: bool,
    private_creators: &'a mut PrivateCreators,
}

impl<'a, R: Read + Seek> Parser<'a, R> {
    fn ctx(&self, pos: u64) -> ParseContext {
        ParseContext {
            position: pos,
            depth: self.depth,
        }
    }

    fn position(&mut self) -> Result<u64, Error> {
        self.source.position().map_err(|e| io_err(e, 0))
    }

    /// Parse context at the current source position.
    fn here(&mut self) -> Result<ParseContext, Error> {
        let pos = self.position()?;
        Ok(self.ctx(pos))
    }

    fn read_tag(&mut self, at: u64) -> Result<(u16, u16), Error> {
        let group = self.source.read_u16().map_err(|e| io_err_need(e, at, 2))?;
        let element = self
            .source
            .read_u16()
            .map_err(|e| io_err_need(e, at + 2, 2))?;
        Ok((group, element))
    }

    /// Dictionary VR resolution for implicit VRs and fallbacks. The tag must
    /// carry its private creator: without it a private element has no
    /// dictionary row and stays UN (RD-05).
    fn dictionary_vr(&self, tag: &Tag) -> Vr {
        if tag.element() == 0x0000 {
            return Vr::Ul; // group length is always UL (T9 target)
        }
        let entry = crate::model::dictionary::dictionary().entry(tag);
        if entry.keyword == "Unknown" {
            return Vr::Un;
        }
        match entry.vrs.as_ref() {
            [] => Vr::Un,
            [only] => *only,
            many => {
                if many.contains(&Vr::Ob) && many.contains(&Vr::Ow) {
                    Vr::Ow
                } else {
                    many[0]
                }
            }
        }
    }

    /// T9: a group length element written with UN behaves as UL.
    fn fix_group_length_vr(&self, element: u16, vr: Vr) -> Vr {
        if element == 0x0000 && vr == Vr::Un {
            Vr::Ul
        } else {
            vr
        }
    }

    /// T4: probes whether the SQ subtree at the current position is encoded
    /// explicitly. Zero-length items are skipped first (#223/#487), then the
    /// first element inside carries a 2-letter VR at offset 4 of the item
    /// body under explicit encoding, while an implicit item has the first
    /// half of a 32-bit length there.
    fn probe_inner_explicit(&mut self, count: u32) -> Result<bool, Error> {
        let pos = self.position()?;
        let mut remaining = count as i64;
        // Skip empty items until one has a body (fo-dicom IsPrivateSequenceBad).
        loop {
            if self.source.require(8).is_err() {
                self.source.goto(pos).map_err(|e| io_err(e, pos))?;
                return Ok(self.explicit_vr);
            }
            let g = self.source.read_u16().map_err(|e| io_err_need(e, pos, 2))?;
            let e = self
                .source
                .read_u16()
                .map_err(|e| io_err_need(e, pos + 2, 2))?;
            if (g, e) != (0xFFFE, 0xE000) {
                self.source.goto(pos).map_err(|e| io_err(e, pos))?;
                return Ok(self.explicit_vr);
            }
            let len = self
                .source
                .read_u32()
                .map_err(|e| io_err_need(e, pos + 4, 4))?;
            if len > 0 {
                break;
            }
            remaining -= 8;
            if remaining <= 0 {
                self.source.goto(pos).map_err(|e| io_err(e, pos))?;
                return Ok(self.explicit_vr);
            }
        }
        // At the first element of the first non-empty item: tag(4) + VR(2).
        let mut probe = [0u8; 6];
        let ok = self.source.read_exact(&mut probe).is_ok();
        self.source.goto(pos).map_err(|e| io_err(e, pos))?;
        if !ok {
            return Ok(self.explicit_vr);
        }
        Ok(Vr::try_parse(&[probe[4], probe[5]]).is_some())
    }

    /// CP-246 content is Implicit VR Little Endian, so a conformant item is read
    /// implicitly. Only when the first inner element cannot be implicit — its
    /// 32-bit implicit length would run past the item — but does fit as an
    /// explicit-VR header, is a non-conformant explicit item read as explicit
    /// (keeps legacy UN sequences whose inner content was written explicitly).
    ///
    /// Must be called with the little-endian content byte order already set.
    fn cp246_inner_is_explicit(&mut self) -> Result<bool, Error> {
        let pos = self.position()?;
        let result = self.cp246_inner_is_explicit_at(pos);
        self.source.goto(pos).map_err(|e| io_err(e, pos))?;
        result
    }

    fn cp246_inner_is_explicit_at(&mut self, pos: u64) -> Result<bool, Error> {
        if self.source.require(8).is_err() {
            return Ok(false);
        }
        let g = self.source.read_u16().map_err(|e| io_err_need(e, pos, 2))?;
        let e = self
            .source
            .read_u16()
            .map_err(|e| io_err_need(e, pos + 2, 2))?;
        if (g, e) != (0xFFFE, 0xE000) {
            return Ok(false);
        }
        let item_len = self
            .source
            .read_u32()
            .map_err(|e| io_err_need(e, pos + 4, 4))? as u64;
        if item_len < 8 || self.source.require(12).is_err() {
            return Ok(false);
        }
        let mut head = [0u8; 12];
        self.source
            .read_exact(&mut head)
            .map_err(|e| io_err_need(e, pos + 8, 12))?;
        let vr = Vr::try_parse(&[head[4], head[5]]);
        if item_len == 0xFFFF_FFFF {
            // Undefined-length item: an explicit undefined-length SQ header is a
            // clear explicit signal; otherwise the CP-246 content is implicit.
            let undefined_sq = matches!(vr, Some(v) if !v.is_16bit_length())
                && u32::from_le_bytes([head[8], head[9], head[10], head[11]]) == 0xFFFF_FFFF;
            return Ok(undefined_sq);
        }
        let implicit_len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as u64;
        let implicit_fits = implicit_len == 0xFFFF_FFFF || 8 + implicit_len <= item_len;
        let explicit_fits = match vr {
            Some(vr) if vr.is_16bit_length() => {
                8 + u16::from_le_bytes([head[6], head[7]]) as u64 <= item_len
            }
            Some(_) => {
                12 + u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as u64 <= item_len
            }
            None => false,
        };
        Ok(!implicit_fits && explicit_fits)
    }

    /// fo-dicom `IsPrivateSequence`: true when the value at the current
    /// position starts with an item or a sequence delimiter tag.
    fn next_is_item_or_delimiter(&mut self) -> Result<bool, Error> {
        let pos = self.position()?;
        if self.source.require(4).is_err() {
            return Ok(false);
        }
        let mut tag = [0u8; 4];
        self.source
            .read_exact(&mut tag)
            .map_err(|e| io_err_need(e, pos, 4))?;
        self.source.goto(pos).map_err(|e| io_err(e, pos))?;
        // item/delimiter tags follow the transfer syntax byte order
        let item = (tag[0] == 0xFF && tag[1] == 0xFE && tag[2] == 0xE0 && tag[3] == 0x00)
            || (tag[0] == 0xFE && tag[1] == 0xFF && tag[2] == 0x00 && tag[3] == 0xE0);
        let delimiter = (tag[0] == 0xFF && tag[1] == 0xFE && tag[2] == 0xDD && tag[3] == 0xE0)
            || (tag[0] == 0xFE && tag[1] == 0xFF && tag[2] == 0xE0 && tag[3] == 0xDD);
        Ok(item || delimiter)
    }

    /// Reads a value buffer, wrapping binary VRs so downstream typed reads
    /// are always native LE.
    fn read_value_buffer(&mut self, vr: Vr, length: u64, at: u64) -> Result<SharedBuffer, Error> {
        if length == 0 {
            return Ok(EmptyBuffer::shared());
        }
        // `require` reports the real offset and byte count on truncation.
        self.source.require(length).map_err(Error::Reader)?;
        let large = length >= self.options.large_object_size;
        let raw = if large {
            match self.options.large_policy {
                LargeValuePolicy::OnDemand => match self
                    .source
                    .window_at_current(length)
                    .map_err(|e| io_err(e, at))?
                {
                    Some(window) => {
                        self.source.skip(length).map_err(|e| io_err(e, at))?;
                        window
                    }
                    None => self.source.read_buffer(length).map_err(|e| io_err(e, at))?,
                },
                // `Skip` never reaches this point: the element and fragment
                // loops drop the value before asking for a buffer (M1), so a
                // skipped value can never be stored as an empty placeholder.
                LargeValuePolicy::Skip | LargeValuePolicy::Materialize => {
                    self.source.read_buffer(length).map_err(|e| io_err(e, at))?
                }
            }
        } else {
            self.source.read_buffer(length).map_err(|e| io_err(e, at))?
        };
        let unit = vr.byte_swap_size();
        let needs_swap = unit > 1
            && ((self.big_endian && !cfg!(target_endian = "big"))
                || (!self.big_endian && cfg!(target_endian = "big")));
        if needs_swap {
            Ok(Arc::new(EndianBuffer::new(raw, unit)))
        } else {
            Ok(raw)
        }
    }

    /// Tag resolution: interning plus private creator binding (RD-05).
    fn resolve_tag(&mut self, group: u16, element: u16) -> Tag {
        let tag = crate::model::tag_index::lookup_or_create(group, element);
        if tag.is_private() && tag.private_creator().is_none() && element > 0x00FF {
            let block = (element >> 8) as u8;
            if let Some(creator) = self
                .private_creators
                .get(&group)
                .and_then(|slots| slots[block as usize].as_ref())
            {
                return Tag::private(group, element, Arc::clone(creator));
            }
        }
        tag
    }

    /// VR determination (§7) with T1/T2 fallbacks; returns the VR and
    /// whether the element turned out explicit.
    fn parse_vr(&mut self, tag: &Tag, at: u64) -> Result<(Vr, bool), Error> {
        if self.explicit_vr {
            let pos = self.position()?;
            let mut code = [0u8; 2];
            self.source
                .read_exact(&mut code)
                .map_err(|e| io_err_need(e, at, 2))?;
            match Vr::try_parse(&code) {
                Some(vr) => return Ok((vr, true)),
                None => {
                    if code == [0x20, 0x20] {
                        // T1: two spaces — dictionary VR, stay explicit
                        return Ok((self.dictionary_vr(tag), true));
                    }
                    // T2: rewind the two VR bytes and read this element
                    // implicitly (those bytes are the head of a 32-bit length)
                    self.source.goto(pos).map_err(|e| io_err(e, at))?;
                }
            }
        }
        Ok((self.dictionary_vr(tag), false))
    }

    /// Length field per VR/syntax (§7).
    fn parse_length(&mut self, vr: Vr, explicit: bool, at: u64) -> Result<u32, Error> {
        if explicit && vr.is_16bit_length() {
            let v = self.source.read_u16().map_err(|e| io_err_need(e, at, 2))?;
            Ok(v as u32)
        } else {
            if explicit {
                // A standard SV/UV carries reserved(2) + u32 like other 32-bit
                // VRs, but prior fo-dicom versions wrote a bare 2-byte length.
                // Read the first u16: a non-zero value for SV/UV is that legacy
                // length; otherwise it was the reserved field of the standard
                // 32-bit form (fo-dicom DicomReader.ParseLength, #177).
                let first = self.source.read_u16().map_err(|e| io_err_need(e, at, 2))?;
                if matches!(vr, Vr::Sv | Vr::Uv) && first != 0 {
                    return Ok(first as u32);
                }
            }
            self.source.read_u32().map_err(|e| io_err_need(e, at, 4))
        }
    }

    /// Enters one nesting level; enforces the depth cap.
    fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > self.options.max_depth {
            // CORE-23: report where the overflow happened, not a placeholder
            let position = self.position().unwrap_or(0);
            return Err(Error::Reader(ReaderError::TooDeep {
                limit: self.options.max_depth,
                position,
            }));
        }
        Ok(())
    }

    /// Switches parser and source byte order together.
    fn set_byte_order(&mut self, big_endian: bool) {
        self.big_endian = big_endian;
        self.source.endian = if big_endian {
            Endian::Big
        } else {
            Endian::Little
        };
    }

    /// Parses one element (or delimiter) header and, for leaves, its value.
    /// Returns before descending into any nested level.
    fn step(
        &mut self,
        stop: Option<&dyn Fn(&ParseState) -> bool>,
        mode: Mode,
        level_start: u64,
        previous: Option<(u16, u16)>,
    ) -> Result<Step, Error> {
        if let Mode::Limited { limit } = mode {
            if self.position()? - level_start >= limit {
                return Ok(Step::End(LevelEnd::Limit));
            }
        }
        let tag_pos = self.position()?;
        // T6/T8: truncation ends the level instead of erroring
        if self.source.require(4).is_err() {
            return Ok(Step::End(LevelEnd::Eof));
        }
        let (group, element) = self.read_tag(tag_pos)?;

        // A header must fit inside a defined-length item/sequence. If only the
        // tag is inside the limit, the header is truncated: end the level (T6)
        // instead of consuming bytes that belong to the parent level.
        if let Mode::Limited { limit } = mode {
            if self.position()? + 4 > level_start + limit {
                return Ok(Step::End(LevelEnd::Limit));
            }
        }

        if group == 0xFFFE {
            let length = self
                .source
                .read_u32()
                .map_err(|e| io_err_need(e, tag_pos + 4, 4))?;
            // PS3.5 §7.5: item and sequence delimiters have a zero length. A
            // nonzero length is nonconformant but unambiguous: skip the declared
            // value bytes so the stream stays aligned for the next element.
            if matches!(element, 0xE00D | 0xE0DD) && length != 0 && length != 0xFFFF_FFFF {
                self.source
                    .skip(length as u64)
                    .map_err(|e| io_err(e, tag_pos + 4))?;
            }
            let defined = (length != 0xFFFF_FFFF).then_some(length);
            return Ok(match element {
                0xE00D => match mode {
                    Mode::ItemDelimiter => Step::End(LevelEnd::ItemEnd),
                    _ => Step::Continue, // stray delimiter: consumed
                },
                0xE0DD => Step::End(LevelEnd::SequenceEnd),
                0xE000 => {
                    // `value_start` is where the item body begins; the
                    // defined-length mode counts from there
                    let raw = (group, element);
                    let value_start = self.position()?;
                    if mode == Mode::Top {
                        Step::Orphan {
                            raw,
                            pos: value_start,
                            defined,
                        }
                    } else {
                        Step::Item {
                            raw,
                            pos: value_start,
                            defined,
                        }
                    }
                }
                _ => {
                    return Err(Error::Reader(ReaderError::Invalid {
                        message: format!("unexpected item-family tag FFFE,{element:04X}"),
                        position: tag_pos,
                    }))
                }
            });
        }

        let tag = self.resolve_tag(group, element);

        if let Some(stop) = stop {
            let state = ParseState {
                previous_tag: previous,
                tag: (tag.group(), tag.element()),
                sequence_depth: self.depth,
            };
            if stop(&state) {
                self.source.goto(tag_pos).map_err(|e| io_err(e, tag_pos))?;
                return Ok(Step::Stopped);
            }
        }

        let (raw_vr, explicit) = match self.parse_vr(&tag, tag_pos) {
            Ok(v) => v,
            Err(Error::Reader(ReaderError::Eof { .. })) => return Ok(Step::End(LevelEnd::Eof)),
            Err(e) => return Err(e),
        };
        // The length field is laid out per the VR that was *written*: T9
        // rewrites UN to UL only after this, since UN uses the long form.
        let length = match self.parse_length(raw_vr, explicit, tag_pos) {
            Ok(l) => l,
            Err(Error::Reader(ReaderError::Eof { .. })) => return Ok(Step::End(LevelEnd::Eof)),
            Err(e) => return Err(e),
        };
        // The value begins *after* the length field, not after the VR. Using the
        // pre-length position as a sequence's `level_start` was off by the
        // header width.
        let value_start = self.position()?;
        let mut vr = self.fix_group_length_vr(element, raw_vr);
        // fo-dicom `DicomReader.ParseValue`: an UN element is re-resolved
        // through the dictionary now that the length has been read, so a
        // UN-encoded SQ is still recognised as a sequence.
        if vr == Vr::Un {
            let dict_vr = self.dictionary_vr(&tag);
            if dict_vr != Vr::Un {
                vr = dict_vr;
            }
        }
        let defined = (length != 0xFFFF_FFFF).then_some(length);
        let raw = (group, element);

        // A defined-length value may not extend past its enclosing
        // defined-length item/sequence: the declared length is not permission to
        // read the parent's bytes. End the level at its boundary (the level
        // caller then seeks to the declared end) instead of reading across it.
        if let (Some(length), Mode::Limited { limit }) = (defined, mode) {
            let target = level_start + limit;
            if value_start + u64::from(length) > target {
                // A truncated long-form explicit-VR header (OB/OW/SQ/UN/… needs
                // 8 bytes after the tag, not 4) was read past the item end, so
                // `value_start` is already beyond the declared bound. The item
                // end is authoritative: resume the parent there instead of
                // inside the item, or the following element/item desynchronises.
                if value_start > target {
                    self.source.goto(target).map_err(|e| io_err(e, target))?;
                }
                return Ok(Step::End(LevelEnd::Limit));
            }
        }

        // fo-dicom `IsPrivateSequence`: a private tag the dictionary calls SQ
        // only descends when the value really starts with an item or sequence
        // delimiter; otherwise it is read as UN bytes.
        let mut private_probe = false;
        if vr == Vr::Sq && tag.is_private() && length > 0 {
            if self.next_is_item_or_delimiter()? {
                private_probe = true;
            } else {
                vr = Vr::Un;
            }
        }
        // CP-246: the same probe applies to a non-private *defined-length* UN
        // the dictionary calls SQ. Without it, a raw UN value that is not
        // item-shaped would be parsed as implicit elements inside a bounded
        // window (garbage, but no desync); keeping it UN is the safe read.
        if vr == Vr::Sq
            && !tag.is_private()
            && raw_vr == Vr::Un
            && defined.is_some()
            && length > 0
            && !self.next_is_item_or_delimiter()?
        {
            vr = Vr::Un;
        }

        if vr == Vr::Sq {
            let cp246 = raw_vr == Vr::Un && !tag.is_private();
            return Ok(Step::Sequence {
                raw,
                tag,
                pos: value_start,
                defined,
                private_probe,
                cp246,
            });
        }

        let Some(length) = defined else {
            if tag.is_private() {
                // T4: private undefined-length value. Only a value that really
                // starts with an item or sequence delimiter can be read as a
                // sequence (fo-dicom `IsPrivateSequence`). Otherwise the bytes
                // are not usable SQ items; consume them up to the sequence
                // delimiter as a skipped raw value so the stream stays aligned
                // instead of mis-parsing them as dataset elements.
                if self.next_is_item_or_delimiter()? {
                    return Ok(Step::Sequence {
                        raw,
                        tag,
                        pos: value_start,
                        defined: None,
                        private_probe: true,
                        cp246: false,
                    });
                }
                let buffer = self.skip_undefined_value_to_delimiter()?;
                return Ok(Step::Element {
                    raw,
                    tag,
                    vr: Vr::Un,
                    pos: tag_pos,
                    buffer,
                });
            }
            // PS3.5 §6.2.2: an element with an undefined length whose VR is UN
            // holds a sequence of items encoded in Implicit VR Little Endian,
            // whatever the enclosing dataset's transfer syntax (CP-246). This
            // holds for explicit VR too - an actual sequence would carry SQ.
            if vr == Vr::Un {
                return Ok(Step::Sequence {
                    raw,
                    tag,
                    pos: value_start,
                    defined: None,
                    private_probe: false,
                    cp246: true,
                });
            }
            return Ok(Step::Fragments {
                raw,
                tag,
                vr,
                pos: value_start,
            });
        };

        // FILE-02 `SkipLargeTags`: like fo-dicom's FileByteSource, a skipped
        // large value leaves *no* element in the dataset (an empty placeholder
        // would be indistinguishable from a genuinely empty attribute).
        if self.options.large_policy == LargeValuePolicy::Skip
            && length as u64 >= self.options.large_object_size
        {
            if self.source.require(length as u64).is_err() {
                if self.options.partial_on_eof {
                    self.suspended = true;
                    return Ok(Step::End(LevelEnd::Eof));
                }
                return Ok(Step::End(LevelEnd::Eof));
            }
            self.source
                .skip(length as u64)
                .map_err(|e| io_err(e, value_start))?;
            return Ok(Step::Continue);
        }

        // A truncated *value* is a real error (its declared length is
        // known); only truncated headers end a level gracefully (T8).
        // Whole-file parsing may instead keep the readable prefix.
        let buffer = match self.read_value_buffer(vr, length as u64, value_start) {
            Ok(buffer) => buffer,
            Err(Error::Reader(ReaderError::Eof { .. })) if self.options.partial_on_eof => {
                self.suspended = true;
                return Ok(Step::End(LevelEnd::Eof));
            }
            Err(e) => return Err(e),
        };
        // RD-05: private creator tracking
        if tag.is_private() && tag.is_private_creator_slot() && vr == Vr::Lo {
            let creator = Arc::new(crate::model::private_creator::PrivateCreator::new(
                String::from_utf8_lossy(&buffer.data()?)
                    .trim_end_matches([' ', '\0'])
                    .to_string(),
            ));
            self.private_creators
                .entry(group)
                .or_insert_with(|| Box::new(std::array::from_fn(|_| None)))
                [(element & 0xFF) as usize] = Some(creator);
        }
        Ok(Step::Element {
            raw,
            tag,
            vr,
            pos: tag_pos,
            buffer,
        })
    }

    /// Best-effort recovery for an undefined-length private value that is not a
    /// sequence: consume bytes up to and including the `SequenceDelimitationItem`
    /// (`FFFE,E0DD`) so the following element stays aligned, and return an empty
    /// (skipped) buffer. At EOF without a delimiter the source is left at EOF.
    fn skip_undefined_value_to_delimiter(&mut self) -> Result<SharedBuffer, Error> {
        // Item/delimiter tags follow the transfer syntax byte order; match both.
        let le = [0xFEu8, 0xFF, 0xDD, 0xE0];
        let be = [0xFFu8, 0xFE, 0xE0, 0xDD];
        let mut window = [0u8; 4];
        let mut filled = 0usize;
        while let Ok(byte) = self.source.read_u8() {
            window = [window[1], window[2], window[3], byte];
            filled = (filled + 1).min(4);
            if filled == 4 && (window == le || window == be) {
                // the delimiter is tag(4) + length(4); consume its length too
                let _ = self.source.read_u32();
                break;
            }
        }
        Ok(EmptyBuffer::shared())
    }

    /// Fragment sequences (§7): fragment 0 is the basic offset table in
    /// 4-byte units; the terminator is `FFFE,E0DD`; EOF is tolerated (T6).
    fn parse_fragment_sequence(
        &mut self,
        observer: &mut impl ParseObserver,
        tag: &Tag,
        vr: Vr,
        start: u64,
    ) -> Result<(), Error> {
        let mut offset_table_seen = false;
        loop {
            if self.source.require(4).is_err() {
                break; // T6: missing final delimiter at EOF
            }
            let pos = self.position()?;
            let (g, e) = self.read_tag(pos)?;
            match (g, e) {
                (0xFFFE, 0xE000) => {
                    let length = self
                        .source
                        .read_u32()
                        .map_err(|e| io_err_need(e, pos + 4, 4))?;
                    if length == 0xFFFF_FFFF {
                        return Err(Error::Reader(ReaderError::Invalid {
                            message: "fragment with undefined length".into(),
                            position: pos,
                        }));
                    }
                    let first = !offset_table_seen;
                    offset_table_seen = true;
                    if self.options.large_policy == LargeValuePolicy::Skip
                        && length as u64 >= self.options.large_object_size
                    {
                        // same policy as elements: the observer is not shown the
                        // value, and fo-dicom additionally withholds the whole
                        // sequence once any fragment is skipped (M1).
                        if self.source.require(length as u64).is_err() {
                            break;
                        }
                        self.source
                            .skip(length as u64)
                            .map_err(|e| io_err(e, pos))?;
                        observer.on_fragment_skipped(&self.ctx(pos))?;
                        continue;
                    }
                    // Every fragment payload (the basic offset table and the
                    // data fragments) is an opaque byte stream. Reading it with
                    // the pixel VR's unit size would byte-swap an OW fragment on
                    // a big-endian host and corrupt the compressed frame, so
                    // both are read as OB (unit 1), matching the writer.
                    let read_vr = Vr::Ob;
                    let buffer = match self.read_value_buffer(read_vr, length as u64, pos) {
                        Ok(buffer) => buffer,
                        Err(Error::Reader(ReaderError::Eof { .. }))
                            if self.options.partial_on_eof =>
                        {
                            self.suspended = true;
                            return Ok(());
                        }
                        Err(e) => return Err(e),
                    };
                    if first {
                        let table = decode_offset_table(&buffer, self.big_endian)?;
                        observer.on_begin_fragment(&self.ctx(start), tag, vr, table)?;
                    } else {
                        // PS3.5 A.4: a fragment value is even-length. A stray odd
                        // length is padded to even for consumers (fo-dicom
                        // EvenLengthBuffer) instead of silently accepted.
                        if length % 2 == 1 {
                            tracing::warn!(length, "odd-length pixel data fragment padded to even");
                        }
                        observer.on_fragment(
                            &self.ctx(pos),
                            EvenLengthBuffer::wrap_if_needed(buffer),
                        )?;
                    }
                }
                (0xFFFE, 0xE0DD) => {
                    // the delimiter is tag(4) + length(4); consuming only the
                    // tag leaves the length in the stream and desynchronizes
                    // every following element
                    let length = self
                        .source
                        .read_u32()
                        .map_err(|e| io_err_need(e, pos + 4, 4))?;
                    // PS3.5 §7.5: a delimiter's length shall be 0; skip any
                    // nonconformant value bytes so the stream stays aligned.
                    if length != 0 && length != 0xFFFF_FFFF {
                        self.source
                            .skip(length as u64)
                            .map_err(|e| io_err(e, pos + 4))?;
                        debug_assert_eq!(self.position()?, pos + 8 + length as u64);
                    }
                    break;
                }
                _ => {
                    return Err(Error::Reader(ReaderError::Invalid {
                        message: format!(
                            "unexpected tag in DICOM fragment sequence {g:04X},{e:04X}"
                        ),
                        position: pos,
                    }))
                }
            }
        }
        // an empty encapsulated value still announces begin/end so the
        // element exists in the built dataset
        if !offset_table_seen {
            observer.on_begin_fragment(&self.ctx(start), tag, vr, Vec::new())?;
        }
        observer.on_end_fragment(&self.here()?)?;
        Ok(())
    }
}

fn io_err(e: std::io::Error, position: u64) -> Error {
    io_err_need(e, position, 0)
}

/// Like [`io_err`], but records how many bytes the read expected. `read_exact`
/// cannot report that from `io::Error`, so the call site supplies it.
fn io_err_need(e: std::io::Error, position: u64, needed: u64) -> Error {
    match e.kind() {
        std::io::ErrorKind::UnexpectedEof => Error::Reader(ReaderError::Eof { position, needed }),
        _ => Error::Reader(ReaderError::Other(e.to_string())),
    }
}

impl DicomReader {
    /// Parses a dataset from `source`. A hit stop predicate rewinds the
    /// source to the element start and yields [`ReadOutcome::Stopped`]
    /// (FILE-01).
    pub fn read<R: Read + Seek>(
        &self,
        source: &mut ByteSourceReader<R>,
        syntax: &TransferSyntax,
        observer: &mut impl ParseObserver,
        stop: Option<&dyn Fn(&ParseState) -> bool>,
    ) -> Result<ReadOutcome, Error> {
        let mut private_creators = HashMap::new();
        let mut parser = Parser {
            source,
            options: self,
            depth: 0,
            explicit_vr: syntax.is_explicit_vr(),
            big_endian: syntax.endian() == Endian::Big,
            stopped: false,
            suspended: false,
            private_creators: &mut private_creators,
        };
        parser.parse_level(observer, stop, Mode::Top, 0)?;
        Ok(if parser.suspended {
            ReadOutcome::Suspended
        } else if parser.stopped {
            ReadOutcome::Stopped
        } else {
            ReadOutcome::Success
        })
    }
}

impl<'a, R: Read + Seek> Parser<'a, R> {
    /// The main loop for one nesting level; the only recursive function.
    fn parse_level(
        &mut self,
        observer: &mut impl ParseObserver,
        stop: Option<&dyn Fn(&ParseState) -> bool>,
        mode: Mode,
        level_start: u64,
    ) -> Result<LevelEnd, Error> {
        let mut previous: Option<(u16, u16)> = None;
        let end = 'level: loop {
            match self.step(stop, mode, level_start, previous)? {
                Step::Continue => {}
                Step::End(end) => break 'level end,
                Step::Stopped => {
                    self.stopped = true;
                    break 'level LevelEnd::Eof;
                }
                Step::Element {
                    raw,
                    tag,
                    vr,
                    pos,
                    buffer,
                } => {
                    observer.on_element(&self.ctx(pos), &tag, vr, buffer)?;
                    previous = Some(raw);
                }
                Step::Fragments { raw, tag, vr, pos } => {
                    self.parse_fragment_sequence(observer, &tag, vr, pos)?;
                    previous = Some(raw);
                }
                Step::Sequence {
                    raw,
                    tag,
                    pos,
                    defined,
                    private_probe,
                    cp246,
                } => {
                    observer.on_begin_sequence(&self.ctx(pos), &tag, defined)?;
                    let (saved_explicit, saved_be) = (self.explicit_vr, self.big_endian);
                    if private_probe {
                        self.explicit_vr =
                            self.probe_inner_explicit(defined.unwrap_or(u32::MAX))?;
                        // IO-03: only an implicitly-encoded outer dataset gets the
                        // little-endian private-subtree assumption (T4). An explicit
                        // big-endian dataset must keep its byte order; forcing LE
                        // here corrupted private sequences.
                        if !saved_explicit && self.explicit_vr {
                            self.set_byte_order(false);
                        }
                    } else if cp246 {
                        // PS3.5 CP-246 (IO-04/IO-B-02): UN + undefined-length
                        // content is Implicit VR Little Endian regardless of the
                        // dataset's byte order, so both `explicit_vr` and the byte
                        // order are reset. A non-conformant explicitly-encoded
                        // item is still read explicitly (see the probe), which
                        // keeps older files parseable.
                        self.set_byte_order(false);
                        self.explicit_vr = self.cp246_inner_is_explicit()?;
                    }
                    self.enter()?;
                    let inner = match defined {
                        Some(len) => Mode::Limited { limit: len as u64 },
                        None => Mode::SequenceDelimiter,
                    };
                    let end = self.parse_level(observer, None, inner, pos)?;
                    self.depth -= 1;
                    self.explicit_vr = saved_explicit;
                    self.set_byte_order(saved_be);
                    observer.on_end_sequence(&self.here()?)?;
                    if end == LevelEnd::Eof {
                        break 'level LevelEnd::Eof;
                    }
                    previous = Some(raw);
                }
                Step::Item { raw, pos, defined } => {
                    let end = self.parse_item(observer, pos, defined)?;
                    // T7: the enclosing SQ may end where an item was expected
                    if matches!(end, LevelEnd::SequenceEnd | LevelEnd::Eof) {
                        break 'level end;
                    }
                    previous = Some(raw);
                }
                Step::Orphan { raw, pos, defined } => {
                    // T5: an item tag at dataset level with no SQ header is an
                    // anonymous undefined-length sequence; its first item was
                    // already consumed by `step`. PS3.5 §7.5 gives it the item
                    // tag as identity (fo-dicom DicomTag.Item).
                    let unknown = Tag::ITEM;
                    observer.on_begin_sequence(&self.ctx(pos), &unknown, None)?;
                    self.enter()?;
                    let end = self.parse_item(observer, pos, defined)?;
                    let end = match (end, defined) {
                        (LevelEnd::Eof, _) | (_, Some(_)) => end,
                        // further sibling items and the sequence delimiter
                        // belong to this anonymous sequence
                        (_, None) => {
                            self.parse_level(observer, None, Mode::SequenceDelimiter, pos)?
                        }
                    };
                    self.depth -= 1;
                    observer.on_end_sequence(&self.here()?)?;
                    if end == LevelEnd::Eof {
                        break 'level LevelEnd::Eof;
                    }
                    previous = Some(raw);
                }
            }
        };
        // A defined-length level always ends at its declared end, even when its
        // body ended early (a value that overruns the item, T6): the parent must
        // resume there, not inside the item's bytes. (A truncated long-form
        // header that read *past* the end is clamped back to it at the overrun
        // guard above, so this forward seek only has to handle short bodies.)
        if let Mode::Limited { limit } = mode {
            let target = level_start.saturating_add(limit);
            if self.position()? < target {
                self.source.goto(target).map_err(|e| io_err(e, target))?;
            }
        }
        Ok(end)
    }

    /// One sequence item body (`FFFE,E000` and its length already read).
    fn parse_item(
        &mut self,
        observer: &mut impl ParseObserver,
        pos: u64,
        defined: Option<u32>,
    ) -> Result<LevelEnd, Error> {
        observer.on_begin_sequence_item(&self.ctx(pos), defined)?;
        self.enter()?;
        let inner = match defined {
            Some(len) => Mode::Limited { limit: len as u64 },
            None => Mode::ItemDelimiter,
        };
        let end = self.parse_level(observer, None, inner, pos)?;
        self.depth -= 1;
        observer.on_end_sequence_item(&self.here()?)?;
        Ok(end)
    }
}

/// Basic offset table entries follow the transfer syntax byte order, four bytes
/// each. PS3.5 requires the item length to be a multiple of 4; a malformed
/// trailing partial entry is ignored without shifting the complete entries (and
/// without a word-swap unit assertion firing on an odd length).
fn decode_offset_table(buffer: &SharedBuffer, big_endian: bool) -> Result<Vec<u32>, Error> {
    let data = buffer.data()?;
    let (chunks, remainder) = data.as_chunks::<4>();
    let mut entries = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let raw = [chunk[0], chunk[1], chunk[2], chunk[3]];
        entries.push(if big_endian {
            u32::from_be_bytes(raw)
        } else {
            u32::from_le_bytes(raw)
        });
    }
    if !remainder.is_empty() {
        tracing::debug!(
            bytes = remainder.len(),
            "basic offset table length is not a multiple of 4; ignoring trailing bytes"
        );
    }
    Ok(entries)
}

/// The default cap on a deflated dataset body, in bytes. A few hundred bytes of
/// DEFLATE can expand to gigabytes (a decompression bomb), so the default read
/// path refuses to inflate past this; callers that legitimately store larger
/// deflated datasets raise [`crate::io::file::ReadOptions::max_inflated_bytes`].
pub const DEFAULT_MAX_INFLATED_BYTES: u64 = 1024 * 1024 * 1024;

/// RD-04: inflates a deflated dataset body. The file meta group is never
/// deflated, so this applies to the dataset bytes only.
///
/// The output is capped at [`DEFAULT_MAX_INFLATED_BYTES`]; use
/// [`inflate_stream_limited`] with an explicit cap to read a larger body.
///
/// PS3.5 A.5: the deflated transfer syntax uses **raw DEFLATE (RFC 1951)** — no
/// zlib header/trailer. Raw is therefore the default decoder; a zlib-wrapped
/// (non-conformant) stream is only tried first when its 2-byte header actually
/// validates as zlib, so a raw stream can never be fed to the zlib decoder and
/// silently mis-decoded. A header we fail to recognise still falls back to zlib
/// after raw fails (tolerance).
pub fn inflate(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    inflate_stream_limited(
        &mut std::io::Cursor::new(bytes),
        0,
        DEFAULT_MAX_INFLATED_BYTES,
    )
}

/// True when `bytes` starts with a syntactically valid zlib (RFC 1950) header.
fn looks_like_zlib(bytes: &[u8]) -> bool {
    if bytes.len() < 2 {
        return false;
    }
    let (cmf, flg) = (bytes[0], bytes[1]);
    (cmf & 0x0F) == 8 && (cmf >> 4) <= 7 && (u16::from(cmf) * 256 + u16::from(flg)) % 31 == 0
}

/// Streaming variant of [`inflate`] for a seekable source: decompresses the
/// deflate body starting at `start` **without first materialising the compressed
/// payload**, so the file path peaks at one full (inflated) copy plus the codec's
/// small window instead of compressed + inflated. Same header-sniffed
/// zlib/raw order and the same typed error on a truncated stream.
pub fn inflate_stream<R: Read + Seek>(source: &mut R, start: u64) -> Result<Vec<u8>, Error> {
    use std::io::Read as _;
    source
        .seek(std::io::SeekFrom::Start(start))
        .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate seek: {e}"))))?;
    let mut hdr = [0u8; 2];
    let zlib_first = source.read_exact(&mut hdr).is_ok() && looks_like_zlib(&hdr);
    source
        .seek(std::io::SeekFrom::Start(start))
        .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate seek: {e}"))))?;
    let mut out = Vec::new();
    if zlib_first {
        let mut decoder = flate2::read::ZlibDecoder::new(&mut *source);
        if decoder.read_to_end(&mut out).is_ok() {
            return Ok(out);
        }
        out.clear();
        source
            .seek(std::io::SeekFrom::Start(start))
            .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate seek: {e}"))))?;
    }
    let raw = flate2::read::DeflateDecoder::new(&mut *source).read_to_end(&mut out);
    if raw.is_ok() {
        return Ok(out);
    }
    if !zlib_first {
        out.clear();
        source
            .seek(std::io::SeekFrom::Start(start))
            .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate seek: {e}"))))?;
        if flate2::read::ZlibDecoder::new(&mut *source)
            .read_to_end(&mut out)
            .is_ok()
        {
            return Ok(out);
        }
    }
    Err(Error::Reader(ReaderError::Other(format!(
        "deflate: {}",
        raw.unwrap_err()
    ))))
}

/// Streaming deflate decoder with a hard cap on the inflated output.
pub fn inflate_stream_limited<R: Read + Seek>(
    source: &mut R,
    start: u64,
    max_output: u64,
) -> Result<Vec<u8>, Error> {
    source
        .seek(std::io::SeekFrom::Start(start))
        .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate seek: {e}"))))?;
    let mut hdr = [0u8; 2];
    let zlib_first = source.read_exact(&mut hdr).is_ok() && looks_like_zlib(&hdr);
    source
        .seek(std::io::SeekFrom::Start(start))
        .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate seek: {e}"))))?;

    fn read_limited<R: Read>(
        decoder: &mut R,
        out: &mut Vec<u8>,
        max_output: u64,
    ) -> std::io::Result<()> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let read = decoder.read(&mut chunk)?;
            if read == 0 {
                return Ok(());
            }
            if (out.len() as u64).saturating_add(read as u64) > max_output {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::FileTooLarge,
                    "deflated dataset exceeds the maximum inflated size",
                ));
            }
            out.extend_from_slice(&chunk[..read]);
        }
    }

    let mut out = Vec::new();
    if zlib_first {
        let mut decoder = flate2::read::ZlibDecoder::new(&mut *source);
        match read_limited(&mut decoder, &mut out, max_output) {
            Ok(()) => return Ok(out),
            Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => {
                return Err(Error::Reader(ReaderError::Other(error.to_string())))
            }
            Err(_) => {}
        }
        out.clear();
        source
            .seek(std::io::SeekFrom::Start(start))
            .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate seek: {e}"))))?;
    }
    let mut raw_decoder = flate2::read::DeflateDecoder::new(&mut *source);
    let raw_error = match read_limited(&mut raw_decoder, &mut out, max_output) {
        Ok(()) => return Ok(out),
        Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => {
            return Err(Error::Reader(ReaderError::Other(error.to_string())))
        }
        Err(error) => error,
    };
    if !zlib_first {
        out.clear();
        source
            .seek(std::io::SeekFrom::Start(start))
            .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate seek: {e}"))))?;
        let mut zlib_decoder = flate2::read::ZlibDecoder::new(&mut *source);
        match read_limited(&mut zlib_decoder, &mut out, max_output) {
            Ok(()) => return Ok(out),
            Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => {
                return Err(Error::Reader(ReaderError::Other(error.to_string())))
            }
            Err(_) => {}
        }
    }
    Err(Error::Reader(ReaderError::Other(format!(
        "deflate: {raw_error}"
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn raw_deflate(plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder =
            flate2::write::DeflateEncoder::new(&mut out, flate2::Compression::default());
        encoder.write_all(plain).unwrap();
        encoder.finish().unwrap();
        out
    }

    fn zlib_deflate(plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = flate2::write::ZlibEncoder::new(&mut out, flate2::Compression::default());
        encoder.write_all(plain).unwrap();
        encoder.finish().unwrap();
        out
    }

    #[test]
    fn inflate_stream_matches_inflate_for_raw_and_zlib_at_an_offset() {
        let plain = b"a deflated dataset body";
        for body in [raw_deflate(plain), zlib_deflate(plain)] {
            let mut source = Cursor::new([b"PRE".as_slice(), body.as_slice()].concat());
            let streamed = inflate_stream(&mut source, 3).unwrap();
            assert_eq!(streamed, plain);
            assert_eq!(
                streamed,
                inflate(&body).unwrap(),
                "streaming == slice decoder"
            );
        }
    }

    #[test]
    fn inflate_stream_truncated_stream_is_a_typed_error() {
        let body = raw_deflate(b"a deflated dataset body that will be truncated");
        let truncated = &body[..body.len() - 4];
        let mut source = Cursor::new(truncated.to_vec());
        match inflate_stream(&mut source, 0) {
            Err(Error::Reader(ReaderError::Other(message))) => {
                assert!(
                    message.contains("deflate"),
                    "typed deflate error, got {message}"
                );
            }
            other => panic!("expected ReaderError::Other(deflate), got {other:?}"),
        }
    }

    #[test]
    fn inflate_stream_limited_rejects_an_oversized_output() {
        let body = raw_deflate(&vec![b'x'; 4096]);
        match inflate_stream_limited(&mut Cursor::new(body), 0, 1024) {
            Err(Error::Reader(ReaderError::Other(message))) => {
                assert!(message.contains("maximum"), "got {message}");
            }
            other => panic!("expected the output limit error, got {other:?}"),
        }
    }

    fn read_with(
        bytes: &[u8],
        options: DicomReader,
    ) -> Result<crate::model::dataset::Dataset, Error> {
        use crate::io::dataset_builder::DatasetBuilder;
        let syntax = (*crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).clone();
        let mut source =
            ByteSourceReader::new(std::io::Cursor::new(bytes.to_vec()), syntax.endian());
        let mut builder = DatasetBuilder::with_transfer_syntax(syntax.clone());
        options.read(&mut source, &syntax, &mut builder, None)?;
        Ok(builder.into_dataset())
    }

    fn read_implicit(bytes: &[u8]) -> Result<crate::model::dataset::Dataset, Error> {
        use crate::io::dataset_builder::DatasetBuilder;
        let syntax = crate::model::transfer_syntax::syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone();
        let mut source =
            ByteSourceReader::new(std::io::Cursor::new(bytes.to_vec()), syntax.endian());
        let mut builder = DatasetBuilder::with_transfer_syntax(syntax.clone());
        DicomReader::default().read(&mut source, &syntax, &mut builder, None)?;
        Ok(builder.into_dataset())
    }

    /// An explicit-VR element with the length form its VR requires.
    fn explicit_element(group: u16, element: u16, vr: Vr, value: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&group.to_le_bytes());
        bytes.extend_from_slice(&element.to_le_bytes());
        bytes.extend_from_slice(vr.code().as_bytes());
        if vr.is_16bit_length() {
            bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
        } else {
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        }
        bytes.extend_from_slice(value);
        bytes
    }

    /// A defined-length item (`FFFE,E000`) holding `body`.
    fn defined_item(body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn a_value_overrunning_its_item_ends_the_item_at_its_declared_end() {
        // The item declares 12 bytes but its only element claims a 100-byte
        // value: the following (0020,000D) belongs to the parent dataset and
        // must still parse instead of being swallowed by the overrun.
        let mut header = Vec::new();
        header.extend_from_slice(&0x0010u16.to_le_bytes());
        header.extend_from_slice(&0x0010u16.to_le_bytes());
        header.extend_from_slice(b"PN");
        header.extend_from_slice(&100u16.to_le_bytes());
        header.extend_from_slice(b"ABCD");
        let mut bytes = explicit_element(0x0008, 0x1111, Vr::Sq, &defined_item(&header));
        bytes.extend_from_slice(&explicit_element(0x0020, 0x000D, Vr::Ui, b"1.2.3 "));
        let ds = read_with(&bytes, DicomReader::default()).expect("the dataset parses");
        assert_eq!(
            ds.try_get_str(Tag::STUDY_INSTANCE_UID).as_deref(),
            Some("1.2.3 "),
            "the element after the overrunning item is not lost"
        );
    }

    #[test]
    fn a_header_truncated_by_the_item_boundary_keeps_the_next_element() {
        // Only the tag fits inside the 6-byte item; its VR does not. The item
        // must end at its declared end, not after the 4 bytes it managed to read.
        let mut body = Vec::new();
        body.extend_from_slice(&0x0010u16.to_le_bytes());
        body.extend_from_slice(&0x0010u16.to_le_bytes());
        body.extend_from_slice(b"PN");
        let mut bytes = explicit_element(0x0008, 0x1111, Vr::Sq, &defined_item(&body));
        bytes.extend_from_slice(&explicit_element(0x0020, 0x000D, Vr::Ui, b"1.2.3 "));
        let ds = read_with(&bytes, DicomReader::default()).expect("the dataset parses");
        assert_eq!(
            ds.try_get_str(Tag::STUDY_INSTANCE_UID).as_deref(),
            Some("1.2.3 "),
            "the parent resumes at the item's declared end"
        );
    }

    #[test]
    fn cp246_in_explicit_vr_le_reads_implicit_inner_content() {
        // IO-B-02: CP-246 applies in Explicit VR LE too, not only Big Endian.
        // The inner element's implicit length (0x4E50) has low bytes that spell
        // the VR "PN"; treating the content as explicit would misread it.
        let vlen = 0x4E50usize;
        let mut inner = Vec::new();
        inner.extend_from_slice(&0x0010u16.to_le_bytes());
        inner.extend_from_slice(&0x0020u16.to_le_bytes());
        inner.extend_from_slice(&(vlen as u32).to_le_bytes());
        inner.extend_from_slice(&vec![b'A'; vlen]);

        let mut bytes = Vec::new();
        // (0008,2112) UN, undefined length, Explicit VR Little Endian header
        bytes.extend_from_slice(&0x0008u16.to_le_bytes());
        bytes.extend_from_slice(&0x2112u16.to_le_bytes());
        bytes.extend_from_slice(b"UN");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // item header/body in little endian
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&inner);
        // sequence delimiter
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE0DDu16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let ds = read_with(&bytes, DicomReader::default()).unwrap();
        let seq = ds
            .get_sequence(Tag::SOURCE_IMAGE_SEQUENCE)
            .expect("CP-246 sequence");
        assert_eq!(seq.len(), 1);
        assert_eq!(
            seq.items()[0].get_str(Tag::PATIENT_ID).unwrap().len(),
            vlen,
            "the implicit inner value was read with its real length"
        );
    }

    #[test]
    fn a_defined_length_non_private_un_is_descended_only_when_item_shaped() {
        let tag = Tag::new(0x0008, 0x1140); // Referenced Image Sequence (dictionary VR = SQ)
        let header = |len: usize| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&0x0008u16.to_le_bytes());
            bytes.extend_from_slice(&0x1140u16.to_le_bytes());
            bytes.extend_from_slice(b"UN");
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(&(len as u32).to_le_bytes());
            bytes
        };

        // content that is not item-shaped: a lone implicit element, no FFFE,E000
        let mut body = Vec::new();
        body.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, 0x02, 0x00, 0x00, 0x00, b'A', b'B']);
        let mut bytes = header(body.len());
        bytes.extend_from_slice(&body);
        let ds = read_with(&bytes, DicomReader::default()).unwrap();
        assert!(
            ds.try_get_sequence(&tag).is_none(),
            "a non-item-shaped defined-length UN stays UN instead of mis-parsing"
        );

        // item-shaped content: one item holding an inner element
        let mut inner = Vec::new();
        inner.extend_from_slice(&0x0010u16.to_le_bytes());
        inner.extend_from_slice(&0x0020u16.to_le_bytes());
        inner.extend_from_slice(&2u32.to_le_bytes());
        inner.extend_from_slice(b"AB");
        let mut item = Vec::new();
        item.extend_from_slice(&0xFFFEu16.to_le_bytes());
        item.extend_from_slice(&0xE000u16.to_le_bytes());
        item.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        item.extend_from_slice(&inner);
        let mut bytes = header(item.len());
        bytes.extend_from_slice(&item);
        let ds = read_with(&bytes, DicomReader::default()).unwrap();
        let seq = ds
            .try_get_sequence(tag)
            .expect("an item-shaped defined-length UN descends as a sequence");
        assert_eq!(seq.len(), 1);
    }

    #[test]
    fn a_nesting_overflow_reports_the_real_position() {
        // 8 nested SQ/item levels; the reader must stop at max_depth
        fn level(depth: usize) -> Vec<u8> {
            let mut out = vec![
                0x08, 0x00, 0x11, 0x11, b'S', b'Q', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
            ];
            out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0xFF, 0xFF, 0xFF, 0xFF]);
            if depth > 1 {
                out.extend_from_slice(&level(depth - 1));
            }
            out.extend_from_slice(&[0xFE, 0xFF, 0x0D, 0xE0, 0, 0, 0, 0]);
            out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
            out
        }
        let bytes = level(8);
        let options = DicomReader {
            max_depth: 4,
            ..DicomReader::default()
        };
        match read_with(&bytes, options) {
            Err(Error::Reader(ReaderError::TooDeep { limit, position })) => {
                assert_eq!(limit, 4);
                assert!(position > 0, "CORE-23: the real offset, not 0");
                assert!(position <= bytes.len() as u64);
            }
            other => panic!("expected TooDeep, got {other:?}"),
        }
    }

    #[test]
    fn implicit_unknown_undefined_length_tag_is_read_as_a_sequence() {
        // CORE-20: implicit VR, unknown non-private tag (0022,0001) with
        // undefined length, one empty item and a sequence delimiter.
        let mut bytes = vec![0xF2, 0x00, 0x01, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        // a trailing PN proves the stream stayed aligned after the sequence
        bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, 0x02, 0x00, 0x00, 0x00, b'A', b'B']);
        let ds = read_implicit(&bytes).unwrap();
        match ds.item(&Tag::new(0x00F2, 0x0001)) {
            Some(crate::model::item::DicomItem::Sequence(sequence)) => {
                assert_eq!(sequence.items().len(), 1, "one empty item");
            }
            other => {
                let tags: Vec<String> = ds.items().map(|i| i.tag().to_string()).collect();
                panic!("expected an anonymous sequence, got {other:?} in {tags:?}")
            }
        }
        assert_eq!(ds.get_str(Tag::PATIENT_NAME).unwrap().trim(), "AB");
    }

    #[test]
    fn truncated_headers_end_the_level_cleanly() {
        // tag + only part of the VR field
        read_with(&[0x10, 0x00, 0x10, 0x00, b'P'], DicomReader::default()).unwrap();
        // VR + reserved bytes, but the length field is short
        read_with(
            &[0x10, 0x00, 0x10, 0x00, b'P', b'N', 0x00, 0x00, 0x02],
            DicomReader::default(),
        )
        .unwrap();
    }

    #[test]
    fn undefined_length_fragment_and_unexpected_tag_are_errors() {
        // An encapsulated (OB) fragment item cannot itself have an undefined
        // length.
        let mut bytes = vec![
            0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(read_with(&bytes, DicomReader::default()).is_err());

        // A fragment sequence may only contain item tags.
        let mut bytes = vec![
            0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, b'P', b'N', 0x02, 0x00, b'X', b' ']);
        assert!(read_with(&bytes, DicomReader::default()).is_err());
    }

    #[test]
    fn explicit_un_with_undefined_length_is_an_implicit_vr_sequence() {
        // PS3.5 §6.2.2 / CP-246: the undefined-length UN value is a sequence
        // whose items are Implicit VR Little Endian, even in an explicit-VR
        // dataset.
        let mut bytes = vec![
            0x44, 0x00, 0x01, 0x10, b'U', b'N', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x0A, 0x00, 0x00, 0x00]);
        // (0010,0010) PN "AB" in implicit VR inside the item
        bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, 0x02, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(b"AB");
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        let ds = read_with(&bytes, DicomReader::default()).unwrap();
        let seq = ds
            .try_get_sequence(Tag::new(0x0044, 0x1001))
            .expect("UN with undefined length is a sequence");
        assert_eq!(seq.items().len(), 1);
        assert_eq!(
            seq.items()[0].get_str(Tag::PATIENT_NAME).unwrap().trim(),
            "AB"
        );
    }

    #[test]
    fn empty_and_skipped_offset_tables_are_tolerated() {
        // encapsulated value with no items: begin/end announced, no fragments
        let mut bytes = vec![
            0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        let ds = read_with(&bytes, DicomReader::default()).unwrap();
        assert_eq!(ds.value_count(Tag::PIXEL_DATA).unwrap(), 0);

        // a basic offset table above the skip threshold is skipped, not read
        let mut bytes = vec![
            0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x08, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        let options = DicomReader {
            large_object_size: 4,
            large_policy: LargeValuePolicy::Skip,
            ..DicomReader::default()
        };
        assert!(read_with(&bytes, options).is_ok());
    }

    #[test]
    fn a_delimiter_with_an_undefined_length_does_not_panic_or_desync() {
        // A sequence delimiter whose length field is 0xFFFFFFFF (written by a
        // broken encoder) must end the fragment sequence: the tag and length are
        // consumed, the bogus value is not a 4 GiB skip, and a following element
        // still parses.
        let mut bytes = vec![
            0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        // empty basic offset table, then one fragment
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x02, 0x00, 0x00, 0x00, 0xAB, 0xCD]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0xFF, 0xFF, 0xFF, 0xFF]);
        // (0010,0010) PN "AB" after the malformed delimiter
        bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, b'P', b'N', 0x02, 0x00, b'A', b'B']);
        let ds = read_with(&bytes, DicomReader::default()).unwrap();
        assert_eq!(ds.value_count(Tag::PIXEL_DATA).unwrap(), 1);
        assert_eq!(
            ds.get_str(Tag::PATIENT_NAME).unwrap().trim(),
            "AB",
            "the dataset stays aligned after the delimiter"
        );
    }

    #[test]
    fn skipped_values_are_absent_instead_of_empty() {
        fn skip() -> DicomReader {
            DicomReader {
                large_object_size: 4,
                large_policy: LargeValuePolicy::Skip,
                ..DicomReader::default()
            }
        }

        // a leaf above the skip threshold is dropped from the dataset, not kept
        // as an empty element (M1; fo-dicom calls no observer for a skipped value)
        let mut bytes = vec![0x10, 0x00, 0x10, 0x00, b'P', b'N', 0x08, 0x00];
        bytes.extend_from_slice(b"ABCDEFGH");
        let ds = read_with(&bytes, skip()).unwrap();
        assert!(
            !ds.contains(&Tag::PATIENT_NAME),
            "a skipped element must not exist in the dataset"
        );

        // an encapsulated value whose offset table and fragment are skipped
        // withholds the whole fragment sequence (fo-dicom never adds a sequence
        // whose fragment buffer is null)
        let mut bytes = vec![
            0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x08, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x08, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        let ds = read_with(&bytes, skip()).unwrap();
        assert!(
            !ds.contains(&Tag::PIXEL_DATA),
            "a skipped fragment sequence must not exist in the dataset"
        );
    }

    #[test]
    fn truncated_fragment_value_keeps_the_readable_prefix() {
        let mut bytes = vec![
            0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
        // fragment claims 8 bytes but only 3 are present
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x08, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(&[1, 2, 3]);
        let options = DicomReader {
            partial_on_eof: true,
            ..DicomReader::default()
        };
        assert!(read_with(&bytes, options).is_ok());
    }

    #[test]
    fn private_undefined_value_without_items_is_skipped_to_the_delimiter() {
        let mut bytes = vec![
            0x09, 0x00, 0x01, 0x10, b'U', b'N', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        bytes.extend_from_slice(b"junk");
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, b'P', b'N', 0x02, 0x00, b'X', b' ']);
        let ds = read_with(&bytes, DicomReader::default()).unwrap();
        assert_eq!(ds.get_str(Tag::PATIENT_NAME).unwrap().trim(), "X");
    }

    #[test]
    fn non_eof_io_errors_map_to_a_typed_reader_error() {
        let mapped = io_err(std::io::Error::other("boom"), 7);
        assert!(matches!(mapped, Error::Reader(ReaderError::Other(_))));
    }

    #[test]
    fn private_sequence_inside_a_big_endian_dataset_stays_big_endian() {
        // IO-03: an explicit big-endian dataset must not have its private SQ
        // subtree forced to little-endian by the T4 probe.
        use crate::io::dataset_builder::parse_dataset_bytes;
        let syntax = crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone();
        // inner element (0029,1010) LO "widget", Big Endian
        let mut inner = Vec::new();
        inner.extend_from_slice(&0x0029u16.to_be_bytes());
        inner.extend_from_slice(&0x1010u16.to_be_bytes());
        inner.extend_from_slice(b"LO");
        inner.extend_from_slice(&6u16.to_be_bytes());
        inner.extend_from_slice(b"widget");
        // item FFFE,E000 with that body, Big Endian
        let mut item = Vec::new();
        item.extend_from_slice(&0xFFFEu16.to_be_bytes());
        item.extend_from_slice(&0xE000u16.to_be_bytes());
        item.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        item.extend_from_slice(&inner);
        // private SQ (0029,1001) defined length, Big Endian
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0029u16.to_be_bytes());
        bytes.extend_from_slice(&0x1001u16.to_be_bytes());
        bytes.extend_from_slice(b"SQ");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&(item.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&item);

        let ds = parse_dataset_bytes(&bytes, &syntax).expect("parse big-endian private SQ");
        let seq = ds
            .try_get_sequence(Tag::new(0x0029, 0x1001))
            .expect("private sequence survives");
        assert_eq!(seq.items().len(), 1);
        assert_eq!(
            seq.items()[0].get_str(Tag::new(0x0029, 0x1010)).unwrap(),
            "widget",
            "inner element decoded with the dataset's big-endian order"
        );
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::error::ReaderError;
    use crate::io::byte_source::ByteSourceReader;
    use crate::io::dataset_builder::DatasetBuilder;
    use crate::model::dataset::Dataset;
    use crate::model::transfer_syntax::syntaxes;
    use std::io::Cursor;

    fn read(bytes: &[u8], options: DicomReader) -> Result<Dataset, Error> {
        let syntax = (*syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).clone();
        let mut source = ByteSourceReader::new(Cursor::new(bytes.to_vec()), syntax.endian());
        let mut builder = DatasetBuilder::with_transfer_syntax(syntax.clone());
        options.read(&mut source, &syntax, &mut builder, None)?;
        Ok(builder.into_dataset())
    }

    /// An observer that inherits the default `on_fragment_skipped` hook.
    struct NullObserver;

    impl ParseObserver for NullObserver {
        fn on_element(
            &mut self,
            _ctx: &ParseContext,
            _tag: &Tag,
            _vr: Vr,
            _buffer: SharedBuffer,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn on_begin_sequence(
            &mut self,
            _ctx: &ParseContext,
            _tag: &Tag,
            _defined_length: Option<u32>,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn on_begin_sequence_item(
            &mut self,
            _ctx: &ParseContext,
            _defined_length: Option<u32>,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn on_end_sequence_item(&mut self, _ctx: &ParseContext) -> Result<(), Error> {
            Ok(())
        }
        fn on_end_sequence(&mut self, _ctx: &ParseContext) -> Result<(), Error> {
            Ok(())
        }
        fn on_begin_fragment(
            &mut self,
            _ctx: &ParseContext,
            _tag: &Tag,
            _vr: Vr,
            _offset_table: Vec<u32>,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn on_fragment(&mut self, _ctx: &ParseContext, _buffer: SharedBuffer) -> Result<(), Error> {
            Ok(())
        }
        fn on_end_fragment(&mut self, _ctx: &ParseContext) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn default_fragment_skipped_hook_is_a_no_op() {
        let mut observer = NullObserver;
        observer
            .on_fragment_skipped(&ParseContext {
                position: 0,
                depth: 0,
            })
            .unwrap();
    }

    #[test]
    fn skipped_truncated_value_ends_the_level_cleanly() {
        // (0010,0010) PN declares 8 bytes but only 2 are present; Skip policy
        // above the 4-byte threshold must end the level instead of erroring.
        let mut bytes = vec![0x10, 0x00, 0x10, 0x00, b'P', b'N', 0x08, 0x00];
        bytes.extend_from_slice(b"AB");
        for partial in [true, false] {
            let options = DicomReader {
                large_object_size: 4,
                large_policy: LargeValuePolicy::Skip,
                partial_on_eof: partial,
                ..DicomReader::default()
            };
            assert!(read(&bytes, options).is_ok(), "partial={partial}");
        }
    }

    #[test]
    fn skipped_truncated_fragment_breaks_the_sequence() {
        let mut bytes = vec![0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0];
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0, 0, 0, 0]); // empty offset table
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 8, 0, 0, 0]);
        bytes.extend_from_slice(&[1, 2]); // only 2 of 8 fragment bytes
        let options = DicomReader {
            large_object_size: 4,
            large_policy: LargeValuePolicy::Skip,
            partial_on_eof: true,
            ..DicomReader::default()
        };
        assert!(read(&bytes, options).is_ok());
    }

    #[test]
    fn a_second_fragment_is_delivered_to_the_observer() {
        let mut bytes = vec![0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0];
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 8, 0, 0, 0]);
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 8, 0, 0, 0]);
        bytes.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        let ds = read(&bytes, DicomReader::default()).unwrap();
        assert_eq!(ds.value_count(Tag::PIXEL_DATA).unwrap(), 2);
    }

    #[test]
    fn a_non_multiple_of_four_offset_table_keeps_whole_entries() {
        let mut bytes = vec![0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0];
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // basic offset table of 5 bytes: one whole u32 entry, one trailing byte
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 5, 0, 0, 0]);
        bytes.extend_from_slice(&[1, 2, 3, 4, 5]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 4, 0, 0, 0]);
        bytes.extend_from_slice(&[9, 9, 9, 9]);
        bytes.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        let ds = read(&bytes, DicomReader::default()).unwrap();
        assert_eq!(ds.value_count(Tag::PIXEL_DATA).unwrap(), 1);
    }

    #[test]
    fn cp246_probe_handles_short_and_non_item_content() {
        // UN undefined-length with only 4 content bytes: the probe's require(8)
        // fails and it declines the explicit reading.
        let mut bytes = vec![0x08, 0x00, 0x12, 0x21, b'U', b'N', 0, 0];
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let _ = read(&bytes, DicomReader::default());

        // UN undefined-length whose content starts with a non-item tag.
        let mut bytes = vec![0x08, 0x00, 0x12, 0x21, b'U', b'N', 0, 0];
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, 0x02, 0x00, 0x00, 0x00, b'A', b'B']);
        let _ = read(&bytes, DicomReader::default());

        // an item header claiming fewer than 8 bytes is not a valid item.
        let mut bytes = vec![0x08, 0x00, 0x12, 0x21, b'U', b'N', 0, 0];
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0, 0, 0, 0]);
        let _ = read(&bytes, DicomReader::default());
    }

    #[test]
    fn inflate_stream_recovers_when_a_zlib_header_does_not_decode() {
        // header bytes that look like zlib (FCHECK passes) but are not a stream
        let bytes = vec![0x78u8, 0x9C, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut source = Cursor::new(bytes);
        assert!(matches!(
            inflate_stream(&mut source, 0),
            Err(Error::Reader(ReaderError::Other(_)))
        ));
    }
}
