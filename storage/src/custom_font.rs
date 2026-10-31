//! The custom font pack as the firmware reaches it: the SD-card side of
//! [`ui::custom_font`], plus glyph drawing.
//!
//! Every decision about *when* the card is worth touching lives in
//! `ui::custom_font`, behind [`PackSource`] and [`PackReader`], where host
//! tests can count the opens. This module is the implementation of those
//! two traits over `embedded_sdmmc`, and the drawing that reads the glyph
//! bitmaps they point at.

use display::fb::Framebuffer;
use display::font::{
    fixed_ceil, fixed_round, FontSize, FontStyle, FontWeight, GlyphMetric, STYLE_MARKER,
};
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::PageRecord;
use proto::font_pack::{
    FontPackFaceRecord, FONT_PACK_DIR, FONT_PACK_FACE_BOLD, FONT_PACK_FACE_BOLD_ITALIC,
    FONT_PACK_FACE_ITALIC, FONT_PACK_FACE_REGULAR, FONT_PACK_FILE, FONT_PACK_SIZE_LARGE,
    FONT_PACK_SIZE_MEDIUM, FONT_PACK_SIZE_SMALL,
};
use proto::image::token::{inline_pieces, InlinePiece};
use proto::text::TextAlign;
use reader_cache::store::ReaderStore;
use ui::custom_font::{BitmapCache, MetricRecord, PackReader, PackSource};

/// The metric cache is pure RAM bookkeeping, so it lives with the
/// measurement logic in `ui`; firmware keeps the name it has always used.
pub use ui::custom_font::MetricCache;

const MAX_ROW_BYTES: usize = 32;
/// Whole-glyph read ceiling for the draw path: 32 row bytes x 256 rows is
/// far past any real glyph; in practice the largest size's glyphs stay
/// well under this, and an oversized claim falls back to not drawing.
const MAX_GLYPH_BYTES: usize = 512;

/// The font pack on the SD card. Opening it is a directory walk plus a
/// file open, which is exactly the cost `ui::custom_font` schedules; this
/// type touches nothing until [`PackSource::with_reader`] is called.
struct SdFontPack<
    'a,
    'volume,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    root: &'a Directory<'volume, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> PackSource
    for SdFontPack<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn with_reader(&self, read: &mut dyn FnMut(&mut dyn PackReader)) -> bool {
        let Ok(cache_root) = self.root.open_dir(proto::cache::CACHE_ROOT_DIR) else {
            return false;
        };
        let Ok(fonts) = cache_root.open_dir(FONT_PACK_DIR) else {
            return false;
        };
        let Ok(file) = fonts.open_file_in_dir(FONT_PACK_FILE, Mode::ReadOnly) else {
            return false;
        };
        read(&mut FilePackReader { file: &file });
        true
    }
}

/// Metric reads from an open pack file.
struct FilePackReader<
    'a,
    'volume,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: &'a File<'volume, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> PackReader
    for FilePackReader<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn read_records(&mut self, offset: u32, records: &mut [MetricRecord]) -> Option<()> {
        // One seek for the whole run: metric records are contiguous.
        self.file.seek_from_start(offset).ok()?;
        for record in records.iter_mut() {
            read_full(self.file, record)?;
        }
        Some(())
    }
}

/// Measure a whole styled text run. Face selection is the only part the
/// firmware owns; the cache-first, open-once-on-miss policy is
/// [`ui::custom_font::for_each_metric`].
// One argument over the limit: the run parameters (size/weight/style) are
// deliberately separate so the caller's cursor state stays the only owner
// of layout context; bundling them would just invent a one-use struct.
#[allow(clippy::too_many_arguments)]
pub fn for_each_metric<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source: &ReaderStore,
    cache: &mut MetricCache,
    size: FontSize,
    weight: FontWeight,
    default_style: FontStyle,
    text: &str,
    visit: impl FnMut(FontStyle, Option<GlyphMetric>),
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    ui::custom_font::for_each_metric(
        &SdFontPack { root },
        cache,
        source.custom_font_identity(),
        |style| custom_face(source, size, weight, style),
        text,
        default_style,
        visit,
    );
}

