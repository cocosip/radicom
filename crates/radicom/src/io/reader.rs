//! Recursive-descent DICOM parser (RD-01..05; docs/05-file-io.md §3).
//!
//! The parser emits observer events; building, DICOMDIR stitching, dumps
//! and JSON all reuse it. Tolerance rows T1–T10 map to the behaviors in
//! docs/05-file-io.md §3.8.
//!
//! Recursion happens only in [`Parser::parse_level`], whose stack frame
//! holds nothing but a [`Step`] value; the per-element work lives in
//! [`Parser::step`], which returns before any nested level starts. That
//! keeps the `max_depth = 256` guard meaningful on the default 2 MiB thread
//! stack (docs/05-file-io.md §3.1).

use crate::error::{Error, ReaderError};
use crate::io::byte_source::ByteSourceReader;
use crate::model::buffer::{EmptyBuffer, EndianBuffer, SharedBuffer};
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
    fn on_element(&mut self, ctx: &ParseContext, tag: &Tag, vr: Vr, buffer: SharedBuffer) -> Result<(), Error>;
    fn on_begin_sequence(&mut self, ctx: &ParseContext, tag: &Tag, defined_length: Option<u32>) -> Result<(), Error>;
    fn on_begin_sequence_item(&mut self, ctx: &ParseContext, defined_length: Option<u32>) -> Result<(), Error>;
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
    Stopped,
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
}

impl Default for DicomReader {
    fn default() -> Self {
        DicomReader { large_object_size: 64 * 1024, max_depth: 256, large_policy: LargeValuePolicy::Materialize }
    }
}

type PrivateCreators = HashMap<(u16, u8), String>;

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
        private_probe: bool,
    },
    /// A sequence item to descend into.
    Item {
        raw: (u16, u16),
        pos: u64,
        defined: Option<u32>,
    },
    /// T5: an item tag at dataset level with no preceding SQ header.
    Orphan { raw: (u16, u16), pos: u64, defined: Option<u32> },
    /// A fragment sequence to scan into the observer.
    Fragments { raw: (u16, u16), tag: Tag, vr: Vr, pos: u64 },
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
    private_creators: &'a mut PrivateCreators,
}

impl<'a, R: Read + Seek> Parser<'a, R> {
    fn ctx(&self, pos: u64) -> ParseContext {
        ParseContext { position: pos, depth: self.depth }
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
        let group = self.source.read_u16().map_err(|e| io_err(e, at))?;
        let element = self.source.read_u16().map_err(|e| io_err(e, at))?;
        Ok((group, element))
    }

    /// Dictionary VR resolution for implicit VRs and fallbacks.
    fn dictionary_vr(&self, group: u16, element: u16) -> Vr {
        if element == 0x0000 {
            return Vr::Ul; // group length is always UL (T9 target)
        }
        let tag = Tag::new(group, element);
        if tag.is_private() {
            return if tag.is_private_creator_slot() { Vr::Lo } else { Vr::Un };
        }
        let entry = crate::model::dictionary::default_dictionary().entry(&tag);
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
    /// explicitly. An item header is `tag(4) + length(4)`; the first element
    /// inside carries a 2-letter VR at offset 12 under explicit encoding,
    /// while an implicit item has the first half of a 32-bit length there.
    fn probe_inner_explicit(&mut self) -> Result<bool, Error> {
        const PROBE: usize = 14;
        let pos = self.position()?;
        if self.source.require(PROBE as u64).is_err() {
            return Ok(self.explicit_vr);
        }
        let mut header = [0u8; PROBE];
        self.source.read_exact(&mut header).map_err(|e| io_err(e, pos))?;
        self.source.goto(pos).map_err(|e| io_err(e, pos))?;
        // item tag in either byte order
        let item_ok = (header[0] == 0xFF && header[1] == 0xFE && header[2] == 0xE0 && header[3] == 0x00)
            || (header[0] == 0xFE && header[1] == 0xFF && header[2] == 0x00 && header[3] == 0xE0);
        if !item_ok {
            return Ok(self.explicit_vr);
        }
        // a real VR code beats "two uppercase letters" as a signal
        Ok(Vr::try_parse(&[header[12], header[13]]).is_some())
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
                LargeValuePolicy::Skip => {
                    self.source.skip(length).map_err(|e| io_err(e, at))?;
                    return Ok(EmptyBuffer::shared());
                }
                LargeValuePolicy::OnDemand => match self.source.window_at_current(length).map_err(|e| io_err(e, at))? {
                    Some(window) => {
                        self.source.skip(length).map_err(|e| io_err(e, at))?;
                        window
                    }
                    None => self.source.read_buffer(length).map_err(|e| io_err(e, at))?,
                },
                LargeValuePolicy::Materialize => self.source.read_buffer(length).map_err(|e| io_err(e, at))?,
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
            if let Some(creator) = self.private_creators.get(&(group, block)) {
                return Tag::private(
                    group,
                    element,
                    Arc::new(crate::model::private_creator::PrivateCreator::new(creator.clone())),
                );
            }
        }
        tag
    }

