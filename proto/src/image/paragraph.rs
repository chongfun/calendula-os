//! Keeping a chapter's images in their paragraphs, ahead of the content
//! stream.
//!
//! The parser hands an image over the moment it meets its tag, but where
//! the image goes depends on what follows: an image at a paragraph's start
//! goes inside the paragraph's first line when text follows it in the same
//! paragraph, and stands alone as a block when the paragraph ends first; a
//! drop cap floated in a block of its own joins the paragraph after it.
//! [`ImageParagraphSink`] sits between the parser and the sink that
//! captures the content stream, resolves each image against the book's
//! image index through [`ImageLookup`], holds the images met at a
//! paragraph's start until the paragraph shows whether text follows, and
//! pushes each as what it turned out to be: a block
//! ([`XhtmlBlockSink::push_image_block`]), an image inside its line
//! ([`XhtmlBlockSink::push_inline_image`]), or the alt text that stands in
//! for one that cannot be shown.
//!
//! Everything it pushes carries the stream offset the parser gave the
//! image when it was met, so the content stream's coordinate space is the
//! parser's whatever the images became. An image shown as itself takes one
//! byte of the stream, as `proto::anchor` provides for; its alt text, when
//! that stands in, takes its length and one like any block.
//!
//! Nothing here keeps static memory: the held records live in a scratch
//! slice the caller lends (on device, the idle tail of the container
//! scratch), each a [`PENDING_HEAD_BYTES`] head and the text it needs.

use crate::epub::{alt_is_initial, image_alt_text, XhtmlBlockSink, XhtmlError};
use crate::image::token::{ContentImage, IMAGE_ALT_MAX_BYTES, IMAGE_TOKEN_MARK};
use crate::text::{FontStyle, TextAlign, TextRole};

/// What the book's image index says of an image a chapter names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// An image the decoder takes, and whether it may go inside a line
    /// (its fitted block height is within the inline bound in every
    /// orientation; the sink that knows the page boxes decides).
    Shown { image: ContentImage, inline: bool },
    /// Nothing to show: its alt text stands in.
    Missing,
    /// The index cannot be read: the chapter stops here.
    Stop,
}

/// Resolves an image's `src` (raw, relative to the chapter) against the
/// book's image index.
pub trait ImageLookup {
    fn look_up(&mut self, src: &str) -> Lookup;
    /// The sink put `image` inside a line, where it shows at its inline
    /// size.
    fn shown_inline(&mut self, image: ContentImage) {
        let _ = image;
    }
}

/// Bytes of a held record before its text.
pub const PENDING_HEAD_BYTES: usize = 16;
/// Most records held back at once; past it (or past the scratch), the
/// images held go out as the blocks they would be if no text followed.
pub const MAX_PENDING_IMAGES: u8 = 32;
/// Smallest scratch that holds one image with the longest alt.
pub const MIN_PENDING_SCRATCH: usize = PENDING_HEAD_BYTES + IMAGE_ALT_MAX_BYTES;

/// What a held record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// An image the decoder takes; its text is its raw alt.
    Shown(ContentImage),
    /// An image that cannot be shown; its text is the alt text that stands
    /// in for it, cleaned.
    Missing,
    /// A run of spacing met among the images, kept so it is laid out in
    /// its place either way.
    Spacing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingImage {
    held: Held,
    /// The source had a space before it.
    leading_space: bool,
    /// A drop cap: alone in a block that floats left, with a single letter
    /// for its alt. It may join the paragraph after its own.
    drop_cap: bool,
    /// Where it sits in the content stream.
    offset: u32,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
}

/// Records held back at a paragraph's start, in borrowed scratch.
struct PendingImages<'a> {
    buf: &'a mut [u8],
    len: usize,
    count: u8,
    /// Records held since the last paragraph end.
    fresh: u8,
    /// The records were carried over from a floated block ahead of the
    /// paragraph in hand.
    carried: bool,
}