// Keep the temporary bitmap working set out of the deeper EPUB build stack.
#[inline(never)]
pub fn draw_reading_page_body<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    fb: &mut Framebuffer,
    source: &ReaderStore,
    cache: &mut MetricCache,
    page: PageRecord,
    mut draw_image: impl FnMut(&mut Framebuffer, &ui::reading::ReaderImage<'_>),
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if !source.custom_font_available() {
        return false;
    }
    let Some(cache_root) = root.open_dir(proto::cache::CACHE_ROOT_DIR).ok() else {
        return false;
    };
    let Some(fonts) = cache_root.open_dir(FONT_PACK_DIR).ok() else {
        return false;
    };
    let Some(file) = fonts.open_file_in_dir(FONT_PACK_FILE, Mode::ReadOnly).ok() else {
        return false;
    };
    let mut font = CustomFontFile {
        file: &file,
        source,
        cache,
        bitmaps: BitmapCache::new(),
    };
    let settings = source.type_settings();
    let page_box = ui::reading::ReadingBlocks::page_box(source);
    let mut ok = true;
    ui::reading::for_each_drawable_block(source, page, |block| {
        // As `ui::reading::draw_reading_page_body` draws them: an image
        // block goes to `draw_image` with its placement, its text (an image
        // token) never drawn; so do the images inside a line, where they
        // stand among its text.
        if let Some(image) = block.image {
            draw_image(fb, &image);
            return true;
        }
        if block.record.has_inline_images() {
            let images = ui::reading::InlineImages::for_line(source, block.record);
            ok = font
                .draw_inline_line(fb, &images, &block, page_box, &mut draw_image)
                .is_some();
            return ok;
        }
        if block.record.line_count != 1 {
            ok = false;
            return false;
        }
        let align = ui::reading::effective_align(block.record.align, settings);
        let x = match align {
            TextAlign::Center => {
                let width = font
                    .styled_ink_width(block.text, settings.size, settings.weight, block.style)
                    .min(page_box.right - page_box.left);
                ((page_box.left + page_box.right - width) / 2).max(page_box.left)
            }
            TextAlign::Left | TextAlign::Justify => {
                page_box.x_for(block.record.role) + block.indent
            }
        };
        let drawn = if align == TextAlign::Justify && !block.paragraph_end {
            font.draw_justified_line(
                fb,
                block.text,
                settings.size,
                settings.weight,
                block.style,
                x,
                block.y,
                page_box.right,
            )
        } else {
            font.draw_styled_line(
                fb,
                block.text,
                settings.size,
                settings.weight,
                block.style,
                x,
                block.y,
            )
        };
        if drawn.is_none() {
            ok = false;
            return false;
        }
        true
    });
    ok
}

struct CustomFontFile<
    'file,
    'volume,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: &'file File<'volume, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source: &'file ReaderStore,
    cache: &'file mut MetricCache,
    bitmaps: BitmapCache,
}