    /// VR determination (§3.2) with T1/T2 fallbacks; returns the VR and
    /// whether the element turned out explicit.
    fn parse_vr(&mut self, group: u16, element: u16, at: u64) -> Result<(Vr, bool), Error> {
        if self.explicit_vr {
            let pos = self.position()?;
            let mut code = [0u8; 2];
            self.source.read_exact(&mut code).map_err(|e| io_err(e, at))?;
            match Vr::try_parse(&code) {
                Some(vr) => return Ok((vr, true)),
                None => {
                    if code == [0x20, 0x20] {
                        // T1: two spaces — dictionary VR, stay explicit
                        return Ok((self.dictionary_vr(group, element), true));
                    }
                    // T2: rewind the two VR bytes and read this element
                    // implicitly (those bytes are the head of a 32-bit length)
                    self.source.goto(pos).map_err(|e| io_err(e, at))?;
                }
            }
        }
        Ok((self.dictionary_vr(group, element), false))
    }

    /// Length field per VR/syntax (§3.3).
    fn parse_length(&mut self, vr: Vr, explicit: bool, at: u64) -> Result<u32, Error> {
        if explicit && vr.is_16bit_length() {
            let v = self.source.read_u16().map_err(|e| io_err(e, at))?;
            Ok(v as u32)
        } else {
            if explicit {
                self.source.read_u16().map_err(|e| io_err(e, at))?; // reserved
            }
            self.source.read_u32().map_err(|e| io_err(e, at))
        }
    }