impl<'a> PendingImages<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            count: 0,
            fresh: 0,
            carried: false,
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Holds `record` with the text `write` puts in the room it is handed
    /// (its length, or `None` when it does not fit whole). False, holding
    /// nothing, when there is no room for both.
    fn push(
        &mut self,
        record: PendingImage,
        write: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) -> bool {
        if self.count >= MAX_PENDING_IMAGES {
            return false;
        }
        let Some(room) = self.buf.get_mut(self.len..) else {
            return false;
        };
        let Some((head, text)) = room.split_at_mut_checked(PENDING_HEAD_BYTES) else {
            return false;
        };
        let Some(text_len) = write(text).and_then(|len| u16::try_from(len).ok()) else {
            return false;
        };
        let (kind, image) = match record.held {
            Held::Missing => (0, ContentImage::default()),
            Held::Shown(image) => (1, image),
            Held::Spacing => (2, ContentImage::default()),
        };
        head[0] = kind;
        head[1] = u8::from(record.leading_space) | u8::from(record.drop_cap) << 1;
        head[2..4].copy_from_slice(&image.slot.to_le_bytes());
        head[4..6].copy_from_slice(&image.width.to_le_bytes());
        head[6..8].copy_from_slice(&image.height.to_le_bytes());
        head[8..12].copy_from_slice(&record.offset.to_le_bytes());
        head[12..14].copy_from_slice(&text_len.to_le_bytes());
        head[14] = role_code(record.role) | style_code(record.style) << 4;
        head[15] = align_code(record.align);
        self.len += PENDING_HEAD_BYTES + usize::from(text_len);
        self.count += 1;
        self.fresh += 1;
        true
    }

    /// The held records in order, each with its text.
    fn iter(&self) -> impl Iterator<Item = (PendingImage, &str)> + '_ {
        let mut at = 0usize;
        (0..self.count).map_while(move |_| {
            let head = self.buf.get(at..at + PENDING_HEAD_BYTES)?;
            let u16_at = |index: usize| u16::from_le_bytes([head[index], head[index + 1]]);
            let text_at = at + PENDING_HEAD_BYTES;
            let text = self.buf.get(text_at..text_at + usize::from(u16_at(12)))?;
            at = text_at + text.len();
            let image = ContentImage {
                slot: u16_at(2),
                width: u16_at(4),
                height: u16_at(6),
            };
            let held = match head[0] {
                1 => Held::Shown(image),
                2 => Held::Spacing,
                _ => Held::Missing,
            };
            let record = PendingImage {
                held,
                leading_space: head[1] & 1 != 0,
                drop_cap: head[1] & 2 != 0,
                offset: u32::from_le_bytes([head[8], head[9], head[10], head[11]]),
                role: role_from_code(head[14] & 0xf),
                style: style_from_code(head[14] >> 4),
                align: align_from_code(head[15]),
            };
            Some((record, core::str::from_utf8(text).unwrap_or("")))
        })
    }

    fn clear(&mut self) {
        self.len = 0;
        self.count = 0;
        self.fresh = 0;
        self.carried = false;
    }

    /// A lone drop cap held in the paragraph just ended, which carries
    /// into the next.
    fn holds_lone_drop_cap(&self) -> bool {
        self.count == 1
            && self.fresh == 1
            && !self.carried
            && self.iter().all(|(record, _)| record.drop_cap)
    }
}

fn role_code(role: TextRole) -> u8 {
    match role {
        TextRole::Body => 0,
        TextRole::Heading1 => 1,
        TextRole::Heading2 => 2,
        TextRole::Heading3 => 3,
        TextRole::BlockQuote => 4,
    }
}

fn role_from_code(code: u8) -> TextRole {
    match code {
        1 => TextRole::Heading1,
        2 => TextRole::Heading2,
        3 => TextRole::Heading3,
        4 => TextRole::BlockQuote,
        _ => TextRole::Body,
    }
}

fn style_code(style: FontStyle) -> u8 {
    match style {
        FontStyle::Regular => 0,
        FontStyle::Italic => 1,
        FontStyle::Bold => 2,
        FontStyle::BoldItalic => 3,
    }
}

fn style_from_code(code: u8) -> FontStyle {
    match code {
        1 => FontStyle::Italic,
        2 => FontStyle::Bold,
        3 => FontStyle::BoldItalic,
        _ => FontStyle::Regular,
    }
}

fn align_code(align: TextAlign) -> u8 {
    match align {
        TextAlign::Left => 0,
        TextAlign::Center => 1,
        TextAlign::Justify => 2,
    }
}

fn align_from_code(code: u8) -> TextAlign {
    match code {
        0 => TextAlign::Left,
        1 => TextAlign::Center,
        _ => TextAlign::Justify,
    }
}

/// Whether a run of text holds anything but whitespace: an image in the
/// same paragraph then goes inside its line.
fn has_visible_text(text: &str) -> bool {
    text.chars()
        .any(|ch| !ch.is_whitespace() && !is_invisible_format_char(ch) && ch != IMAGE_TOKEN_MARK)
}

/// Whether a run's first visible character is a letter, as the rest of a
/// drop cap's word is.
fn starts_with_letter(text: &str) -> bool {
    text.chars()
        .find(|&ch| !ch.is_whitespace() && !is_invisible_format_char(ch))
        .is_some_and(char::is_alphabetic)
}

/// Characters with no glyph and no width: soft hyphen, zero-width
/// space/joiners, directional marks, word joiner, byte-order mark.
fn is_invisible_format_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}'
    )
}

fn ends_with_space(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|ch| ch.is_ascii_whitespace())
}