impl<
        'file,
        'volume,
        D,
        T,
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > CustomFontFile<'file, 'volume, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    #[allow(clippy::too_many_arguments)]
    fn draw_styled_line(
        &mut self,
        fb: &mut Framebuffer,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        default_style: FontStyle,
        x: i16,
        baseline_y: i16,
    ) -> Option<i16> {
        let mut cursor_x = x;
        let mut run_start = 0usize;
        let mut style = default_style;
        let mut iter = text.char_indices().peekable();
        while let Some((index, ch)) = iter.next() {
            if ch != STYLE_MARKER {
                continue;
            }
            if run_start < index {
                cursor_x = self.draw_text(
                    fb,
                    &text[run_start..index],
                    size,
                    weight,
                    style,
                    cursor_x,
                    baseline_y,
                )?;
            }
            if let Some((code_index, code)) = iter.next() {
                style = display::font::style_from_marker_code(code).unwrap_or(style);
                run_start = code_index + code.len_utf8();
            } else {
                run_start = index + ch.len_utf8();
            }
        }
        if run_start < text.len() {
            cursor_x = self.draw_text(
                fb,
                &text[run_start..],
                size,
                weight,
                style,
                cursor_x,
                baseline_y,
            )?;
        }
        Some(cursor_x)
    }

    /// Justified counterpart of `draw_styled_line`, spreading the leftover
    /// width over inter-word spaces exactly as `ui::reading` does for the
    /// built-in faces.
    #[allow(clippy::too_many_arguments)]
    fn draw_justified_line(
        &mut self,
        fb: &mut Framebuffer,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        default_style: FontStyle,
        x: i16,
        baseline_y: i16,
        right: i16,
    ) -> Option<i16> {
        let gaps = ui::reading::justify_gap_count(text);
        let natural = self.styled_ink_width(text, size, weight, default_style);
        let space = self
            .styled_ink_width(" x", size, weight, default_style)
            .saturating_sub(self.styled_ink_width("x", size, weight, default_style));
        let Some(extra) = ui::reading::justify_extra(x, natural, right, gaps, space) else {
            return self.draw_styled_line(fb, text, size, weight, default_style, x, baseline_y);
        };
        let per_gap = extra / gaps as i16;
        let mut remainder = extra % gaps as i16;
        let mut cursor_x = x;
        let completed = ui::reading::walk_styled_words(text, default_style, |step| {
            let next = match step {
                ui::reading::StyledWordStep::Word(word, style) => {
                    self.draw_styled_line(fb, word, size, weight, style, cursor_x, baseline_y)
                }
                ui::reading::StyledWordStep::Gap(style) => self
                    .draw_text(fb, " ", size, weight, style, cursor_x, baseline_y)
                    .map(|after_space| {
                        let mut step_x = after_space + per_gap;
                        if remainder > 0 {
                            step_x += 1;
                            remainder -= 1;
                        }
                        step_x
                    }),
            };
            match next {
                Some(next_x) => {
                    cursor_x = next_x;
                    true
                }
                None => false,
            }
        });
        if completed {
            Some(cursor_x)
        } else {
            None
        }
    }

    /// A line holding inline images, aligned as its record says: justified
    /// as a text line is, at natural spacing from the margin, or centred,
    /// as the built-in draw does, in this face. `None` when the face cannot
    /// draw a run of its text.
    fn draw_inline_line(
        &mut self,
        fb: &mut Framebuffer,
        images: &ui::reading::InlineImages,
        block: &ui::reading::ReaderDrawableBlock<'_>,
        page_box: ui::reading::PageBox,
        draw_image: &mut dyn FnMut(&mut Framebuffer, &ui::reading::ReaderImage<'_>),
    ) -> Option<i16> {
        let settings = self.source.type_settings();
        let (size, weight) = (settings.size, settings.weight);
        let align = ui::reading::effective_align(block.record.align, settings);
        let x = match align {
            TextAlign::Center => {
                let width = self
                    .inline_ink_width(block.text, size, weight, block.style)
                    .min(page_box.right - page_box.left);
                ((page_box.left + page_box.right - width) / 2).max(page_box.left)
            }
            TextAlign::Left | TextAlign::Justify => {
                page_box.x_for(block.record.role) + block.indent
            }
        };
        if align == TextAlign::Justify && !block.paragraph_end {
            return self.draw_justified_inline_line(
                fb,
                images,
                block.text,
                size,
                weight,
                block.style,
                x,
                block.y,
                page_box.right,
                draw_image,
            );
        }
        self.draw_inline_styled_line(
            fb,
            images,
            block.text,
            size,
            weight,
            block.style,
            x,
            block.y,
            draw_image,
        )
    }

    /// `draw_justified_line` for a line holding inline images, an image
    /// counting as a word.
    #[allow(clippy::too_many_arguments)]
    fn draw_justified_inline_line(
        &mut self,
        fb: &mut Framebuffer,
        images: &ui::reading::InlineImages,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        default_style: FontStyle,
        x: i16,
        baseline_y: i16,
        right: i16,
        draw_image: &mut dyn FnMut(&mut Framebuffer, &ui::reading::ReaderImage<'_>),
    ) -> Option<i16> {
        let gaps = ui::reading::justify_gap_count(text);
        let natural = self.inline_ink_width(text, size, weight, default_style);
        let Some(extra) = ui::reading::justify_inline_extra(x, natural, right, gaps) else {
            return self.draw_inline_styled_line(
                fb,
                images,
                text,
                size,
                weight,
                default_style,
                x,
                baseline_y,
                draw_image,
            );
        };
        let per_gap = extra / gaps as i16;
        let mut remainder = extra % gaps as i16;
        let mut cursor_x = x;
        let completed = ui::reading::walk_styled_words(text, default_style, |step| {
            let next = match step {
                ui::reading::StyledWordStep::Word(word, style) => self.draw_inline_styled_line(
                    fb, images, word, size, weight, style, cursor_x, baseline_y, draw_image,
                ),
                ui::reading::StyledWordStep::Gap(style) => self
                    .draw_text(fb, " ", size, weight, style, cursor_x, baseline_y)
                    .map(|after_space| {
                        let mut step_x = after_space + per_gap;
                        if remainder > 0 {
                            step_x += 1;
                            remainder -= 1;
                        }
                        step_x
                    }),
            };
            match next {
                Some(next_x) => {
                    cursor_x = next_x;
                    true
                }
                None => false,
            }
        });
        completed.then_some(cursor_x)
    }

    /// `draw_styled_line` for a line holding inline images: the style
    /// carries on past each image, and each image goes to `draw_image`
    /// where it stands, its bottom row just above the baseline.
    #[allow(clippy::too_many_arguments)]
    fn draw_inline_styled_line(
        &mut self,
        fb: &mut Framebuffer,
        images: &ui::reading::InlineImages,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        default_style: FontStyle,
        x: i16,
        baseline_y: i16,
        draw_image: &mut dyn FnMut(&mut Framebuffer, &ui::reading::ReaderImage<'_>),
    ) -> Option<i16> {
        let mut cursor_x = x;
        let mut style = default_style;
        for piece in inline_pieces(text) {
            let run = match piece {
                InlinePiece::Image(token) => {
                    let image = images.place(token, cursor_x, baseline_y);
                    draw_image(fb, &image);
                    cursor_x = cursor_x.saturating_add(image.width as i16);
                    continue;
                }
                InlinePiece::Text(run) => run,
            };
            let mut run_start = 0usize;
            let mut iter = run.char_indices();
            while let Some((index, ch)) = iter.next() {
                if ch != STYLE_MARKER {
                    continue;
                }
                if run_start < index {
                    cursor_x = self.draw_text(
                        fb,
                        &run[run_start..index],
                        size,
                        weight,
                        style,
                        cursor_x,
                        baseline_y,
                    )?;
                }
                if let Some((code_index, code)) = iter.next() {
                    style = display::font::style_from_marker_code(code).unwrap_or(style);
                    run_start = code_index + code.len_utf8();
                } else {
                    run_start = index + ch.len_utf8();
                }
            }
            if run_start < run.len() {
                cursor_x = self.draw_text(
                    fb,
                    &run[run_start..],
                    size,
                    weight,
                    style,
                    cursor_x,
                    baseline_y,
                )?;
            }
        }
        Some(cursor_x)
    }

    /// `styled_ink_width` for a line holding inline images, each image
    /// measuring its width.
    fn inline_ink_width(
        &mut self,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        default_style: FontStyle,
    ) -> i16 {
        let mut ink = CustomInkCursor::new();
        let mut style = default_style;
        for piece in inline_pieces(text) {
            let run = match piece {
                InlinePiece::Image(token) => {
                    ink.push_object(token.width);
                    continue;
                }
                InlinePiece::Text(run) => run,
            };
            let mut chars = run.chars();
            while let Some(ch) = chars.next() {
                if ch == STYLE_MARKER {
                    if let Some(code) = chars.next() {
                        ink.reset_pair();
                        style = display::font::style_from_marker_code(code).unwrap_or(style);
                    }
                    continue;
                }
                if let Some(face) = custom_face(self.source, size, weight, style) {
                    ink.push_char(self, face, ch);
                }
            }
        }
        ink.width()
    }

    fn styled_ink_width(
        &mut self,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        default_style: FontStyle,
    ) -> i16 {
        let mut ink = CustomInkCursor::new();
        let mut style = default_style;
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == STYLE_MARKER {
                if let Some(code) = chars.next() {
                    ink.reset_pair();
                    style = display::font::style_from_marker_code(code).unwrap_or(style);
                }
                continue;
            }
            if let Some(face) = custom_face(self.source, size, weight, style) {
                ink.push_char(self, face, ch);
            }
        }
        ink.width()
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        fb: &mut Framebuffer,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        style: FontStyle,
        x: i16,
        baseline_y: i16,
    ) -> Option<i16> {
        let face = custom_face(self.source, size, weight, style)?;
        let mut cursor_fp = (x as i32) << 4;
        for ch in text.chars() {
            let x = fixed_round(cursor_fp);
            let metric = self.metric_for_char(face, ch)?;
            self.draw_glyph_bitmap(fb, face, metric, x, baseline_y)?;
            cursor_fp += metric.advance_fp as i32;
        }
        Some(fixed_round(cursor_fp))
    }

    fn metric_for_char(&mut self, face: FontPackFaceRecord, ch: char) -> Option<GlyphMetric> {
        let identity = self.source.custom_font_identity();
        if let Some(metric) = self.cache.lookup(identity, face, ch) {
            return Some(metric);
        }
        // The pack is already open for drawing, so a miss here costs a read
        // rather than the open `for_each_metric` schedules.
        self.cache
            .fill_and_lookup(&mut FilePackReader { file: self.file }, identity, face, ch)
    }

    fn draw_glyph_bitmap(
        &mut self,
        fb: &mut Framebuffer,
        face: FontPackFaceRecord,
        metric: GlyphMetric,
        x: i16,
        baseline_y: i16,
    ) -> Option<()> {
        let row_bytes = (metric.width as usize).div_ceil(8);
        if row_bytes > MAX_ROW_BYTES {
            return None;
        }
        let glyph_x = x + metric.x_offset as i16;
        let glyph_y = baseline_y + metric.y_offset as i16;
        let glyph_len = row_bytes.checked_mul(metric.height as usize)?;
        if glyph_len > MAX_GLYPH_BYTES || glyph_len > metric.len as usize {
            return None;
        }
        let offset = face.bitmap_offset.checked_add(metric.offset)?;
        let file = self.file;
        let bitmap = self.bitmaps.get_or_load(
            self.source.custom_font_identity(),
            offset,
            glyph_len,
            |bytes| {
                file.seek_from_start(offset).ok()?;
                read_full(file, bytes)
            },
        )?;
        // Same whole-glyph blit the built-in faces take: it batches the
        // portrait column writes eight rows at a time instead of paying a
        // coordinate transform per lit pixel.
        fb.blit_bitmap(
            glyph_x as i32,
            glyph_y as i32,
            bitmap,
            metric.width as usize,
            metric.height as usize,
            false,
        );
        Some(())
    }
}

fn custom_face(
    source: &ReaderStore,
    size: FontSize,
    weight: FontWeight,
    style: FontStyle,
) -> Option<FontPackFaceRecord> {
    let size_px = match size {
        FontSize::Small => FONT_PACK_SIZE_SMALL,
        FontSize::Medium => FONT_PACK_SIZE_MEDIUM,
        FontSize::Large => FONT_PACK_SIZE_LARGE,
    };
    let style = match (weight, style) {
        (FontWeight::Normal, FontStyle::Regular) => FONT_PACK_FACE_REGULAR,
        (FontWeight::Normal, FontStyle::Italic) => FONT_PACK_FACE_ITALIC,
        (FontWeight::Normal, FontStyle::Bold) => FONT_PACK_FACE_BOLD,
        (FontWeight::Normal, FontStyle::BoldItalic) => FONT_PACK_FACE_BOLD_ITALIC,
        (FontWeight::Heavy, FontStyle::Regular | FontStyle::Bold) => FONT_PACK_FACE_BOLD,
        (FontWeight::Heavy, FontStyle::Italic | FontStyle::BoldItalic) => {
            FONT_PACK_FACE_BOLD_ITALIC
        }
    };
    source.custom_font_face(size_px, style)
}

fn read_full<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut buf: &mut [u8],
) -> Option<()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !buf.is_empty() {
        let read = file.read(buf).ok()?;
        if read == 0 {
            return None;
        }
        let tmp = buf;
        buf = &mut tmp[read..];
    }
    Some(())
}

