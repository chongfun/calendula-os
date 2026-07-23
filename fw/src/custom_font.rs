//! The custom font pack as the firmware reaches it: the SD-card side of
//! [`ui::custom_font`], plus glyph drawing.
//!
//! Every decision about *when* the card is worth touching lives in
//! `ui::custom_font`, behind [`PackSource`] and [`PackReader`], where host
//! tests can count the opens. This module is the implementation of those
//! two traits over `embedded_sdmmc`, and the drawing that reads the glyph
//! bitmaps they point at.

use crate::reader_store::ReaderStore;
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
use proto::text::TextAlign;
use ui::custom_font::{MetricRecord, PackReader, PackSource};

/// The metric cache is pure RAM bookkeeping, so it lives with the
/// measurement logic in `ui`; firmware keeps the name it has always used.
pub(crate) use ui::custom_font::MetricCache;

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
        let Ok(xteink) = self.root.open_dir(proto::cache::CACHE_ROOT_DIR) else {
            return false;
        };
        let Ok(fonts) = xteink.open_dir(FONT_PACK_DIR) else {
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
pub(crate) fn for_each_metric<
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

pub(crate) fn draw_reading_page_body<
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
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if !source.custom_font_available() {
        return false;
    }
    let Some(xteink) = root.open_dir(proto::cache::CACHE_ROOT_DIR).ok() else {
        return false;
    };
    let Some(fonts) = xteink.open_dir(FONT_PACK_DIR).ok() else {
        return false;
    };
    let Some(file) = fonts.open_file_in_dir(FONT_PACK_FILE, Mode::ReadOnly).ok() else {
        return false;
    };
    let mut font = CustomFontFile {
        file: &file,
        source,
        cache,
    };
    let settings = source.type_settings();
    let page_box = ui::reading::ReadingBlocks::page_box(source);
    let mut ok = true;
    ui::reading::for_each_drawable_block(source, page, |block| {
        if block.record.line_count != 1 {
            ok = false;
            return false;
        }
        let x = match block.record.align {
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
        if !font.draw_styled_line(
            fb,
            block.text,
            settings.size,
            settings.weight,
            block.style,
            x,
            block.y,
        ) {
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
    ) -> bool {
        let mut cursor_x = x;
        let mut run_start = 0usize;
        let mut style = default_style;
        let mut iter = text.char_indices().peekable();
        while let Some((index, ch)) = iter.next() {
            if ch != STYLE_MARKER {
                continue;
            }
            if run_start < index {
                let Some(next_x) = self.draw_text(
                    fb,
                    &text[run_start..index],
                    size,
                    weight,
                    style,
                    cursor_x,
                    baseline_y,
                ) else {
                    return false;
                };
                cursor_x = next_x;
            }
            if let Some((code_index, code)) = iter.next() {
                style = display::font::style_from_marker_code(code).unwrap_or(style);
                run_start = code_index + code.len_utf8();
            } else {
                run_start = index + ch.len_utf8();
            }
        }
        if run_start < text.len() {
            self.draw_text(
                fb,
                &text[run_start..],
                size,
                weight,
                style,
                cursor_x,
                baseline_y,
            )
            .is_some()
        } else {
            true
        }
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
        &self,
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
        // One seek + one read for the whole bitmap instead of a
        // seek/read per row: on-card glyph rows are contiguous, and the
        // per-row cycle dominated glyph drawing on multi-row glyphs.
        let glyph_len = row_bytes.checked_mul(metric.height as usize)?;
        if glyph_len > MAX_GLYPH_BYTES || glyph_len > metric.len as usize {
            return None;
        }
        let offset = face.bitmap_offset.checked_add(metric.offset)?;
        self.file.seek_from_start(offset).ok()?;
        let mut bitmap = [0u8; MAX_GLYPH_BYTES];
        read_full(self.file, &mut bitmap[..glyph_len])?;
        for y in 0..metric.height as usize {
            let row = &bitmap[y * row_bytes..(y + 1) * row_bytes];
            for (x_byte, &byte) in row.iter().enumerate().take(row_bytes) {
                if byte == 0 {
                    continue;
                }
                for bit in 0..8 {
                    let px = x_byte * 8 + bit;
                    if px >= metric.width as usize {
                        break;
                    }
                    if byte & (0x80 >> bit) != 0 {
                        let draw_x = glyph_x + px as i16;
                        let draw_y = glyph_y + y as i16;
                        if draw_x >= 0 && draw_y >= 0 {
                            fb.set_pixel(draw_x as usize, draw_y as usize, false);
                        }
                    }
                }
            }
        }
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

    fn reset_pair(&mut self) {}

    fn width(&self) -> i16 {
        self.right.max(fixed_ceil(self.advance_fp))
    }
}