/// The longest prefix of `alt` within [`IMAGE_ALT_MAX_BYTES`], cut on a
/// character boundary, with control characters dropped: what a block
/// image's content token keeps of its raw alt.
fn raw_alt_into(alt: Option<&str>, out: &mut [u8]) -> Option<usize> {
    let mut len = 0usize;
    for ch in alt.unwrap_or("").chars() {
        if ch.is_control() {
            continue;
        }
        if len + ch.len_utf8() > IMAGE_ALT_MAX_BYTES {
            break;
        }
        let room = out.get_mut(len..len + ch.len_utf8())?;
        len += ch.encode_utf8(room).len();
    }
    Some(len)
}

/// The sink between the parser and a sink that takes resolved images.
pub struct ImageParagraphSink<'a, S: XhtmlBlockSink + ?Sized> {
    inner: &'a mut S,
    lookup: &'a mut dyn ImageLookup,
    pending: PendingImages<'a>,
    /// The paragraph in hand holds text, not just whitespace: an image met
    /// now goes inside its line.
    paragraph_has_text: bool,
    /// The last run pushed ended in a space, which the next image takes.
    trailing_space: bool,
    /// The paragraph in hand's role, style and alignment, from its last
    /// run, for images that go inside its lines.
    run: (TextRole, FontStyle, TextAlign),
}

impl<'a, S: XhtmlBlockSink + ?Sized> ImageParagraphSink<'a, S> {
    /// A sink over `inner`, resolving images through `lookup`, holding
    /// records in `scratch` (at least [`MIN_PENDING_SCRATCH`] for any image
    /// to be held; with less, every image at a paragraph's start is a
    /// block).
    pub fn new(inner: &'a mut S, lookup: &'a mut dyn ImageLookup, scratch: &'a mut [u8]) -> Self {
        Self {
            inner,
            lookup,
            pending: PendingImages::new(scratch),
            paragraph_has_text: false,
            trailing_space: false,
            run: (TextRole::Body, FontStyle::Regular, TextAlign::Justify),
        }
    }