    /// Enters one nesting level; enforces the depth cap.
    fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > self.options.max_depth {
            return Err(Error::Reader(ReaderError::TooDeep { limit: self.options.max_depth, position: 0 }));
        }
        Ok(())
    }

    /// Switches parser and source byte order together.
    fn set_byte_order(&mut self, big_endian: bool) {
        self.big_endian = big_endian;
        self.source.endian = if big_endian { Endian::Big } else { Endian::Little };
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

        if group == 0xFFFE {
            let length = self.source.read_u32().map_err(|e| io_err(e, tag_pos))?;
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
                        Step::Orphan { raw, pos: value_start, defined }
                    } else {
                        Step::Item { raw, pos: value_start, defined }
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

        let (raw_vr, explicit) = match self.parse_vr(group, element, tag_pos) {
            Ok(v) => v,
            Err(Error::Reader(ReaderError::Eof { .. })) => return Ok(Step::End(LevelEnd::Eof)),
            Err(e) => return Err(e),
        };
        let value_start = self.position()?;
        // The length field is laid out per the VR that was *written*: T9
        // rewrites UN to UL only after this, since UN uses the long form.
        let length = match self.parse_length(raw_vr, explicit, tag_pos) {
            Ok(l) => l,
            Err(Error::Reader(ReaderError::Eof { .. })) => return Ok(Step::End(LevelEnd::Eof)),
            Err(e) => return Err(e),
        };
        let vr = self.fix_group_length_vr(element, raw_vr);
        let defined = (length != 0xFFFF_FFFF).then_some(length);
        let raw = (group, element);

        // Sequence detection (§3.4): an explicit SQ; or an undefined-length
        // value that must be read as SQ (T3 UN-encoded SQ recovery, CP-246).
        // Undefined-length OB/OW is a fragment sequence; an undefined-length
        // private value goes to the T4 probe path.
        let is_sequence = vr == Vr::Sq
            || (defined.is_none()
                && !tag.is_private()
                && (vr == Vr::Un || self.dictionary_vr(group, element) == Vr::Sq));
        if is_sequence {
            return Ok(Step::Sequence { raw, tag, pos: value_start, defined, private_probe: false });
        }

        let Some(length) = defined else {
            if tag.is_private() {
                // T4: private undefined-length value; the probe decides
                // whether the subtree is encoded explicitly
                return Ok(Step::Sequence { raw, tag, pos: value_start, defined: None, private_probe: true });
            }
            return Ok(Step::Fragments { raw, tag, vr, pos: value_start });
        };

        // A truncated *value* is a real error (its declared length is
        // known); only truncated headers end a level gracefully (T8).
        let buffer = self.read_value_buffer(vr, length as u64, value_start)?;
        // RD-05: private creator tracking
        if tag.is_private() && tag.is_private_creator_slot() && vr == Vr::Lo {
            let creator = String::from_utf8_lossy(&buffer.data()).trim_end_matches([' ', '\0']).to_string();
            self.private_creators.insert((group, (element & 0xFF) as u8), creator);
        }
        Ok(Step::Element { raw, tag, vr, pos: tag_pos, buffer })
    }

    /// Fragment sequences (§3.4): fragment 0 is the basic offset table in
    /// 4-byte units; the terminator is `FFFE,E0DD`; EOF is tolerated (T6).
    fn parse_fragment_sequence(
        &mut self,
        observer: &mut impl ParseObserver,
        tag: &Tag,
        vr: Vr,
        start: u64,
    ) -> Result<(), Error> {
        let mut offset_table: Option<Vec<u32>> = None;
        loop {
            if self.source.require(4).is_err() {
                break; // T6: missing final delimiter at EOF
            }
            let pos = self.position()?;
            let (g, e) = self.read_tag(pos)?;
            match (g, e) {
                (0xFFFE, 0xE000) => {
                    let length = self.source.read_u32().map_err(|e| io_err(e, pos))?;
                    if length == 0xFFFF_FFFF {
                        return Err(Error::Reader(ReaderError::Invalid {
                            message: "fragment with undefined length".into(),
                            position: pos,
                        }));
                    }
                    let buffer = self.read_value_buffer(vr, length as u64, pos)?;
                    match offset_table {
                        // fragment 0 is the basic offset table, never data
                        None => {
                            let table = decode_offset_table(&buffer, self.big_endian);
                            observer.on_begin_fragment(&self.ctx(start), tag, vr, table.clone())?;
                            offset_table = Some(table);
                        }
                        Some(_) => observer.on_fragment(&self.ctx(pos), buffer)?,
                    }
                }
                (0xFFFE, 0xE0DD) => break,
                _ => {
                    return Err(Error::Reader(ReaderError::Invalid {
                        message: format!("unexpected tag in DICOM fragment sequence {g:04X},{e:04X}"),
                        position: pos,
                    }))
                }
            }
        }
        // an empty encapsulated value still announces begin/end so the
        // element exists in the built dataset
        if offset_table.is_none() {
            observer.on_begin_fragment(&self.ctx(start), tag, vr, Vec::new())?;
        }
        observer.on_end_fragment(&self.here()?)?;
        Ok(())
    }
}