#[derive(Clone, Copy, Debug, Default)]
struct CustomInkCursor {
    advance_fp: i32,
    right: i16,
}

impl CustomInkCursor {
    const fn new() -> Self {
        Self {
            advance_fp: 0,
            right: 0,
        }
    }

    fn push_char<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
        &mut self,
        font: &mut CustomFontFile<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        face: FontPackFaceRecord,
        ch: char,
    ) where
        D: embedded_sdmmc::BlockDevice,
        T: TimeSource,
    {
        let Some(metric) = font.metric_for_char(face, ch) else {
            self.advance_fp += 8 << 4;
            self.right = self.right.max(fixed_ceil(self.advance_fp));
            return;
        };
        let advance = fixed_round(self.advance_fp);
        let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
        self.right = self.right.max(glyph_right);
        self.advance_fp += metric.advance_fp as i32;
    }

    /// An object `width` columns wide standing in the line, as an inline
    /// image does: it advances the pen by its width, with no bearing.
    fn push_object(&mut self, width: u16) {
        self.advance_fp += i32::from(width) << 4;
        self.right = self.right.max(fixed_ceil(self.advance_fp));
    }

    fn reset_pair(&mut self) {}

    fn width(&self) -> i16 {
        self.right.max(fixed_ceil(self.advance_fp))
    }
}