    /// Whether images met at a paragraph's start are still held, waiting
    /// for the paragraph to show whether text follows. A chapter ends with
    /// none: the parser ends its last paragraph.
    pub fn holds_images(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Ends the chapter: anything still held goes out as blocks.
    pub fn finish(&mut self) -> Result<(), XhtmlError> {
        if !self.pending.is_empty() {
            self.commit_pending_blocks()?;
        }
        Ok(())
    }

    fn forward_text(
        &mut self,
        text: &str,
        role: TextRole,
        style: FontStyle,
        align: TextAlign,
        paragraph_end: bool,
        offset: u32,
    ) -> Result<(), XhtmlError> {
        self.inner
            .push_block(text, role, style, align, paragraph_end, offset)?;
        if has_visible_text(text) {
            self.paragraph_has_text = true;
            self.run = (role, style, align);
        }
        self.trailing_space =
            !text.is_empty() && (ends_with_space(text) || !has_visible_text(text));
        if paragraph_end {
            self.paragraph_has_text = false;
            self.trailing_space = false;
        }
        Ok(())
    }

    /// A run of nothing but spacing among held images: held in order with
    /// them. False when there is no room.
    fn hold_spacing(
        &mut self,
        text: &str,
        role: TextRole,
        style: FontStyle,
        align: TextAlign,
        offset: u32,
    ) -> bool {
        let record = PendingImage {
            held: Held::Spacing,
            leading_space: false,
            drop_cap: false,
            offset,
            role,
            style,
            align,
        };
        self.pending.push(record, |out| {
            let room = out.get_mut(..text.len())?;
            room.copy_from_slice(text.as_bytes());
            Some(text.len())
        })
    }

    /// An `<img>`: inside its line when its paragraph already has text,
    /// otherwise held back until the paragraph shows whether text follows.
    /// `floated` says its block floats left. Answers with the stream bytes
    /// the image takes.
    fn push_image_in_paragraph(
        &mut self,
        src: &str,
        alt: Option<&str>,
        floated: bool,
        offset: u32,
    ) -> Result<u32, XhtmlError> {
        let shown = match self.lookup.look_up(src) {
            Lookup::Stop => return Err(XhtmlError::TooManyRuns),
            Lookup::Missing => None,
            Lookup::Shown {
                image,
                inline: false,
            } => return self.push_illustration(image, alt, offset),
            Lookup::Shown {
                image,
                inline: true,
            } => Some(image),
        };
        if self.pending.carried {
            // An image, not text, follows the drop cap carried ahead: it
            // stays the block it was.
            self.commit_pending_blocks()?;
        }
        let (role, style, align) = self.run;
        if self.paragraph_has_text {
            // Its paragraph has text: inside the line at once, nothing to
            // hold. The alt text of one that cannot be shown stands in its
            // place, in italics.
            let leading_space = core::mem::take(&mut self.trailing_space);
            return match shown {
                Some(image) => {
                    self.inner.push_inline_image(
                        image,
                        leading_space,
                        false,
                        role,
                        style,
                        align,
                        offset,
                    )?;
                    self.lookup.shown_inline(image);
                    Ok(1)
                }
                None => {
                    let mut text = [0u8; IMAGE_ALT_MAX_BYTES];
                    let len = image_alt_text(alt, &mut text);
                    let text = core::str::from_utf8(&text[..len]).unwrap_or("");
                    let trimmed = text.trim_matches(|ch: char| ch.is_ascii_whitespace());
                    if !trimmed.is_empty() {
                        self.inner.push_block(
                            trimmed,
                            role,
                            FontStyle::Italic,
                            align,
                            false,
                            offset,
                        )?;
                    }
                    Ok(crate::anchor::block_stream_len(text.len()))
                }
            };
        }
        let record = PendingImage {
            held: shown.map_or(Held::Missing, Held::Shown),
            leading_space: core::mem::take(&mut self.trailing_space),
            drop_cap: floated && alt.is_some_and(alt_is_initial),
            offset,
            role,
            style,
            align,
        };
        // What the record keeps: a shown image's raw alt, for its block
        // token should it become one; the placeholder text of one that
        // cannot be shown.
        let write = |out: &mut [u8]| match shown {
            Some(_) => raw_alt_into(alt, out),
            None => Some(image_alt_text(alt, out.get_mut(..IMAGE_ALT_MAX_BYTES)?)),
        };
        let mut taken = 1;
        if shown.is_none() {
            let mut text = [0u8; IMAGE_ALT_MAX_BYTES];
            taken = crate::anchor::block_stream_len(image_alt_text(alt, &mut text));
        }
        if !self.pending.push(record, write) {
            // A long run of images before any text: those held go out as
            // blocks, as they would if the paragraph ended here.
            self.commit_pending_blocks()?;
            if !self.pending.push(record, write) {
                // No room even then: the block it would be.
                return match shown {
                    Some(image) => {
                        let mut raw = [0u8; IMAGE_ALT_MAX_BYTES];
                        let len = raw_alt_into(alt, &mut raw).unwrap_or(0);
                        let alt = core::str::from_utf8(&raw[..len]).unwrap_or("");
                        self.inner.push_image_block(image, alt, offset)?;
                        Ok(1)
                    }
                    None => self.push_missing_block(alt, offset),
                };
            }
        }
        Ok(taken)
    }

    /// The alt-text placeholder of an image that cannot be shown, alone.
    fn push_missing_block(&mut self, alt: Option<&str>, offset: u32) -> Result<u32, XhtmlError> {
        let mut text = [0u8; IMAGE_ALT_MAX_BYTES];
        let len = image_alt_text(alt, &mut text);
        let text = core::str::from_utf8(&text[..len]).unwrap_or("");
        self.inner.push_block(
            text,
            TextRole::Body,
            FontStyle::Italic,
            TextAlign::Center,
            true,
            offset,
        )?;
        self.paragraph_has_text = false;
        self.trailing_space = false;
        Ok(crate::anchor::block_stream_len(text.len()))
    }

    /// An illustration, too tall to go inside a line: a block, ending the
    /// paragraph around it even when that has text. Images held at the
    /// paragraph's start go out first, as blocks; text after it starts a
    /// paragraph of its own.
    fn push_illustration(
        &mut self,
        image: ContentImage,
        alt: Option<&str>,
        offset: u32,
    ) -> Result<u32, XhtmlError> {
        if !self.pending.is_empty() {
            self.commit_pending_blocks()?;
        }
        let mut raw = [0u8; IMAGE_ALT_MAX_BYTES];
        let len = raw_alt_into(alt, &mut raw).unwrap_or(0);
        let alt = core::str::from_utf8(&raw[..len]).unwrap_or("");
        self.inner.push_image_block(image, alt, offset)?;
        self.paragraph_has_text = false;
        self.trailing_space = false;
        Ok(1)
    }

    /// Places the records held back inside the line, in order: each image
    /// at its inline size, in place of each that cannot be shown its alt
    /// text in italics, and the spacing among them. `role`, `style` and
    /// `align` are the paragraph's.
    fn place_pending_inline(
        &mut self,
        role: TextRole,
        style: FontStyle,
        align: TextAlign,
    ) -> Result<(), XhtmlError> {
        let carried = self.pending.carried;
        let mut pending = core::mem::replace(&mut self.pending, PendingImages::new(&mut []));
        let mut result = Ok(());
        for (record, text) in pending.iter() {
            result = match record.held {
                Held::Shown(image) => self
                    .inner
                    .push_inline_image(
                        image,
                        record.leading_space && !carried,
                        carried && record.drop_cap,
                        role,
                        style,
                        align,
                        record.offset,
                    )
                    .map(|()| self.lookup.shown_inline(image)),
                Held::Missing => {
                    let text = text.trim_matches(|ch: char| ch.is_ascii_whitespace());
                    if text.is_empty() {
                        Ok(())
                    } else {
                        self.inner.push_block(
                            text,
                            role,
                            FontStyle::Italic,
                            align,
                            false,
                            record.offset,
                        )
                    }
                }
                Held::Spacing => self.inner.push_block(
                    text,
                    record.role,
                    record.style,
                    record.align,
                    false,
                    record.offset,
                ),
            };
            if result.is_err() {
                break;
            }
        }
        pending.clear();
        self.pending = pending;
        self.trailing_space = false;
        result
    }

    /// Sends the records held back out as what they are alone in a
    /// paragraph: image blocks, alt-text placeholders for images that
    /// cannot be shown, and their spacing as the lines it made.
    fn commit_pending_blocks(&mut self) -> Result<(), XhtmlError> {
        let mut pending = core::mem::replace(&mut self.pending, PendingImages::new(&mut []));
        let mut result = Ok(());
        for (record, text) in pending.iter() {
            result = match record.held {
                Held::Shown(image) => self.inner.push_image_block(image, text, record.offset),
                Held::Missing => self.inner.push_block(
                    text,
                    TextRole::Body,
                    FontStyle::Italic,
                    TextAlign::Center,
                    true,
                    record.offset,
                ),
                Held::Spacing => self.inner.push_block(
                    text,
                    record.role,
                    record.style,
                    record.align,
                    true,
                    record.offset,
                ),
            };
            if result.is_err() {
                break;
            }
        }
        pending.clear();
        self.pending = pending;
        self.paragraph_has_text = false;
        self.trailing_space = false;
        result
    }

    /// A paragraph that held images ended without text after them. A lone
    /// drop cap carries into the next paragraph (it stays held through the
    /// empty ones between); anything else goes out as blocks.
    fn end_pending_paragraph(&mut self) -> Result<(), XhtmlError> {
        if self.pending.fresh == 0 && self.pending.carried {
            return Ok(());
        }
        if self.pending.holds_lone_drop_cap() {
            self.pending.carried = true;
            self.pending.fresh = 0;
            return Ok(());
        }
        self.commit_pending_blocks()
    }
}

impl<S: XhtmlBlockSink + ?Sized> XhtmlBlockSink for ImageParagraphSink<'_, S> {
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: FontStyle,
        align: TextAlign,
        paragraph_end: bool,
        logical_offset: u32,
    ) -> Result<(), XhtmlError> {
        if self.pending.is_empty() {
            return self.forward_text(text, role, style, align, paragraph_end, logical_offset);
        }
        if !has_visible_text(text) {
            if paragraph_end {
                self.end_pending_paragraph()?;
            } else if text.is_empty() || self.hold_spacing(text, role, style, align, logical_offset)
            {
                return Ok(());
            } else {
                self.commit_pending_blocks()?;
            }
            return self.forward_text(text, role, style, align, paragraph_end, logical_offset);
        }
        // Text follows the held images in their paragraph: they go inside
        // its first line, ahead of it. A drop cap carried from its own block
        // is the first letter of a word of body text: before a heading, a
        // centred line or anything that does not start with a letter, it
        // stays a block.
        let joins = !self.pending.carried
            || (role == TextRole::Body && align != TextAlign::Center && starts_with_letter(text));
        if joins {
            self.place_pending_inline(role, style, align)?;
        } else {
            self.commit_pending_blocks()?;
        }
        self.forward_text(text, role, style, align, paragraph_end, logical_offset)
    }

    fn push_image(
        &mut self,
        src: &str,
        alt: Option<&str>,
        logical_offset: u32,
    ) -> Result<u32, XhtmlError> {
        self.push_image_in_paragraph(src, alt, false, logical_offset)
    }

    fn push_floated_image(
        &mut self,
        src: &str,
        alt: Option<&str>,
        logical_offset: u32,
    ) -> Result<u32, XhtmlError> {
        self.push_image_in_paragraph(src, alt, true, logical_offset)
    }

    fn wants_inline_images(&self) -> bool {
        true
    }

    fn push_svg_image(&mut self, href: &str, logical_offset: u32) -> Result<u32, XhtmlError> {
        if !self.pending.is_empty() {
            // Only a drop cap carried ahead is still held: the parser ended
            // the paragraph before the svg. An image follows it, so it
            // stays the block it was.
            self.commit_pending_blocks()?;
        }
        match self.lookup.look_up(href) {
            Lookup::Stop => Err(XhtmlError::TooManyRuns),
            Lookup::Shown { image, .. } => {
                self.inner.push_image_block(image, "", logical_offset)?;
                self.paragraph_has_text = false;
                self.trailing_space = false;
                Ok(1)
            }
            // An svg image never had a placeholder.
            Lookup::Missing => Ok(0),
        }
    }

    fn wants_svg_images(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::epub::{xhtml_blocks_to_sink, StreamingXmlTokenizer, XhtmlBlockStreamParser};
    use std::format;
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    /// Everything the sink below the adapter hears, one line per call.
    #[derive(Default)]
    struct Log(Vec<String>);

    impl XhtmlBlockSink for Log {
        fn push_block(
            &mut self,
            text: &str,
            role: TextRole,
            style: FontStyle,
            align: TextAlign,
            paragraph_end: bool,
            offset: u32,
        ) -> Result<(), XhtmlError> {
            self.0.push(format!(
                "text {text:?} {role:?} {style:?} {align:?} end={paragraph_end} @{offset}"
            ));
            Ok(())
        }

        fn push_image_block(
            &mut self,
            image: ContentImage,
            alt: &str,
            offset: u32,
        ) -> Result<(), XhtmlError> {
            self.0.push(format!(
                "block slot={} {}x{} alt={alt:?} @{offset}",
                image.slot, image.width, image.height
            ));
            Ok(())
        }

        fn push_inline_image(
            &mut self,
            image: ContentImage,
            leading_space: bool,
            drop_cap: bool,
            role: TextRole,
            style: FontStyle,
            align: TextAlign,
            offset: u32,
        ) -> Result<(), XhtmlError> {
            self.0.push(format!(
                "inline slot={} {}x{} space={leading_space} cap={drop_cap} {role:?} {style:?} {align:?} @{offset}",
                image.slot, image.width, image.height
            ));
            Ok(())
        }
    }

    /// Images by their file name: `t12x.png` is a 17 x 12 symbol, `t.png`
    /// a 75 x 75 initial, `plate.png` a 640 x 900 illustration, `gone.png`
    /// missing from the index, `stop.png` an index that cannot be read.
    struct Index;

    impl ImageLookup for Index {
        fn look_up(&mut self, src: &str) -> Lookup {
            let file = src.rsplit('/').next().unwrap_or(src);
            let shown = |slot, width, height, inline| Lookup::Shown {
                image: ContentImage {
                    slot,
                    width,
                    height,
                },
                inline,
            };
            match file {
                "t12x.png" => shown(1, 17, 12, true),
                "t.png" => shown(2, 75, 75, true),
                "a.png" => shown(3, 40, 30, true),
                "plate.png" => shown(4, 640, 900, false),
                "stop.png" => Lookup::Stop,
                _ => Lookup::Missing,
            }
        }
    }

    /// The log for `xhtml`, whole and streamed at several chunk sizes
    /// (which must agree), with the empty paragraph ends that only end
    /// another dropped.
    fn events(xhtml: &str) -> Vec<String> {
        fn parse(xhtml: &str, streamed: Option<usize>) -> Vec<String> {
            let mut log = Log::default();
            let mut index = Index;
            let mut scratch = vec![0xA5u8; 4096];
            let mut sink = ImageParagraphSink::new(&mut log, &mut index, &mut scratch);
            match streamed {
                None => xhtml_blocks_to_sink(xhtml, None, &mut sink).expect("whole parse"),
                Some(chunk_len) => {
                    let mut tokenizer = StreamingXmlTokenizer::new();
                    let mut parser = XhtmlBlockStreamParser::new(false);
                    for chunk in xhtml.as_bytes().chunks(chunk_len) {
                        tokenizer
                            .feed_xhtml_blocks(chunk, &mut parser, None, &mut sink)
                            .expect("chunk feeds");
                    }
                    tokenizer
                        .finish_xhtml_blocks(&mut parser, &mut sink)
                        .expect("finish");
                }
            }
            sink.finish().unwrap();
            assert!(!sink.holds_images());
            log.0
        }
        let whole = parse(xhtml, None);
        for chunk_len in [1usize, 3, 7, 4096] {
            assert_eq!(
                parse(xhtml, Some(chunk_len)),
                whole,
                "chunk size {chunk_len}"
            );
        }
        let mut events: Vec<String> = Vec::new();
        for event in whole {
            if event.starts_with("text \"\" ")
                && event.contains("end=true")
                && events.last().is_some_and(|last| last.contains("end=true"))
            {
                continue;
            }
            events.push(event);
        }
        events
    }

    #[test]
    fn an_image_among_words_goes_inside_its_line() {
        assert_eq!(
            events(r#"<body><p>Complete the <img alt="PICT" src="t12x.png"/> AEBG.</p></body>"#),
            [
                "text \"\" Body Regular Justify end=true @0",
                "text \"Complete the \" Body Regular Justify end=false @1",
                "inline slot=1 17x12 space=true cap=false Body Regular Justify @15",
                "text \" AEBG.\" Body Regular Justify end=true @16",
            ]
        );
    }

    #[test]
    fn images_at_a_paragraph_start_wait_for_its_text() {
        // Text follows: inside the line. The paragraph ends first: a block.
        assert_eq!(
            events(
                r#"<body><p><img src="a.png"/>Start</p><p>End <img src="a.png"/></p><p><img src="a.png"/></p></body>"#
            ),
            [
                "text \"\" Body Regular Justify end=true @0",
                "inline slot=3 40x30 space=false cap=false Body Regular Justify @1",
                "text \"Start\" Body Regular Justify end=true @2",
                "text \"End \" Body Regular Justify end=false @9",
                "inline slot=3 40x30 space=true cap=false Body Regular Justify @14",
                "text \"\" Body Regular Justify end=true @15",
                "block slot=3 40x30 alt=\"\" @17",
                "text \"\" Body Regular Justify end=true @18",
            ]
        );
        // Two images alone with a space between: blocks, the spacing in
        // its place; and a missing image's alt among them.
        assert_eq!(
            events(
                r#"<body><p><img src="a.png"/> <img alt="Gone &amp; lost" src="gone.png"/></p></body>"#
            ),
            [
                "text \"\" Body Regular Justify end=true @0",
                "block slot=3 40x30 alt=\"\" @1",
                "text \" \" Body Regular Justify end=true @2",
                "text \"Gone & lost\" Body Italic Center end=true @4",
            ]
        );
    }

    #[test]
    fn a_missing_image_in_a_paragraph_shows_its_alt_in_the_line() {
        assert_eq!(
            events(r#"<body><p>See <img alt="the  plate" src="gone.png"/> here.</p></body>"#),
            [
                "text \"\" Body Regular Justify end=true @0",
                "text \"See \" Body Regular Justify end=false @1",
                "text \"the plate\" Body Italic Justify end=false @6",
                "text \" here.\" Body Regular Justify end=true @16",
            ]
        );
    }

    #[test]
    fn an_illustration_ends_the_paragraph_around_it() {
        assert_eq!(
            events(
                r#"<body><p>Before <img alt="A &quot;plate&quot;" src="plate.png"/> after</p></body>"#
            ),
            [
                "text \"\" Body Regular Justify end=true @0",
                "text \"Before \" Body Regular Justify end=false @1",
                "block slot=4 640x900 alt=\"A &quot;plate&quot;\" @9",
                "text \" after\" Body Regular Justify end=true @10",
            ]
        );
    }

    #[test]
    fn a_drop_cap_in_its_own_block_opens_the_next_paragraph() {
        // Rackham's Alice.
        assert_eq!(
            events(concat!(
                r#"<body><div class="chapter">CHAPTER III</div>"#,
                r#"<div class="figleft" style="width: 75px;">"#,
                "\n",
                r#"<img alt="T" src="t.png" title="" id="img_images_t.png"/>"#,
                "\n",
                r#"</div><div class="unindent">HEY were indeed</div></body>"#,
            )),
            [
                "text \"\" Body Regular Justify end=true @0",
                "text \"CHAPTER III\" Body Regular Justify end=true @1",
                "text \" \" Body Regular Justify end=false @14",
                "text \"\" Body Regular Justify end=true @17",
                "inline slot=2 75x75 space=false cap=true Body Regular Justify @16",
                "text \"HEY were indeed\" Body Regular Justify end=true @19",
            ]
        );
        // Before a heading, a centred line, or another image, the drop cap
        // stays a block.
        assert_eq!(
            events(concat!(
                r#"<body><p class="figleft"><img alt="T" src="t.png"/></p>"#,
                r#"<h2>Chapter</h2><p>text</p></body>"#,
            )),
            [
                "text \"\" Body Regular Justify end=true @0",
                "block slot=2 75x75 alt=\"T\" @1",
                "text \"Chapter\" Heading2 Bold Center end=true @4",
                "text \"text\" Body Regular Justify end=true @13",
            ]
        );
        assert_eq!(
            events(concat!(
                r#"<body><p style="float: left"><img alt="&quot;C" src="t.png"/></p>"#,
                r#"<p><img src="a.png"/>go</p></body>"#,
            )),
            [
                "text \"\" Body Regular Justify end=true @0",
                "block slot=2 75x75 alt=\"&quot;C\" @1",
                "inline slot=3 40x30 space=false cap=false Body Regular Justify @4",
                "text \"go\" Body Regular Justify end=true @5",
            ]
        );
        // A floated image whose alt is not a letter is an ordinary image.
        assert_eq!(
            events(
                r#"<body><p class="figleft"><img alt="A cat" src="t.png"/></p><p>text</p></body>"#
            ),
            [
                "text \"\" Body Regular Justify end=true @0",
                "block slot=2 75x75 alt=\"A cat\" @1",
                "text \"\" Body Regular Justify end=true @2",
                "text \"text\" Body Regular Justify end=true @4",
            ]
        );
    }

    #[test]
    fn svg_images_are_blocks_and_missing_ones_leave_no_trace() {
        assert_eq!(
            events(concat!(
                r#"<body><p>text <svg><image href="a.png"/></svg> more</p>"#,
                r#"<div><svg><image xlink:href="gone.png"/></svg></div></body>"#,
            )),
            [
                "text \"\" Body Regular Justify end=true @0",
                "text \"text \" Body Regular Justify end=true @1",
                "block slot=3 40x30 alt=\"\" @7",
                "text \" more\" Body Regular Justify end=true @8",
            ]
        );
    }

    #[test]
    fn an_index_that_cannot_be_read_stops_the_chapter() {
        let mut log = Log::default();
        let mut index = Index;
        let mut scratch = [0u8; 512];
        let mut sink = ImageParagraphSink::new(&mut log, &mut index, &mut scratch);
        assert_eq!(
            xhtml_blocks_to_sink(
                r#"<body><p>a <img src="stop.png"/></p></body>"#,
                None,
                &mut sink
            ),
            Err(XhtmlError::TooManyRuns)
        );
    }

    #[test]
    fn too_many_held_images_go_out_as_blocks() {
        let mut xhtml = String::from("<body><p>");
        for _ in 0..40 {
            xhtml.push_str(r#"<img src="a.png"/>"#);
        }
        xhtml.push_str("then text</p></body>");
        let log = events(&xhtml);
        let blocks = log.iter().filter(|line| line.starts_with("block")).count();
        let inline = log.iter().filter(|line| line.starts_with("inline")).count();
        assert_eq!((blocks, inline), (32, 8));
        assert!(log.last().unwrap().starts_with("text \"then text\""));
        // Offsets stay the parser's: one per image, in order.
        let offsets: Vec<u32> = log
            .iter()
            .filter_map(|line| line.rsplit('@').next()?.parse().ok())
            .collect();
        let mut sorted = offsets.clone();
        sorted.sort_unstable();
        assert_eq!(offsets, sorted);
        assert_eq!(offsets[1..=40], (1..=40).collect::<Vec<u32>>()[..]);

        // With scratch too small to hold an image, every image at a
        // paragraph's start is a block at once; one in a paragraph with
        // text is never held, and still goes inside its line.
        let mut log = Log::default();
        let mut index = Index;
        let mut scratch = [0u8; 8];
        let mut sink = ImageParagraphSink::new(&mut log, &mut index, &mut scratch);
        xhtml_blocks_to_sink(
            r#"<body><p><img src="a.png"/>x</p><p>y <img src="a.png"/></p></body>"#,
            None,
            &mut sink,
        )
        .unwrap();
        assert!(log.0.iter().any(|line| line.starts_with("block slot=3")));
        assert!(log.0.iter().any(|line| line.starts_with("inline slot=3")));
    }

    #[test]
    fn held_records_round_trip_through_the_scratch() {
        let mut scratch = [0u8; 256];
        let mut pending = PendingImages::new(&mut scratch);
        let image = ContentImage {
            slot: 9,
            width: 200,
            height: 100,
        };
        let shown = PendingImage {
            held: Held::Shown(image),
            leading_space: true,
            drop_cap: true,
            offset: 77,
            role: TextRole::BlockQuote,
            style: FontStyle::BoldItalic,
            align: TextAlign::Left,
        };
        assert!(pending.push(shown, |out| {
            out[..3].copy_from_slice(b"alt");
            Some(3)
        }));
        let spacing = PendingImage {
            held: Held::Spacing,
            leading_space: false,
            drop_cap: false,
            offset: 78,
            role: TextRole::Heading3,
            style: FontStyle::Italic,
            align: TextAlign::Center,
        };
        assert!(pending.push(spacing, |out| {
            out[..2].copy_from_slice(b"  ");
            Some(2)
        }));
        let held: Vec<_> = pending.iter().collect();
        assert_eq!(held, [(shown, "alt"), (spacing, "  ")]);
        assert!(!pending.holds_lone_drop_cap());
        // No room for a third of that size.
        let mut small = [0u8; PENDING_HEAD_BYTES + 2];
        let mut pending = PendingImages::new(&mut small);
        assert!(pending.push(spacing, |out| {
            out[..2].copy_from_slice(b"  ");
            Some(2)
        }));
        assert!(!pending.push(spacing, |_| Some(2)));
    }
}