fn io_err(e: std::io::Error, position: u64) -> Error {
    match e.kind() {
        std::io::ErrorKind::UnexpectedEof => Error::Reader(ReaderError::Eof { position, needed: 0 }),
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
            private_creators: &mut private_creators,
        };
        parser.parse_level(observer, stop, Mode::Top, 0)?;
        Ok(if parser.stopped { ReadOutcome::Stopped } else { ReadOutcome::Success })
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
        loop {
            match self.step(stop, mode, level_start, previous)? {
                Step::Continue => {}
                Step::End(end) => return Ok(end),
                Step::Stopped => {
                    self.stopped = true;
                    return Ok(LevelEnd::Eof);
                }
                Step::Element { raw, tag, vr, pos, buffer } => {
                    observer.on_element(&self.ctx(pos), &tag, vr, buffer)?;
                    previous = Some(raw);
                }
                Step::Fragments { raw, tag, vr, pos } => {
                    self.parse_fragment_sequence(observer, &tag, vr, pos)?;
                    previous = Some(raw);
                }
                Step::Sequence { raw, tag, pos, defined, private_probe } => {
                    observer.on_begin_sequence(&self.ctx(pos), &tag, defined)?;
                    let (saved_explicit, saved_be) = (self.explicit_vr, self.big_endian);
                    if private_probe {
                        self.explicit_vr = self.probe_inner_explicit()?;
                        if self.explicit_vr {
                            self.set_byte_order(false);
                        }
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
                        return Ok(LevelEnd::Eof);
                    }
                    previous = Some(raw);
                }
                Step::Item { raw, pos, defined } => {
                    let end = self.parse_item(observer, pos, defined)?;
                    // T7: the enclosing SQ may end where an item was expected
                    if matches!(end, LevelEnd::SequenceEnd | LevelEnd::Eof) {
                        return Ok(end);
                    }
                    previous = Some(raw);
                }
                Step::Orphan { raw, pos, defined } => {
                    // T5: an item tag at dataset level with no SQ header is an
                    // anonymous undefined-length sequence; its first item was
                    // already consumed by `step`
                    let unknown = Tag::UNKNOWN;
                    observer.on_begin_sequence(&self.ctx(pos), &unknown, None)?;
                    self.enter()?;
                    let end = self.parse_item(observer, pos, defined)?;
                    let end = match (end, defined) {
                        (LevelEnd::Eof, _) | (_, Some(_)) => end,
                        // further sibling items and the sequence delimiter
                        // belong to this anonymous sequence
                        (_, None) => self.parse_level(observer, None, Mode::SequenceDelimiter, pos)?,
                    };
                    self.depth -= 1;
                    observer.on_end_sequence(&self.here()?)?;
                    if end == LevelEnd::Eof {
                        return Ok(LevelEnd::Eof);
                    }
                    previous = Some(raw);
                }
            }
        }
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


/// Basic offset table entries follow the transfer syntax byte order.
fn decode_offset_table(buffer: &SharedBuffer, big_endian: bool) -> Vec<u32> {
    let data = buffer.data();
    data.chunks_exact(4)
        .map(|c| {
            let raw = [c[0], c[1], c[2], c[3]];
            if big_endian {
                u32::from_be_bytes(raw)
            } else {
                u32::from_le_bytes(raw)
            }
        })
        .collect()
}

/// RD-04: inflates a deflated dataset body. The file meta group is never
/// deflated, so this applies to the dataset bytes only.
///
/// PS3.5 A.5: the deflated transfer syntax uses **raw DEFLATE (RFC 1951)** — no
/// zlib header/trailer. Some non-conformant writers wrap it in zlib anyway, so
/// try the zlib decoder first (it errors cleanly on raw deflate) and fall back
/// to the raw decoder. The reverse order would let the raw decoder silently
/// mis-decode a zlib-wrapped stream.
pub fn inflate(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    use std::io::Read as _;
    let mut out = Vec::new();
    if flate2::read::ZlibDecoder::new(bytes).read_to_end(&mut out).is_ok() {
        return Ok(out);
    }
    out.clear();
    flate2::read::DeflateDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| Error::Reader(ReaderError::Other(format!("deflate: {e}"))))?;
    Ok(out)
}
