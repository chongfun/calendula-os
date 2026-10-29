use crate::book::{BookId, BookMeta, BookSource, ChapterMeta, CoverStatus};
use crate::text::{FontStyle, TextAlign, TextBlock, TextRole};
use heapless::Vec;
use miniz_oxide::inflate::core::decompress_with_limit;
use miniz_oxide::inflate::core::inflate_flags::{
    TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
pub use miniz_oxide::inflate::core::DecompressorOxide;
use miniz_oxide::inflate::decompress_slice_iter_to_slice;
use miniz_oxide::inflate::TINFLStatus;

// Upper bound on spine items (one per chapter file). The spine lives in the OPF
// arena, 8 bytes an item plus its href, not on the stack (see `Spine`), so in
// practice the arena's size decides how many fit; this caps the slot table so
// strings always keep room. A spine past either limit sets `spine_truncated`
// rather than silently dropping the tail. One value for firmware and host so a
// host test exercises the same ceiling the device does.
pub const MAX_SPINE_ITEMS: usize = 1024;
pub const MAX_ENTRY_NAME_BYTES: usize = 160;
const ZIP_TAIL_READ_WINDOW: usize = 512;
const ZIP_EOCD_MIN_BYTES: u32 = 22;
const ZIP_EOCD_OVERLAP_BYTES: u32 = ZIP_EOCD_MIN_BYTES - 1;

pub trait ByteStream {
    type Error;

    fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error>;
}

pub trait ReadAt {
    type Error;

    fn len(&mut self) -> Result<u32, Self::Error>;
    fn is_empty(&mut self) -> Result<bool, Self::Error> {
        Ok(self.len()? == 0)
    }
    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    Start(&'a str),
    End(&'a str),
    Text(&'a str),
    Cdata(&'a str),
}

pub struct XmlCursor<'a> {
    input: &'a str,
    cursor: usize,
}

impl<'a> XmlCursor<'a> {
    pub const fn new(input: &'a str) -> Self {
        Self { input, cursor: 0 }
    }

    pub fn next_token(&mut self) -> Option<Token<'a>> {
        while self.cursor < self.input.len() {
            let rest = &self.input[self.cursor..];
            if let Some(after_lt) = rest.strip_prefix('<') {
                if let Some(comment) = after_lt.strip_prefix("!--") {
                    let end = comment.find("-->")?;
                    self.cursor += 4 + end + 3; // Opening and closing comment delimiters.
                    continue;
                }
                if let Some(cdata) = after_lt.strip_prefix("![CDATA[") {
                    let end = cdata.find("]]>")?;
                    self.cursor += 9 + end + 3;
                    let text = &cdata[..end];
                    if !text.is_empty() {
                        return Some(Token::Cdata(text));
                    }
                    continue;
                }
                let end = tag_end(after_lt)?;
                self.cursor += end + 2;
                let tag = after_lt[..end].trim();
                if tag.starts_with('!') || tag.starts_with('?') {
                    continue;
                }
                if let Some(name) = tag.strip_prefix('/') {
                    return Some(Token::End(name.trim()));
                }
                return Some(Token::Start(tag));
            }

            let end = rest.find('<').unwrap_or(rest.len());
            self.cursor += end;
            let text = &rest[..end];
            if !text.is_empty() {
                return Some(Token::Text(text));
            }
        }
        None
    }
}

/// Where the tag after a `<` ends: the first `>` outside a quoted value, so
/// `title="1 > 0"` stays inside its tag. A declaration or processing
/// instruction ends at its first `>`, because an apostrophe in one is text.
/// Comments never get here: [`XmlCursor::next_token`] skips them through
/// `-->` first.
fn tag_end(after_lt: &str) -> Option<usize> {
    if after_lt.starts_with(['!', '?']) {
        return after_lt.find('>');
    }
    let mut quote = None;
    for (at, byte) in after_lt.bytes().enumerate() {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'>' => return Some(at),
            None if matches!(byte, b'"' | b'\'') => quote = Some(byte),
            None => {}
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZipError {
    MissingEndOfCentralDirectory,
    BadCentralDirectory,
    BadLocalHeader,
    EntryNotFound,
    NameTooLong,
    UnsupportedCompression,
    OutputTooSmall,
    Inflate,
    Io,
    EntryBufferTooSmall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZipEntry<'a> {
    pub name: &'a str,
    pub compression_method: u16,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub local_header_offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedZipEntry {
    pub compression_method: u16,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub local_header_offset: u32,
}

/// How a [`ZipStream`] resolves entry names against the central directory.
#[derive(Clone, Copy)]
enum CentralLookup<'a> {
    /// The whole central directory fit in the tail scratch.
    Cached(&'a [u8]),
    /// One streaming pass built a bounded `(name hash, entry)` index in
    /// the tail scratch. Hash hits are verified against the local file
    /// header before use, so collisions cannot return the wrong entry.
    Indexed { records: &'a [u8], complete: bool },
    /// No acceleration: walk the central directory on storage per lookup.
    Walk,
}

const CENTRAL_INDEX_RECORD_BYTES: usize = 20;
const FNV_OFFSET: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

fn fnv1a_update(mut hash: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub struct ZipStream<'a, R> {
    reader: R,
    central_offset: u32,
    central_size: u32,
    entry_count: u16,
    walk_cursor: u32,
    lookup: CentralLookup<'a>,
}

pub struct ZipLocalStream<R> {
    reader: R,
    cursor: u32,
    pending_payload_offset: u32,
    pending_payload_remaining: u32,
}

/// The LZ77 window deflate decodes against, which is also the ring the
/// decoded bytes land in. `inflate::core::decompress` requires a power of
/// two, and a match may reach 32 KiB back, so this is the minimum that
/// decodes every legal stream.
pub const INFLATE_WINDOW_BYTES: usize = 32 * 1024;
const INFLATE_WINDOW_MASK: usize = INFLATE_WINDOW_BYTES - 1;
const _: () = assert!(INFLATE_WINDOW_BYTES.is_power_of_two());

/// Decoder window scratch, owned by the caller.
///
/// The current struct is ~32 KiB (the 32 KiB LZ77 window buffer plus a position
/// counter and a borrowed decoder reference). The ~10.5 KiB `DecompressorOxide`
/// decoder state is allocated in a separate static cell and supplied via
/// [`Self::prepare`].
///
/// The shape is dictated by one requirement: **this must be constructible
/// without ever existing as a value on a tight stack.** Historically, when
/// `ZipInflateScratch` held `InflateState` by value, the combined ~43 KB struct
/// was returned by value against a 42 KB stack. That temporary landed on the stack,
/// ran 11 KB past the bottom into `.bss`, and unwrapped `None` on a clock read.
/// Borrowing the scratch moved the allocation, but `ZipInflateScratch::new()`
/// returning a ~21 KB frame by value still relied on LLVM eliding the copy.
///
/// Splitting out `DecompressorOxide` and keeping `ZipInflateScratch::new()` a
/// `const fn` whose value is **all zero bytes** allows `ZipInflateScratch` to be
/// const-initialized directly in `.bss`. Holding `DecompressorOxide` inside by
/// value — even as `Option<DecompressorOxide>` — would cost 43 KB of flash because
/// `None` is not provably all-zero and would force the whole struct into `.data`.
/// Measured, both ways.
///
/// The decoder is therefore borrowed separately, supplied once by [`Self::prepare`].
pub struct ZipInflateScratch {
    /// `None` until [`Self::prepare`] runs. A null reference, so the const
    /// constructor stays all-zero — see the type docs.
    decompressor: Option<&'static mut DecompressorOxide>,
    window: [u8; INFLATE_WINDOW_BYTES],
    /// Total bytes decoded into `window` this stream, unmasked, so the
    /// write position is `window_pos & INFLATE_WINDOW_MASK`.
    window_pos: usize,
}

impl ZipInflateScratch {
    pub const fn new() -> Self {
        Self {
            decompressor: None,
            window: [0; INFLATE_WINDOW_BYTES],
            window_pos: 0,
        }
    }

    /// Adopt the decoder this scratch will use. Must be called before any
    /// entry is decoded; decoding without it fails every entry.
    ///
    /// The decoder is passed in rather than built here because miniz only
    /// constructs it by value at ~10 KB, and the caller knows which stack
    /// can afford that. Firmware initializes a `StaticCell` for it while
    /// setting up its scratch, on a shallow frame — the alternative is that
    /// it lands several frames deep inside the EPUB open chain, the deepest
    /// stack in the system.
    pub fn prepare(&mut self, decompressor: &'static mut DecompressorOxide) {
        self.decompressor = Some(decompressor);
    }

    /// Take the prepared `DecompressorOxide` reference, if one was provided.
    pub fn take_decompressor(&mut self) -> Option<&'static mut DecompressorOxide> {
        self.decompressor.take()
    }

    /// Borrow the decoder and window as separate pieces, with the decoder
    /// reset for a fresh stream. `None` when [`Self::prepare`] never ran.
    ///
    /// The window keeps the previous entry's bytes; [`window_flags`] is what
    /// stops the new stream from reading them, at no per-entry cost.
    fn begin(&mut self) -> Option<(&mut DecompressorOxide, &mut [u8], &mut usize)> {
        let decompressor = self.decompressor.as_deref_mut()?;
        decompressor.init();
        self.window_pos = 0;
        Some((decompressor, &mut self.window, &mut self.window_pos))
    }
}

impl Default for ZipInflateScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode flags that keep one stream from reading another's leftovers.
///
/// The ring is not cleared between entries, so until it has wrapped once,
/// everything at or past `window_pos` is still the *previous* entry's
/// output. Wrapping mode would happily resolve a back-reference into it:
/// miniz only rejects a distance larger than the whole buffer, computes the
/// source with a wrapping subtraction, and a malformed entry whose first
/// operation is a match at distance 32768 therefore copies out — and emits —
/// 32 KiB of the last entry. ZIP deflate has no preset dictionary, so no
/// legitimate stream does that.
///
/// `TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF` makes miniz compare the
/// distance against `out_pos` instead, i.e. against what *this* stream has
/// produced, and fail the stream when it reaches back further. Once the ring
/// has wrapped, every byte within 32 KiB is genuinely this stream's own, and
/// wrapping mode — the only mode that can address it — takes over. The flag
/// is read fresh on each `decompress` call and leaves no state behind, so
/// switching at the wrap is safe.
///
/// This replaces the `FullReset` the old `InflateState` path did, which zeroed
/// a separate 32 KiB dictionary per entry; the point of the caller-owned ring
/// is not to pay that.
const fn window_flags(window_pos: usize) -> u32 {
    if window_pos < INFLATE_WINDOW_BYTES {
        TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF
    } else {
        0
    }
}

impl<R> ZipLocalStream<R>
where
    R: ByteStream,
{
    pub const fn new(reader: R) -> Self {
        Self {
            reader,
            cursor: 0,
            pending_payload_offset: 0,
            pending_payload_remaining: 0,
        }
    }

    pub fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        loop {
            read_exact_stream(&mut self.reader, &mut header_scratch[..30])?;
            let signature = read_u32(header_scratch, 0)?;
            if signature == 0x0201_4b50 || signature == 0x0605_4b50 {
                return Err(ZipError::EntryNotFound);
            }
            if signature != 0x0403_4b50 {
                return Err(ZipError::BadLocalHeader);
            }
            let flags = read_u16(header_scratch, 6)?;
            let compression_method = read_u16(header_scratch, 8)?;
            let compressed_size = read_u32(header_scratch, 18)?;
            let uncompressed_size = read_u32(header_scratch, 22)?;
            let name_len = read_u16(header_scratch, 26)? as usize;
            let extra_len = read_u16(header_scratch, 28)? as usize;
            if flags & 0x0008 != 0 {
                return Err(ZipError::UnsupportedCompression);
            }
            if name_len > name_scratch.len() {
                return Err(ZipError::EntryBufferTooSmall);
            }
            read_exact_stream(&mut self.reader, &mut name_scratch[..name_len])?;
            skip_stream(&mut self.reader, extra_len)?;
            let payload_offset = self
                .cursor
                .checked_add(30 + name_len as u32 + extra_len as u32)
                .ok_or(ZipError::BadLocalHeader)?;
            let entry_matches = core::str::from_utf8(&name_scratch[..name_len])
                .map(|entry_name| entry_name == name)
                .unwrap_or(false);
            if entry_matches {
                self.pending_payload_offset = payload_offset;
                self.pending_payload_remaining = compressed_size;
                return Ok(OwnedZipEntry {
                    compression_method,
                    compressed_size,
                    uncompressed_size,
                    local_header_offset: payload_offset,
                });
            }
            skip_stream(&mut self.reader, compressed_size as usize)?;
            self.cursor = payload_offset
                .checked_add(compressed_size)
                .ok_or(ZipError::BadLocalHeader)?;
        }
    }

    pub fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError> {
        if entry.uncompressed_size as usize > output.len() {
            return Err(ZipError::OutputTooSmall);
        }
        let (len, complete) =
            self.read_entry_prefix_streamed(entry, compressed_scratch, output, inflate_scratch)?;
        if complete {
            Ok(len)
        } else {
            Err(ZipError::OutputTooSmall)
        }
    }

    pub fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        if entry.local_header_offset != self.pending_payload_offset
            || entry.compressed_size != self.pending_payload_remaining
        {
            return Err(ZipError::BadLocalHeader);
        }
        match entry.compression_method {
            0 => {
                let output_len = output.len().min(entry.uncompressed_size as usize);
                read_exact_stream(&mut self.reader, &mut output[..output_len])?;
                self.finish_pending_payload(entry.compressed_size, output_len as u32)?;
                Ok((output_len, output_len == entry.uncompressed_size as usize))
            }
            8 => self.inflate_entry_prefix(entry.compressed_size, input, output, inflate_scratch),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    /// Stream a zip entry's uncompressed bytes into a caller-supplied sink.
    /// `output_window` bounds the chunks: it only needs to be large enough to
    /// make forward progress (a few hundred bytes is plenty), and `emit` never
    /// sees a slice longer than it. Stored entries are read through it;
    /// deflated ones are decoded in the inflate scratch's own ring and it caps
    /// how far each decode may run, so no byte is copied through it. Either
    /// way the parser/tokenizer on the other side consumes the chunks
    /// incrementally and never has to materialize the whole entry in RAM.
    pub fn read_entry_to_sink<F>(
        &mut self,
        entry: OwnedZipEntry,
        input: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        mut emit: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&[u8]) -> Result<(), ZipError>,
    {
        // A zero-length window makes no progress on either path: stored
        // entries would loop forever on a zero-byte read.
        if output_window.is_empty() {
            return Err(ZipError::OutputTooSmall);
        }
        if entry.local_header_offset != self.pending_payload_offset
            || entry.compressed_size != self.pending_payload_remaining
        {
            return Err(ZipError::BadLocalHeader);
        }
        match entry.compression_method {
            0 => {
                let mut remaining = entry.uncompressed_size as usize;
                while remaining > 0 {
                    let take = remaining.min(output_window.len());
                    read_exact_stream(&mut self.reader, &mut output_window[..take])?;
                    emit(&output_window[..take])?;
                    remaining -= take;
                }
                self.finish_pending_payload(entry.compressed_size, entry.uncompressed_size)
            }
            8 => self.inflate_entry_to_sink(
                entry.compressed_size,
                input,
                output_window.len(),
                inflate_scratch,
                emit,
            ),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    fn inflate_entry_to_sink<F>(
        &mut self,
        compressed_size: u32,
        input: &mut [u8],
        emit_limit: usize,
        inflate_scratch: &mut ZipInflateScratch,
        emit: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&[u8]) -> Result<(), ZipError>,
    {
        let reader = &mut self.reader;
        let fetched = inflate_chunks_to_sink(
            |_, buf| read_exact_stream(reader, buf),
            compressed_size,
            input,
            emit_limit,
            inflate_scratch,
            emit,
        )?;
        self.finish_pending_payload(compressed_size, fetched)
    }

    fn inflate_entry_prefix(
        &mut self,
        compressed_size: u32,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        let reader = &mut self.reader;
        let (len, complete, fetched) = inflate_chunks_prefix(
            |_, buf| read_exact_stream(reader, buf),
            compressed_size,
            input,
            output,
            inflate_scratch,
        )?;
        self.finish_pending_payload(compressed_size, fetched)?;
        Ok((len, complete))
    }

    /// Skip whatever part of the pending entry payload was not read from the
    /// underlying stream, then advance the cursor past the entry so the next
    /// `find_entry` starts at a clean local-header boundary.
    fn finish_pending_payload(
        &mut self,
        compressed_size: u32,
        fetched: u32,
    ) -> Result<(), ZipError> {
        skip_stream(
            &mut self.reader,
            compressed_size.saturating_sub(fetched) as usize,
        )?;
        self.cursor = self
            .pending_payload_offset
            .checked_add(compressed_size)
            .ok_or(ZipError::BadLocalHeader)?;
        self.pending_payload_remaining = 0;
        Ok(())
    }
}

impl<'a, R> ZipStream<'a, R>
where
    R: ReadAt,
{
    pub fn new(mut reader: R, tail_scratch: &'a mut [u8]) -> Result<Self, ZipError> {
        let len = reader.len().map_err(|_| ZipError::Io)?;
        if len < ZIP_EOCD_MIN_BYTES || tail_scratch.len() < ZIP_EOCD_MIN_BYTES as usize {
            return Err(ZipError::MissingEndOfCentralDirectory);
        }
        let search_floor = len.saturating_sub(tail_scratch.len() as u32);
        let read_window = tail_scratch.len().min(ZIP_TAIL_READ_WINDOW);
        let mut chunk_end = len;
        let (entry_count, central_size, central_offset) = loop {
            let available = chunk_end.saturating_sub(search_floor) as usize;
            let chunk_len = available.min(read_window);
            if chunk_len < ZIP_EOCD_MIN_BYTES as usize {
                return Err(ZipError::MissingEndOfCentralDirectory);
            }
            let chunk_start = chunk_end - chunk_len as u32;
            let chunk = &mut tail_scratch[..chunk_len];
            read_exact_at(&mut reader, chunk_start, chunk)?;
            if let Some(eocd) = find_eocd(chunk) {
                break (
                    read_u16(chunk, eocd + 10)?,
                    read_u32(chunk, eocd + 12)?,
                    read_u32(chunk, eocd + 16)?,
                );
            }
            if chunk_start == search_floor {
                return Err(ZipError::MissingEndOfCentralDirectory);
            }
            chunk_end = chunk_start + ZIP_EOCD_OVERLAP_BYTES;
        };
        let lookup = if central_size as usize <= tail_scratch.len() {
            let central = &mut tail_scratch[..central_size as usize];
            read_exact_at(&mut reader, central_offset, central)?;
            CentralLookup::Cached(&central[..])
        } else {
            match build_central_index(&mut reader, central_offset, entry_count, tail_scratch) {
                Ok((records, complete)) => CentralLookup::Indexed { records, complete },
                Err(_) => CentralLookup::Walk,
            }
        };
        Ok(Self {
            reader,
            central_offset,
            central_size,
            entry_count,
            walk_cursor: central_offset,
            lookup,
        })
    }

    pub fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        match self.lookup {
            CentralLookup::Cached(central) => {
                find_entry_in_central_cache(central, self.entry_count, name)
            }
            CentralLookup::Indexed { records, complete } => {
                let hash = fnv1a_update(FNV_OFFSET, name.as_bytes());
                let mut unverified_hit = false;
                for record in records.as_chunks::<CENTRAL_INDEX_RECORD_BYTES>().0 {
                    if read_u32(record, 0)? != hash {
                        continue;
                    }
                    let entry = OwnedZipEntry {
                        compression_method: read_u16(record, 16)?,
                        compressed_size: read_u32(record, 8)?,
                        uncompressed_size: read_u32(record, 12)?,
                        local_header_offset: read_u32(record, 4)?,
                    };
                    if self.local_name_matches(
                        entry.local_header_offset,
                        name,
                        header_scratch,
                        name_scratch,
                    )? {
                        return Ok(entry);
                    }
                    unverified_hit = true;
                }
                if complete && !unverified_hit {
                    Err(ZipError::EntryNotFound)
                } else {
                    // Either the index holds only a prefix of the central
                    // directory, or a hash hit failed local-header
                    // verification (possible with central/local name
                    // mismatches); the walk stays authoritative.
                    self.find_entry_by_walk(name, header_scratch, name_scratch)
                }
            }
            CentralLookup::Walk => self.find_entry_by_walk(name, header_scratch, name_scratch),
        }
    }

    fn find_entry_by_walk(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        let central_end = self
            .central_offset
            .checked_add(self.central_size)
            .ok_or(ZipError::BadCentralDirectory)?;
        let mut cursor = self.walk_cursor;
        for _ in 0..self.entry_count {
            if cursor >= central_end {
                cursor = self.central_offset;
            }
            read_exact_at(&mut self.reader, cursor, header_scratch)?;
            if read_u32(header_scratch, 0)? != 0x0201_4b50 {
                return Err(ZipError::BadCentralDirectory);
            }
            let compression_method = read_u16(header_scratch, 10)?;
            let compressed_size = read_u32(header_scratch, 20)?;
            let uncompressed_size = read_u32(header_scratch, 24)?;
            let name_len = read_u16(header_scratch, 28)? as usize;
            let extra_len = read_u16(header_scratch, 30)? as u32;
            let comment_len = read_u16(header_scratch, 32)? as u32;
            let local_header_offset = read_u32(header_scratch, 42)?;
            if name_len > name_scratch.len() {
                return Err(ZipError::EntryBufferTooSmall);
            }
            let next_cursor = cursor
                .checked_add(46 + name_len as u32 + extra_len + comment_len)
                .ok_or(ZipError::BadCentralDirectory)?;
            read_exact_at(&mut self.reader, cursor + 46, &mut name_scratch[..name_len])?;
            if core::str::from_utf8(&name_scratch[..name_len])
                .map(|entry_name| entry_name == name)
                .unwrap_or(false)
            {
                self.walk_cursor = next_cursor;
                return Ok(OwnedZipEntry {
                    compression_method,
                    compressed_size,
                    uncompressed_size,
                    local_header_offset,
                });
            }
            cursor = next_cursor;
        }
        Err(ZipError::EntryNotFound)
    }

    fn local_name_matches(
        &mut self,
        local_header_offset: u32,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<bool, ZipError> {
        let header = &mut header_scratch[..30];
        read_exact_at(&mut self.reader, local_header_offset, header)?;
        if read_u32(header, 0)? != 0x0403_4b50 {
            return Err(ZipError::BadLocalHeader);
        }
        let name_len = read_u16(header, 26)? as usize;
        if name_len != name.len() || name_len > name_scratch.len() {
            return Ok(false);
        }
        read_exact_at(
            &mut self.reader,
            local_header_offset + 30,
            &mut name_scratch[..name_len],
        )?;
        Ok(&name_scratch[..name_len] == name.as_bytes())
    }

    pub fn read_entry(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
    ) -> Result<usize, ZipError> {
        if entry.compressed_size as usize > compressed_scratch.len() {
            return Err(ZipError::OutputTooSmall);
        }
        if entry.uncompressed_size as usize > output.len() {
            return Err(ZipError::OutputTooSmall);
        }
        let payload_offset = self.entry_payload_offset(entry)?;
        let compressed = &mut compressed_scratch[..entry.compressed_size as usize];
        read_exact_at(&mut self.reader, payload_offset, compressed)?;
        match entry.compression_method {
            0 => {
                if output.len() < compressed.len() {
                    return Err(ZipError::OutputTooSmall);
                }
                output[..compressed.len()].copy_from_slice(compressed);
                Ok(compressed.len())
            }
            8 => decompress_slice_iter_to_slice(
                output,
                core::iter::once(&compressed[..]),
                false,
                true,
            )
            .map_err(|_| ZipError::Inflate),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    pub fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError> {
        if entry.uncompressed_size as usize > output.len() {
            return Err(ZipError::OutputTooSmall);
        }
        let (len, complete) =
            self.read_entry_prefix_streamed(entry, compressed_scratch, output, inflate_scratch)?;
        if complete {
            Ok(len)
        } else {
            Err(ZipError::OutputTooSmall)
        }
    }

    pub fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        let payload_offset = self.entry_payload_offset(entry)?;
        match entry.compression_method {
            0 => {
                let output_len = output.len().min(entry.uncompressed_size as usize);
                read_exact_at(&mut self.reader, payload_offset, &mut output[..output_len])?;
                Ok((output_len, output_len == entry.uncompressed_size as usize))
            }
            8 => self.inflate_entry_prefix(
                payload_offset,
                entry.compressed_size,
                compressed_scratch,
                output,
                inflate_scratch,
            ),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    /// Random-access counterpart to [`ZipLocalStream::read_entry_to_sink`].
    /// Inflates a zip entry chunk-by-chunk and forwards each chunk to `emit`
    /// without ever holding the full entry in RAM.
    pub fn read_entry_to_sink<F>(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        mut emit: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&[u8]) -> Result<(), ZipError>,
    {
        // See `ZipLocalStream::read_entry_to_sink` for why zero is rejected.
        if output_window.is_empty() {
            return Err(ZipError::OutputTooSmall);
        }
        let payload_offset = self.entry_payload_offset(entry)?;
        match entry.compression_method {
            0 => {
                let total = entry.uncompressed_size as usize;
                let mut written = 0usize;
                while written < total {
                    let take = (total - written).min(output_window.len());
                    read_exact_at(
                        &mut self.reader,
                        payload_offset + written as u32,
                        &mut output_window[..take],
                    )?;
                    emit(&output_window[..take])?;
                    written += take;
                }
                Ok(())
            }
            8 => self.inflate_entry_to_sink(
                payload_offset,
                entry.compressed_size,
                compressed_scratch,
                output_window.len(),
                inflate_scratch,
                emit,
            ),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    fn inflate_entry_to_sink<F>(
        &mut self,
        payload_offset: u32,
        compressed_size: u32,
        input: &mut [u8],
        emit_limit: usize,
        inflate_scratch: &mut ZipInflateScratch,
        emit: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&[u8]) -> Result<(), ZipError>,
    {
        let reader = &mut self.reader;
        inflate_chunks_to_sink(
            |fetched, buf| read_exact_at(reader, payload_offset + fetched, buf),
            compressed_size,
            input,
            emit_limit,
            inflate_scratch,
            emit,
        )
        .map(|_| ())
    }

    fn inflate_entry_prefix(
        &mut self,
        payload_offset: u32,
        compressed_size: u32,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        let reader = &mut self.reader;
        inflate_chunks_prefix(
            |fetched, buf| read_exact_at(reader, payload_offset + fetched, buf),
            compressed_size,
            input,
            output,
            inflate_scratch,
        )
        .map(|(len, complete, _)| (len, complete))
    }

    fn entry_payload_offset(&mut self, entry: OwnedZipEntry) -> Result<u32, ZipError> {
        let mut header = [0u8; 30];
        read_exact_at(&mut self.reader, entry.local_header_offset, &mut header)?;
        if read_u32(&header, 0)? != 0x0403_4b50 {
            return Err(ZipError::BadLocalHeader);
        }
        let name_len = read_u16(&header, 26)? as u32;
        let extra_len = read_u16(&header, 28)? as u32;
        entry
            .local_header_offset
            .checked_add(30 + name_len + extra_len)
            .ok_or(ZipError::BadLocalHeader)
    }
}

/// Narrow zip-entry interface shared by the EPUB loaders: find an entry by
/// name, then read it whole, as a bounded prefix, or streamed into a sink.
/// Both zip front-ends implement it so cache-building code does not care
/// whether compressed bytes come from random-access or forward-only storage.
pub trait EpubZipOps {
    /// Forward-only readers cannot revisit an entry once it has been passed.
    fn is_forward_only(&self) -> bool {
        false
    }

    fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError>;

    fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError>;

    fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError>;

    fn read_entry_to_sink(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        emit: &mut dyn FnMut(&[u8]) -> Result<(), ZipError>,
    ) -> Result<(), ZipError>;
}

impl<R> EpubZipOps for ZipStream<'_, R>
where
    R: ReadAt,
{
    fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        ZipStream::find_entry(self, name, header_scratch, name_scratch)
    }

    fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError> {
        ZipStream::read_entry_streamed(self, entry, compressed_scratch, output, inflate_scratch)
    }

    fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        ZipStream::read_entry_prefix_streamed(
            self,
            entry,
            compressed_scratch,
            output,
            inflate_scratch,
        )
    }

    fn read_entry_to_sink(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        emit: &mut dyn FnMut(&[u8]) -> Result<(), ZipError>,
    ) -> Result<(), ZipError> {
        ZipStream::read_entry_to_sink(
            self,
            entry,
            compressed_scratch,
            output_window,
            inflate_scratch,
            emit,
        )
    }
}

impl<R> EpubZipOps for ZipLocalStream<R>
where
    R: ByteStream,
{
    fn is_forward_only(&self) -> bool {
        true
    }

    fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        ZipLocalStream::find_entry(self, name, header_scratch, name_scratch)
    }

    fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError> {
        ZipLocalStream::read_entry_streamed(
            self,
            entry,
            compressed_scratch,
            output,
            inflate_scratch,
        )
    }

    fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        ZipLocalStream::read_entry_prefix_streamed(
            self,
            entry,
            compressed_scratch,
            output,
            inflate_scratch,
        )
    }

    fn read_entry_to_sink(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        emit: &mut dyn FnMut(&[u8]) -> Result<(), ZipError>,
    ) -> Result<(), ZipError> {
        ZipLocalStream::read_entry_to_sink(
            self,
            entry,
            compressed_scratch,
            output_window,
            inflate_scratch,
            emit,
        )
    }
}

/// Streaming inflate engine shared by both zip front-ends.
///
/// `fetch` fills `buf` with compressed payload bytes starting at the given
/// payload-relative offset; the engine only moves forward, so sequential
/// readers can ignore the offset.
///
/// Decoding runs through `inflate::core::decompress`, whose output buffer
/// *is* the LZ77 window: a power-of-two ring the decoder both writes to and
/// resolves back-references against. That is why the scratch owns a 32 KiB
/// ring rather than decoding into the caller's buffer — the caller's buffers
/// run 4-24 KB, well under the 32 KiB a match may reach back, so they cannot
/// serve as the window.
///
/// A decoded run is therefore always one contiguous slice of the ring,
/// never a wrapped pair: the decoder clamps its own write limit to the end
/// of the slice it is given, so a single call stops at the ring's physical
/// end and the next resumes at zero. The engine relies on that — it hands
/// `emit` a plain `&window[out_pos..]` subslice.
///
/// `emit_limit` caps how much one call may decode, and so bounds every slice
/// `emit` sees. The ring stays a full 32 KiB behind it: that is what lets a
/// match reach 32 KiB back while the caller still gets its chunks in the size
/// it asked for.
///
/// Returns the number of compressed bytes fetched, which sequential callers
/// use to restore their cursor.
fn inflate_chunks_to_sink<F, E>(
    mut fetch: F,
    compressed_size: u32,
    input: &mut [u8],
    emit_limit: usize,
    inflate_scratch: &mut ZipInflateScratch,
    mut emit: E,
) -> Result<u32, ZipError>
where
    F: FnMut(u32, &mut [u8]) -> Result<(), ZipError>,
    E: FnMut(&[u8]) -> Result<(), ZipError>,
{
    if input.is_empty() || emit_limit == 0 {
        return Err(ZipError::OutputTooSmall);
    }
    if compressed_size == 0 {
        return Ok(0);
    }
    // A scratch nobody prepared has no decoder; see `ZipInflateScratch`.
    let (decompressor, window, window_pos) = inflate_scratch.begin().ok_or(ZipError::Inflate)?;

    let mut compressed_read = 0u32;
    loop {
        let input_len = input
            .len()
            .min((compressed_size - compressed_read) as usize);
        if input_len > 0 {
            fetch(compressed_read, &mut input[..input_len])?;
            compressed_read += input_len as u32;
        }
        // Withholding this flag on the final chunk is what turns a truncated
        // stream into an error instead of a wait for input that never comes.
        let input_flags = if compressed_read < compressed_size {
            TINFL_FLAG_HAS_MORE_INPUT
        } else {
            0
        };
        let mut consumed = 0usize;
        loop {
            let out_pos = *window_pos & INFLATE_WINDOW_MASK;
            let (status, used, written) = decompress_with_limit(
                decompressor,
                &input[consumed..input_len],
                window,
                out_pos,
                emit_limit,
                input_flags | window_flags(*window_pos),
            );
            consumed += used;
            if written > 0 {
                emit(&window[out_pos..out_pos + written])?;
                *window_pos += written;
            }
            match status {
                TINFLStatus::Done => return Ok(compressed_read),
                // The emit limit was reached, or this call hit the ring's
                // end: go again at the advanced (possibly wrapped) position.
                TINFLStatus::HasMoreOutput => {
                    if used == 0 && written == 0 {
                        return Err(ZipError::Inflate);
                    }
                }
                TINFLStatus::NeedsMoreInput => break,
                _ => return Err(ZipError::Inflate),
            }
        }
        if compressed_read == compressed_size {
            // Only reachable if the decoder asked for input on the chunk that
            // carried the last byte, which it cannot: the flag was withheld.
            return Err(ZipError::Inflate);
        }
    }
}

/// Prefix variant of [`inflate_chunks_to_sink`]: decodes into `output` until
/// it fills, returning the decoded length, whether the whole entry fit, and
/// the number of compressed bytes fetched.
///
/// Where the sink variant limits each decode to the caller's chunk size, this
/// one limits it to the space left in `output`, so the ring never runs ahead
/// of what has been copied out of it. The ring stays a full 32 KiB regardless,
/// which is what lets a match reach further back than `output` is long.
fn inflate_chunks_prefix<F>(
    mut fetch: F,
    compressed_size: u32,
    input: &mut [u8],
    output: &mut [u8],
    inflate_scratch: &mut ZipInflateScratch,
) -> Result<(usize, bool, u32), ZipError>
where
    F: FnMut(u32, &mut [u8]) -> Result<(), ZipError>,
{
    if input.is_empty() || output.is_empty() {
        return Err(ZipError::OutputTooSmall);
    }
    if compressed_size == 0 {
        return Ok((0, true, 0));
    }
    let (decompressor, window, window_pos) = inflate_scratch.begin().ok_or(ZipError::Inflate)?;

    let mut compressed_read = 0u32;
    let mut output_pos = 0usize;
    loop {
        let input_len = input
            .len()
            .min((compressed_size - compressed_read) as usize);
        if input_len > 0 {
            fetch(compressed_read, &mut input[..input_len])?;
            compressed_read += input_len as u32;
        }
        let input_flags = if compressed_read < compressed_size {
            TINFL_FLAG_HAS_MORE_INPUT
        } else {
            0
        };
        let mut consumed = 0usize;
        loop {
            if output_pos == output.len() {
                // The entry holds more than the prefix can take. Asking for
                // another byte here would cap `out_max` at zero and spin.
                return Ok((output_pos, false, compressed_read));
            }
            let out_pos = *window_pos & INFLATE_WINDOW_MASK;
            let out_max = (INFLATE_WINDOW_BYTES - out_pos).min(output.len() - output_pos);
            let (status, used, written) = decompress_with_limit(
                decompressor,
                &input[consumed..input_len],
                window,
                out_pos,
                out_max,
                input_flags | window_flags(*window_pos),
            );
            consumed += used;
            if written > 0 {
                output[output_pos..output_pos + written]
                    .copy_from_slice(&window[out_pos..out_pos + written]);
                output_pos += written;
                *window_pos += written;
            }
            match status {
                TINFLStatus::Done => return Ok((output_pos, true, compressed_read)),
                TINFLStatus::HasMoreOutput => {
                    if used == 0 && written == 0 && output_pos < output.len() {
                        return Err(ZipError::Inflate);
                    }
                }
                TINFLStatus::NeedsMoreInput => break,
                _ => return Err(ZipError::Inflate),
            }
        }
        if compressed_read == compressed_size {
            return Err(ZipError::Inflate);
        }
    }
}

/// One sequential pass over a central directory too large for the tail
/// scratch, emitting bounded `(name hash, entry)` records into `scratch`.
/// Returns the encoded records and whether every entry fit. Sequential
/// reads are the access pattern storage likes; afterwards each lookup is
/// a RAM scan plus one local-header verification read, instead of a
/// per-lookup walk of the whole directory on storage.
fn build_central_index<'a, R>(
    reader: &mut R,
    central_offset: u32,
    entry_count: u16,
    scratch: &'a mut [u8],
) -> Result<(&'a [u8], bool), ZipError>
where
    R: ReadAt,
{
    let capacity = scratch.len() / CENTRAL_INDEX_RECORD_BYTES;
    let indexed = (entry_count as usize).min(capacity);
    let mut cursor = central_offset;
    let mut header = [0u8; 46];
    let mut name_chunk = [0u8; 64];
    for slot in 0..indexed {
        read_exact_at(reader, cursor, &mut header)?;
        if read_u32(&header, 0)? != 0x0201_4b50 {
            return Err(ZipError::BadCentralDirectory);
        }
        let compression_method = read_u16(&header, 10)?;
        let compressed_size = read_u32(&header, 20)?;
        let uncompressed_size = read_u32(&header, 24)?;
        let name_len = read_u16(&header, 28)? as u32;
        let extra_len = read_u16(&header, 30)? as u32;
        let comment_len = read_u16(&header, 32)? as u32;
        let local_header_offset = read_u32(&header, 42)?;

        let mut hash = FNV_OFFSET;
        let mut name_cursor = cursor
            .checked_add(46)
            .ok_or(ZipError::BadCentralDirectory)?;
        let mut remaining = name_len;
        while remaining > 0 {
            let take = (remaining as usize).min(name_chunk.len());
            read_exact_at(reader, name_cursor, &mut name_chunk[..take])?;
            hash = fnv1a_update(hash, &name_chunk[..take]);
            name_cursor = name_cursor
                .checked_add(take as u32)
                .ok_or(ZipError::BadCentralDirectory)?;
            remaining -= take as u32;
        }

        let record = &mut scratch
            [slot * CENTRAL_INDEX_RECORD_BYTES..(slot + 1) * CENTRAL_INDEX_RECORD_BYTES];
        record[0..4].copy_from_slice(&hash.to_le_bytes());
        record[4..8].copy_from_slice(&local_header_offset.to_le_bytes());
        record[8..12].copy_from_slice(&compressed_size.to_le_bytes());
        record[12..16].copy_from_slice(&uncompressed_size.to_le_bytes());
        record[16..18].copy_from_slice(&compression_method.to_le_bytes());
        record[18..20].copy_from_slice(&[0u8; 2]);

        cursor = cursor
            .checked_add(46)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(extra_len))
            .and_then(|value| value.checked_add(comment_len))
            .ok_or(ZipError::BadCentralDirectory)?;
    }
    Ok((
        &scratch[..indexed * CENTRAL_INDEX_RECORD_BYTES],
        indexed == entry_count as usize,
    ))
}

fn find_entry_in_central_cache(
    central: &[u8],
    entry_count: u16,
    name: &str,
) -> Result<OwnedZipEntry, ZipError> {
    let mut cursor = 0usize;
    for _ in 0..entry_count {
        if read_u32(central, cursor)? != 0x0201_4b50 {
            return Err(ZipError::BadCentralDirectory);
        }
        let compression_method = read_u16(central, cursor + 10)?;
        let compressed_size = read_u32(central, cursor + 20)?;
        let uncompressed_size = read_u32(central, cursor + 24)?;
        let name_len = read_u16(central, cursor + 28)? as usize;
        let extra_len = read_u16(central, cursor + 30)? as usize;
        let comment_len = read_u16(central, cursor + 32)? as usize;
        let local_header_offset = read_u32(central, cursor + 42)?;

        let name_start = cursor
            .checked_add(46)
            .ok_or(ZipError::BadCentralDirectory)?;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or(ZipError::BadCentralDirectory)?;
        let entry_name = central
            .get(name_start..name_end)
            .ok_or(ZipError::BadCentralDirectory)?;
        if entry_name == name.as_bytes() {
            return Ok(OwnedZipEntry {
                compression_method,
                compressed_size,
                uncompressed_size,
                local_header_offset,
            });
        }

        cursor = name_end
            .checked_add(extra_len)
            .and_then(|value| value.checked_add(comment_len))
            .ok_or(ZipError::BadCentralDirectory)?;
    }
    Err(ZipError::EntryNotFound)
}

pub struct ZipArchive<'a> {
    bytes: &'a [u8],
    central_offset: usize,
    entry_count: usize,
}

impl<'a> ZipArchive<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, ZipError> {
        let eocd = find_eocd(bytes).ok_or(ZipError::MissingEndOfCentralDirectory)?;
        if eocd + 22 > bytes.len() {
            return Err(ZipError::BadCentralDirectory);
        }
        let entry_count = read_u16(bytes, eocd + 10)? as usize;
        let central_size = read_u32(bytes, eocd + 12)? as usize;
        let central_offset = read_u32(bytes, eocd + 16)? as usize;
        if central_offset
            .checked_add(central_size)
            .filter(|end| *end <= bytes.len())
            .is_none()
        {
            return Err(ZipError::BadCentralDirectory);
        }
        Ok(Self {
            bytes,
            central_offset,
            entry_count,
        })
    }

    pub fn entries(&self) -> ZipEntries<'a> {
        ZipEntries {
            bytes: self.bytes,
            cursor: self.central_offset,
            remaining: self.entry_count,
        }
    }

    pub fn find(&self, name: &str) -> Result<ZipEntry<'a>, ZipError> {
        self.entries()
            .find(|entry| entry.map(|entry| entry.name == name).unwrap_or(false))
            .ok_or(ZipError::EntryNotFound)?
    }

    pub fn read_entry(&self, entry: ZipEntry<'a>, output: &mut [u8]) -> Result<usize, ZipError> {
        let compressed = self.entry_payload(entry)?;
        match entry.compression_method {
            0 => {
                if output.len() < compressed.len() {
                    return Err(ZipError::OutputTooSmall);
                }
                output[..compressed.len()].copy_from_slice(compressed);
                Ok(compressed.len())
            }
            8 => decompress_slice_iter_to_slice(output, core::iter::once(compressed), false, true)
                .map_err(|_| ZipError::Inflate),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    fn entry_payload(&self, entry: ZipEntry<'a>) -> Result<&'a [u8], ZipError> {
        let offset = entry.local_header_offset as usize;
        if read_u32(self.bytes, offset)? != 0x0403_4b50 {
            return Err(ZipError::BadLocalHeader);
        }
        let name_len = read_u16(self.bytes, offset + 26)? as usize;
        let extra_len = read_u16(self.bytes, offset + 28)? as usize;
        let start = offset
            .checked_add(30)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(extra_len))
            .ok_or(ZipError::BadLocalHeader)?;
        let end = start
            .checked_add(entry.compressed_size as usize)
            .ok_or(ZipError::BadLocalHeader)?;
        self.bytes.get(start..end).ok_or(ZipError::BadLocalHeader)
    }
}

pub struct ZipEntries<'a> {
    bytes: &'a [u8],
    cursor: usize,
    remaining: usize,
}

impl<'a> Iterator for ZipEntries<'a> {
    type Item = Result<ZipEntry<'a>, ZipError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let result = parse_central_entry(self.bytes, self.cursor);
        if let Ok((entry, next_cursor)) = result {
            self.cursor = next_cursor;
            Some(Ok(entry))
        } else {
            self.remaining = 0;
            Some(result.map(|(entry, _)| entry))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpubError {
    Zip(ZipError),
    Utf8,
    MissingContainer,
    MissingOpfPath,
    MissingOpf,
    TooManySpineItems,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XhtmlError {
    TooManyRuns,
    NestingTooDeep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TocError {
    TooManyItems,
    NestingTooDeep,
}

pub trait XhtmlBlockSink {
    /// Take one block of a spine item's content.
    ///
    /// `logical_offset` is where this block starts in that item's logical
    /// content stream, the coordinate space `proto::anchor` defines. It is
    /// assigned here because this is the only place that sees blocks before
    /// anything wraps them: a sink that lays text out into lines cannot
    /// recover it afterwards, since its own offsets move with the font.
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: FontStyle,
        align: TextAlign,
        paragraph_end: bool,
        logical_offset: u32,
    ) -> Result<(), XhtmlError>;
}

pub trait EpubTocSink {
    fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError>;
}

pub const MAX_CSS_RULES: usize = 96;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssRule {
    pub selector: heapless::String<64>,
    pub align: TextAlign,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssRules {
    pub rules: Vec<CssRule, MAX_CSS_RULES>,
}

impl CssRules {
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn clear(&mut self) {
        self.rules.clear();
    }

    pub fn push_text_align(&mut self, selector: &str, align: TextAlign) {
        let selector = selector.trim();
        if !selector_is_supported(selector) {
            return;
        }
        let mut stored = heapless::String::<64>::new();
        if stored.push_str(selector).is_err() {
            return;
        }
        if let Some(rule) = self
            .rules
            .iter_mut()
            .find(|rule| rule.selector.as_str() == stored.as_str())
        {
            rule.align = align;
            return;
        }
        let _ = self.rules.push(CssRule {
            selector: stored,
            align,
        });
    }

    pub fn alignment_for(&self, tag: &str) -> Option<TextAlign> {
        let tag_name = tag_local_name(tag)?;
        let mut result = self.align_for_selector(tag_name);
        if let Some(classes) = attr_value(tag, "class") {
            for class in classes.split_ascii_whitespace() {
                let mut selector = heapless::String::<64>::new();
                if selector.push('.').is_ok() && selector.push_str(class).is_ok() {
                    result = self.align_for_selector(selector.as_str()).or(result);
                }
                selector.clear();
                if selector.push_str(tag_name).is_ok()
                    && selector.push('.').is_ok()
                    && selector.push_str(class).is_ok()
                {
                    result = self.align_for_selector(selector.as_str()).or(result);
                }
            }
        }
        result
    }

    fn align_for_selector(&self, selector: &str) -> Option<TextAlign> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.selector.as_str().eq_ignore_ascii_case(selector))
            .map(|rule| rule.align)
    }
}

impl Default for CssRules {
    fn default() -> Self {
        Self::new()
    }
}

impl From<ZipError> for EpubError {
    fn from(value: ZipError) -> Self {
        Self::Zip(value)
    }
}

/// A byte range inside the OPF text, resolved on demand. Spine and manifest
/// items hold spans instead of `&str` so they carry no lifetime and pack into
/// half the space -- letting the (transient, build-time) spine and manifest
/// tables hold a long book's chapters within the tight EPUB-open stack budget,
/// where fat `&str` items (8 bytes each on 32-bit) overflowed it. This mirrors
/// the offset+len encoding every stored record already uses (`TocRecord`,
/// `BlockRecord`, `SpineRecord`). The OPF is <= 16 KB, so `u16` always fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Span {
    pub off: u16,
    pub len: u16,
}

impl Span {
    /// The slice of `opf` this span indexes. Empty if the span is out of range
    /// (it never is for spans built from `opf` itself).
    pub fn of<'a>(&self, opf: &'a str) -> &'a str {
        let start = self.off as usize;
        opf.get(start..start + self.len as usize).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SpineItem {
    pub href: Span,
    pub media_type: Span,
    pub properties: Span,
}

pub struct EpubPackage<'a> {
    pub meta: BookMeta<'a>,
    /// The OPF text the spine/manifest spans index into; resolve with `Span::of`.
    pub opf_text: &'a str,
    pub opf_path: &'a str,
    pub text_reference_href: Option<&'a str>,
    /// The guide's `cover` page reference: an XHTML page that only frames
    /// the cover image (not the image itself, which is `cover_href`).
    pub cover_page_href: Option<&'a str>,
    pub nav_href: Option<&'a str>,
    pub ncx_href: Option<&'a str>,
    /// The cover image's href: the `cover-image` item, else an image whose id
    /// or href says cover.
    pub cover_href: Option<&'a str>,
    /// True when the OPF held more spine items than `MAX_SPINE_ITEMS`, so the
    /// spine (and the book) is clipped. Callers flag the book partial instead
    /// of silently dropping the tail chapters.
    pub spine_truncated: bool,
    pub spine: Spine<'a>,
}

/// The package's spine, in reading order, as stored in the OPF arena: one
/// `SPINE_SLOT_BYTES` slot per item at the arena's top (item 0 highest), each
/// holding the item's href span and its interned media type and properties.
#[derive(Clone, Copy)]
pub struct Spine<'a> {
    /// The arena up to its top; slot `i` ends `i * SPINE_SLOT_BYTES` below.
    arena: &'a [u8],
    len: usize,
    interned: [Span; OPF_INTERNED_VALUES],
}

impl<'a> Spine<'a> {
    const fn empty() -> Self {
        Self {
            arena: &[],
            len: 0,
            interned: [Span { off: 0, len: 0 }; OPF_INTERNED_VALUES],
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Option<SpineItem> {
        if index >= self.len {
            return None;
        }
        let slot = SpineSlot::read(self.arena, index);
        let interned = |value: u8| {
            self.interned
                .get(value as usize)
                .copied()
                .unwrap_or_default()
        };
        Some(SpineItem {
            href: slot.href,
            media_type: interned(slot.media_type),
            properties: interned(slot.properties),
        })
    }

    pub fn first(&self) -> Option<SpineItem> {
        self.get(0)
    }

    pub fn iter(&self) -> impl Iterator<Item = SpineItem> + Clone + 'a {
        let spine = *self;
        (0..spine.len).filter_map(move |index| spine.get(index))
    }
}

impl<'a> EpubPackage<'a> {
    /// The href of a spine item, resolved against this package's OPF text.
    pub fn spine_href(&self, item: &SpineItem) -> &'a str {
        item.href.of(self.opf_text)
    }

    pub fn chapters(&self, output: &mut Vec<ChapterMeta<'a>, MAX_SPINE_ITEMS>) {
        output.clear();
        for (index, spine) in self.spine.iter().enumerate() {
            let href = spine.href.of(self.opf_text);
            let title = href.rsplit('/').next().unwrap_or(href);
            let _ = output.push(ChapterMeta {
                title,
                spine_index: index as u16,
                source_href: href,
            });
        }
    }
}

pub fn load_epub_package<'a>(
    epub_bytes: &'a [u8],
    container_scratch: &'a mut [u8],
    opf_scratch: &'a mut [u8],
    opf_arena: &'a mut [u8],
    book_id: BookId,
    source_path: &'a str,
) -> Result<EpubPackage<'a>, EpubError> {
    let zip = ZipArchive::new(epub_bytes)?;
    let container = zip.find("META-INF/container.xml")?;
    let container_len = zip.read_entry(container, container_scratch)?;
    let container_xml =
        core::str::from_utf8(&container_scratch[..container_len]).map_err(|_| EpubError::Utf8)?;
    let opf_path =
        find_attr_value(container_xml, "rootfile", "full-path").ok_or(EpubError::MissingOpfPath)?;

    let opf_entry = zip.find(opf_path).map_err(|_| EpubError::MissingOpf)?;
    let opf_len = zip.read_entry(opf_entry, opf_scratch)?;
    parse_opf(
        &opf_scratch[..opf_len],
        opf_arena,
        book_id,
        source_path,
        epub_bytes.len() as u32,
        opf_path,
    )
}

/// Helper to extract the OPF path from an EPUB's META-INF/container.xml entry
/// using the streaming Zip operations. Errors keep their nature — zip
/// failures stay `EpubError::Zip`, text and structure failures map to
/// `Utf8`/`MissingOpfPath` — matching `load_epub_package`'s handling of
/// the identical cases so callers surface the right diagnostics.
pub fn load_container_xml_and_find_opf_path<Z: EpubZipOps>(
    zip: &mut Z,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8],
    compressed_scratch: &mut [u8],
    container_scratch: &mut [u8],
    zip_inflate: &mut ZipInflateScratch,
    opf_path_buf: &mut heapless::String<256>,
) -> Result<(), EpubError> {
    let container_entry = zip
        .find_entry("META-INF/container.xml", header_scratch, name_scratch)
        .map_err(EpubError::Zip)?;
    let container_len = zip
        .read_entry_streamed(
            container_entry,
            compressed_scratch,
            container_scratch,
            zip_inflate,
        )
        .map_err(EpubError::Zip)?;
    let container_xml =
        core::str::from_utf8(&container_scratch[..container_len]).map_err(|_| EpubError::Utf8)?;
    let opf_path =
        find_attr_value(container_xml, "rootfile", "full-path").ok_or(EpubError::MissingOpfPath)?;
    opf_path_buf.clear();
    opf_path_buf
        .push_str(opf_path)
        .map_err(|_| EpubError::Zip(ZipError::NameTooLong))?;
    Ok(())
}

/// Parse an OPF held whole in memory. Thin wrapper over
/// [`parse_opf_streamed`], so slice and streamed callers share one parser.
pub fn parse_opf<'a>(
    opf_xml: &[u8],
    arena: &'a mut [u8],
    book_id: BookId,
    source_path: &'a str,
    byte_size: u32,
    opf_path: &'a str,
) -> Result<EpubPackage<'a>, EpubError> {
    parse_opf_streamed(
        |feed| feed(opf_xml),
        arena,
        book_id,
        source_path,
        byte_size,
        opf_path,
    )
}

/// Parse an OPF package document of any size in bounded memory.
///
/// `replay` streams the whole OPF (as decoded bytes, in any chunking) into
/// the callback it is given; it is called two or three times. The first
/// pass records which manifest ids the spine uses; the second keeps only
/// those manifest items (plus nav/NCX/cover) and resolves the spine; a
/// third runs only for OPFs whose spine resolves to nothing, falling back
/// to manifest order. Every string the package refers to is copied into
/// `arena` (`EpubPackage::opf_text` then views it), so the OPF itself never
/// has to fit in RAM. A spine longer than `MAX_SPINE_ITEMS` or an arena
/// that runs out marks the package `spine_truncated` instead of silently
/// dropping chapters.
///
/// Never inlined: its tokenizer, spine filter and manifest table are
/// several KB, and must be popped before the caller streams chapters on the
/// deepest stack path of the EPUB build.
#[inline(never)]
pub fn parse_opf_streamed<'a, R>(
    mut replay: R,
    arena: &'a mut [u8],
    book_id: BookId,
    source_path: &'a str,
    byte_size: u32,
    opf_path: &'a str,
) -> Result<EpubPackage<'a>, EpubError>
where
    R: FnMut(&mut dyn FnMut(&[u8]) -> Result<(), EpubError>) -> Result<(), EpubError>,
{
    // Pass 1: one slot per itemref, in spine order, holding its idref's hash
    // until the manifest item it names is found.
    let top = arena.len().min(u16::MAX as usize);
    let capacity = MAX_SPINE_ITEMS.min(top / (SPINE_SLOT_BYTES + SPINE_MIN_HREF_ROOM));
    let mut slots = 0usize;
    let mut spine_overflow = false;
    let mut tokenizer = StreamingXmlTokenizer::new();
    {
        let mut in_spine = false;
        let arena = &mut *arena;
        replay(&mut |chunk| {
            tokenizer.feed(chunk, &mut |event| {
                match event {
                    TokEvent::StartTag(tag) if tag_name_is(tag, "spine") => in_spine = true,
                    TokEvent::EndTag(tag) if tag_name_is(tag, "spine") => in_spine = false,
                    TokEvent::StartTag(tag) if in_spine && tag_name_is(tag, "itemref") => {
                        if let Some(idref) = attr_value(tag, "idref") {
                            if slots == capacity {
                                spine_overflow = true;
                            } else {
                                SpineSlot::unresolved(id_hash48(idref)).write(arena, slots);
                                slots += 1;
                            }
                        }
                    }
                    _ => {}
                }
                Ok::<(), EpubError>(())
            })
        })?;
        if in_spine {
            spine_overflow = true;
        }
    }

    let mut package = EpubPackage {
        meta: BookMeta {
            id: book_id,
            title: "Untitled",
            author: "Unknown Author",
            source_path,
            byte_size,
            source: BookSource::MicroSd,
            cover_status: CoverStatus::Missing,
        },
        opf_text: "",
        opf_path,
        text_reference_href: None,
        cover_page_href: None,
        nav_href: None,
        ncx_href: None,
        cover_href: None,
        spine_truncated: false,
        spine: Spine::empty(),
    };

    let summary = {
        let mut collector = OpfCollector::new(arena, slots);
        for fallback in [false, true] {
            if fallback && collector.resolved_slots() > 0 {
                break;
            }
            collector.begin_pass(fallback);
            tokenizer = StreamingXmlTokenizer::new();
            replay(&mut |chunk| {
                tokenizer.feed(chunk, &mut |event| {
                    collector.handle(event);
                    Ok::<(), EpubError>(())
                })
            })?;
            tokenizer.finish(&mut |event| {
                collector.handle(event);
                Ok::<(), EpubError>(())
            })?;
        }
        collector.compact_slots();
        collector.summary()
    };

    let arena: &'a [u8] = arena;
    package.spine = Spine {
        arena: &arena[..top],
        len: summary.slots,
        interned: summary.interned,
    };
    let text = core::str::from_utf8(&arena[..summary.len]).map_err(|_| EpubError::Utf8)?;
    package.opf_text = text;
    if let Some(title) = summary.title {
        package.meta.title = title.of(text);
    }
    if let Some(author) = summary.author {
        package.meta.author = author.of(text);
    }
    package.text_reference_href = summary
        .text_reference
        .or(summary.start_reference)
        .map(|span| strip_fragment(span.of(text)));
    package.cover_page_href = summary
        .cover_reference
        .map(|span| strip_fragment(span.of(text)));
    package.nav_href = summary.nav.map(|span| span.of(text));
    package.ncx_href = summary.ncx.map(|span| span.of(text));
    package.cover_href = summary.cover.map(|span| span.of(text));
    package.meta.cover_status = if summary.any_cover {
        CoverStatus::Present
    } else {
        CoverStatus::Missing
    };
    package.spine_truncated = spine_overflow || summary.lost;
    Ok(package)
}

/// Bytes a spine slot takes at the top of the OPF arena.
pub const SPINE_SLOT_BYTES: usize = 8;
/// Arena room kept per slot for its href, so a huge spine cannot crowd the
/// strings out entirely: items past it are flagged lost instead.
pub const SPINE_MIN_HREF_ROOM: usize = 16;
/// Marks a slot whose manifest item has not been seen yet.
const SLOT_UNRESOLVED: u8 = 0xFE;

/// 48 bits of FNV-1a 64 over a manifest id. Pass 1 keeps the itemrefs' ids
/// only as these; a manifest item claims every slot with its hash. Two of a
/// book's ids sharing one is a ~1e-9 chance.
fn id_hash48(id: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash & 0xFFFF_FFFF_FFFF
}

/// One spine item's slot. Unresolved, its first six bytes are the idref's
/// hash; resolved, the first four are the href span.
#[derive(Clone, Copy)]
struct SpineSlot {
    hash: u64,
    href: Span,
    media_type: u8,
    properties: u8,
}

impl SpineSlot {
    fn unresolved(hash: u64) -> Self {
        Self {
            hash,
            href: Span::default(),
            media_type: SLOT_UNRESOLVED,
            properties: SLOT_UNRESOLVED,
        }
    }

    fn is_resolved(&self) -> bool {
        self.media_type != SLOT_UNRESOLVED
    }

    fn at(arena_top: usize, index: usize) -> usize {
        arena_top - (index + 1) * SPINE_SLOT_BYTES
    }

    fn read(arena: &[u8], index: usize) -> Self {
        let top = arena.len().min(u16::MAX as usize);
        let at = Self::at(top, index);
        let bytes = &arena[at..at + SPINE_SLOT_BYTES];
        let media_type = bytes[6];
        let word = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        if media_type == SLOT_UNRESOLVED {
            let mut hash = [0u8; 8];
            hash[..6].copy_from_slice(&bytes[..6]);
            Self::unresolved(u64::from_le_bytes(hash))
        } else {
            Self {
                hash: 0,
                href: Span {
                    off: word(0),
                    len: word(2),
                },
                media_type,
                properties: bytes[7],
            }
        }
    }

    fn write(&self, arena: &mut [u8], index: usize) {
        let top = arena.len().min(u16::MAX as usize);
        let at = Self::at(top, index);
        let bytes = &mut arena[at..at + SPINE_SLOT_BYTES];
        if self.is_resolved() {
            bytes[0..2].copy_from_slice(&self.href.off.to_le_bytes());
            bytes[2..4].copy_from_slice(&self.href.len.to_le_bytes());
            bytes[4..6].fill(0);
        } else {
            bytes[..6].copy_from_slice(&self.hash.to_le_bytes()[..6]);
        }
        bytes[6] = self.media_type;
        bytes[7] = self.properties;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpfCapture {
    None,
    Title,
    Author,
}

struct OpfSummary {
    len: usize,
    slots: usize,
    interned: [Span; OPF_INTERNED_VALUES],
    title: Option<Span>,
    author: Option<Span>,
    text_reference: Option<Span>,
    start_reference: Option<Span>,
    cover_reference: Option<Span>,
    nav: Option<Span>,
    ncx: Option<Span>,
    cover: Option<Span>,
    any_cover: bool,
    lost: bool,
}

/// Strings grow up from the arena's bottom; the spine slots sit at its top.
/// Media types and properties repeat across items, so each distinct value is
/// stored once (up to this many) and slots refer to it by index.
const OPF_INTERNED_VALUES: usize = 8;
const OPF_NO_INTERNED: u8 = u8::MAX;

/// Second/third-pass state: copies the strings the package needs into the
/// arena and fills the spine slots pass 1 laid out, as the manifest items
/// they name stream by.
struct OpfCollector<'b> {
    arena: &'b mut [u8],
    len: usize,
    /// Spine slots in use; they occupy the arena's top.
    slots: usize,
    fallback: bool,
    in_manifest: bool,
    in_spine: bool,
    in_guide: bool,
    capture: OpfCapture,
    capture_text: heapless::String<256>,
    title: Option<Span>,
    author: Option<Span>,
    text_reference: Option<Span>,
    start_reference: Option<Span>,
    cover_reference: Option<Span>,
    nav: Option<Span>,
    ncx: Option<Span>,
    cover: Option<Span>,
    image_cover: Option<Span>,
    any_cover: bool,
    interned: Vec<Span, OPF_INTERNED_VALUES>,
    lost: bool,
}

impl<'b> OpfCollector<'b> {
    fn new(arena: &'b mut [u8], slots: usize) -> Self {
        Self {
            arena,
            len: 0,
            slots,
            fallback: false,
            in_manifest: false,
            in_spine: false,
            in_guide: false,
            capture: OpfCapture::None,
            capture_text: heapless::String::new(),
            title: None,
            author: None,
            text_reference: None,
            start_reference: None,
            cover_reference: None,
            nav: None,
            ncx: None,
            cover: None,
            image_cover: None,
            any_cover: false,
            interned: Vec::new(),
            lost: false,
        }
    }

    fn begin_pass(&mut self, fallback: bool) {
        self.fallback = fallback;
        if fallback {
            // Nothing resolved: the spine is rebuilt in manifest order.
            self.slots = 0;
        }
        self.in_manifest = false;
        self.in_spine = false;
        self.in_guide = false;
        self.capture = OpfCapture::None;
    }

    fn summary(&self) -> OpfSummary {
        let mut interned = [Span::default(); OPF_INTERNED_VALUES];
        interned[..self.interned.len()].copy_from_slice(&self.interned);
        OpfSummary {
            len: self.len,
            slots: self.slots,
            interned,
            title: self.title,
            author: self.author,
            text_reference: self.text_reference,
            start_reference: self.start_reference,
            cover_reference: self.cover_reference,
            nav: self.nav,
            ncx: self.ncx,
            cover: self.cover.or(self.image_cover),
            any_cover: self.any_cover,
            lost: self.lost,
        }
    }

    fn text(&self, span: Span) -> &str {
        let start = span.off as usize;
        core::str::from_utf8(&self.arena[start..start + span.len as usize]).unwrap_or("")
    }

    fn top(&self) -> usize {
        self.arena.len().min(u16::MAX as usize)
    }

    fn capacity(&self) -> usize {
        MAX_SPINE_ITEMS.min(self.top() / (SPINE_SLOT_BYTES + SPINE_MIN_HREF_ROOM))
    }

    /// Where the spine slots begin; strings must stay below it.
    fn slots_start(&self) -> usize {
        self.top() - self.slots * SPINE_SLOT_BYTES
    }

    fn push(&mut self, text: &str) -> Option<Span> {
        let end = self.len + text.len();
        if end > self.slots_start() {
            self.lost = true;
            return None;
        }
        self.arena[self.len..end].copy_from_slice(text.as_bytes());
        let span = Span {
            off: self.len as u16,
            len: text.len() as u16,
        };
        self.len = end;
        Some(span)
    }

    /// Media types and properties repeat across every item; store each
    /// distinct value once and refer to it by index.
    fn intern(&mut self, text: &str) -> Option<u8> {
        if text.is_empty() {
            return Some(OPF_NO_INTERNED);
        }
        if let Some(index) = self
            .interned
            .iter()
            .position(|span| self.text(*span) == text)
        {
            return Some(index as u8);
        }
        if self.interned.is_full() {
            return Some(OPF_NO_INTERNED);
        }
        let span = self.push(text)?;
        let _ = self.interned.push(span);
        Some((self.interned.len() - 1) as u8)
    }

    fn resolved_slots(&self) -> usize {
        (0..self.slots)
            .filter(|index| SpineSlot::read(self.arena, *index).is_resolved())
            .count()
    }

    /// Drops slots no manifest item filled (an itemref naming nothing),
    /// keeping spine order.
    fn compact_slots(&mut self) {
        let mut kept = 0;
        for index in 0..self.slots {
            let slot = SpineSlot::read(self.arena, index);
            if slot.is_resolved() {
                slot.write(self.arena, kept);
                kept += 1;
            }
        }
        self.slots = kept;
    }

    /// Adds a resolved slot below the existing ones (the manifest-order
    /// fallback), if the strings leave room.
    fn push_slot(&mut self, slot: SpineSlot) -> bool {
        if self.slots >= self.capacity() || self.slots_start() < self.len + SPINE_SLOT_BYTES {
            self.lost = true;
            return false;
        }
        self.slots += 1;
        slot.write(self.arena, self.slots - 1);
        true
    }

    fn handle(&mut self, event: TokEvent<'_>) {
        match event {
            TokEvent::StartTag(tag) => {
                let self_closing = tag.ends_with('/');
                if tag_name_is(tag, "manifest") {
                    self.in_manifest = !self_closing;
                } else if tag_name_is(tag, "spine") {
                    self.in_spine = !self_closing;
                } else if tag_name_is(tag, "guide") {
                    self.in_guide = !self_closing;
                } else if self.in_manifest && tag_name_is(tag, "item") {
                    self.handle_item(tag);
                } else if self.in_guide && tag_name_is(tag, "reference") {
                    if !self.fallback {
                        self.handle_reference(tag);
                    }
                } else if !self_closing && !self.fallback {
                    if self.title.is_none() && tag_name_is(tag, "title") {
                        self.capture = OpfCapture::Title;
                        self.capture_text.clear();
                    } else if self.author.is_none() && tag_name_is(tag, "creator") {
                        self.capture = OpfCapture::Author;
                        self.capture_text.clear();
                    }
                }
            }
            TokEvent::EndTag(tag) => {
                if tag_name_is(tag, "manifest") {
                    self.in_manifest = false;
                } else if tag_name_is(tag, "spine") {
                    self.in_spine = false;
                } else if tag_name_is(tag, "guide") {
                    self.in_guide = false;
                } else if self.capture != OpfCapture::None
                    && (tag_name_is(tag, "title") || tag_name_is(tag, "creator"))
                {
                    let mut value = heapless::String::<256>::new();
                    let _ = value.push_str(self.capture_text.as_str().trim());
                    let span = if value.is_empty() {
                        None
                    } else {
                        self.push(value.as_str())
                    };
                    match self.capture {
                        OpfCapture::Title => self.title = span,
                        OpfCapture::Author => self.author = span,
                        OpfCapture::None => {}
                    }
                    self.capture = OpfCapture::None;
                }
            }
            TokEvent::Text(text) => {
                if self.capture != OpfCapture::None {
                    append_owned_text(&mut self.capture_text, text);
                }
            }
            TokEvent::Cdata(text) => {
                if self.capture != OpfCapture::None {
                    append_owned_text_until_full_inner(&mut self.capture_text, text, false);
                }
            }
        }
    }

    fn handle_item(&mut self, tag: &str) {
        let Some(id) = attr_value(tag, "id") else {
            return;
        };
        let Some(href) = attr_value(tag, "href") else {
            return;
        };
        let media_type = attr_value(tag, "media-type").unwrap_or("");
        let properties = attr_value(tag, "properties").unwrap_or("");
        if self.fallback {
            if !manifest_item_is_reading_candidate(href, media_type, properties) {
                return;
            }
            let (Some(href), Some(media_type), Some(properties)) = (
                self.push(href),
                self.intern(media_type),
                self.intern(properties),
            ) else {
                return;
            };
            self.push_slot(SpineSlot {
                hash: 0,
                href,
                media_type,
                properties,
            });
            return;
        }
        let is_nav = properties
            .split_ascii_whitespace()
            .any(|prop| prop == "nav");
        let is_ncx =
            media_type.eq_ignore_ascii_case("application/x-dtbncx+xml") || href.ends_with(".ncx");
        let is_cover_image = properties
            .split_ascii_whitespace()
            .any(|prop| prop == "cover-image");
        let names_cover = id == "cover" || href.contains("cover");
        let is_image = media_type.starts_with("image/");
        if names_cover || is_cover_image {
            self.any_cover = true;
        }
        if is_nav && self.nav.is_none() {
            self.nav = self.push(href);
        }
        if is_ncx && self.ncx.is_none() {
            self.ncx = self.push(href);
        }
        if is_cover_image && self.cover.is_none() {
            self.cover = self.push(href);
        } else if is_image
            && self.image_cover.is_none()
            && (id.eq_ignore_ascii_case("cover") || href.to_ascii_lowercase_contains("cover"))
        {
            self.image_cover = self.push(href);
        }
        // Fill every spine slot that names this item (a spine may list an
        // item twice); its strings are stored once.
        let hash = id_hash48(id);
        let mut resolved: Option<SpineSlot> = None;
        for index in 0..self.slots {
            let slot = SpineSlot::read(self.arena, index);
            if slot.is_resolved() || slot.hash != hash {
                continue;
            }
            let fill = match resolved {
                Some(fill) => fill,
                None => {
                    let (Some(href), Some(media_type), Some(properties)) = (
                        self.push(href),
                        self.intern(media_type),
                        self.intern(properties),
                    ) else {
                        return;
                    };
                    let fill = SpineSlot {
                        hash: 0,
                        href,
                        media_type,
                        properties,
                    };
                    resolved = Some(fill);
                    fill
                }
            };
            fill.write(self.arena, index);
        }
    }

    fn handle_reference(&mut self, tag: &str) {
        let Some(href) = attr_value(tag, "href") else {
            return;
        };
        if self.text_reference.is_none() && attr_word_eq(tag, "type", "text") {
            self.text_reference = self.push(href);
        } else if self.start_reference.is_none() && attr_word_eq(tag, "type", "start") {
            self.start_reference = self.push(href);
        } else if self.cover_reference.is_none() && attr_word_eq(tag, "type", "cover") {
            self.cover_reference = self.push(href);
        }
    }
}

/// ASCII-case-insensitive substring test without allocating.
trait AsciiContains {
    fn to_ascii_lowercase_contains(&self, needle: &str) -> bool;
}

impl AsciiContains for str {
    fn to_ascii_lowercase_contains(&self, needle: &str) -> bool {
        let needle = needle.as_bytes();
        self.as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
    }
}

fn attr_word_eq(tag: &str, name: &str, word: &str) -> bool {
    attr_value(tag, name)
        .map(|value| {
            value
                .split_ascii_whitespace()
                .any(|value| value.eq_ignore_ascii_case(word))
        })
        .unwrap_or(false)
}

fn manifest_item_is_reading_candidate(href: &str, media_type: &str, properties: &str) -> bool {
    if href.is_empty()
        || properties
            .split_ascii_whitespace()
            .any(|prop| prop == "nav")
        || media_type.eq_ignore_ascii_case("application/x-dtbncx+xml")
        || href.ends_with(".ncx")
        || href.ends_with(".css")
    {
        return false;
    }
    media_type.eq_ignore_ascii_case("application/xhtml+xml")
        || href.ends_with(".xhtml")
        || href.ends_with(".html")
}
pub fn xhtml_text_blocks_with_css<const BLOCK_LEN: usize, const BLOCKS: usize>(
    xhtml: &str,
    css: Option<&CssRules>,
    output: &mut Vec<TextBlock<BLOCK_LEN>, BLOCKS>,
) -> Result<(), XhtmlError> {
    output.clear();
    struct VecSink<'a, const BLOCK_LEN: usize, const BLOCKS: usize> {
        output: &'a mut Vec<TextBlock<BLOCK_LEN>, BLOCKS>,
        continuation_open: bool,
        pending_space: bool,
    }
    impl<const BLOCK_LEN: usize, const BLOCKS: usize> XhtmlBlockSink
        for VecSink<'_, BLOCK_LEN, BLOCKS>
    {
        fn push_block(
            &mut self,
            text: &str,
            role: TextRole,
            style: FontStyle,
            align: TextAlign,
            paragraph_end: bool,
            _logical_offset: u32,
        ) -> Result<(), XhtmlError> {
            let mut rest = text;
            // Whether a space falls between the block before and the next
            // one this fragment spills into. The block before is trimmed, so
            // the space opens the next one instead.
            let mut space_before = false;
            if self.continuation_open {
                if let Some(last) = self.output.last_mut() {
                    if self.pending_space
                        && !text
                            .chars()
                            .next()
                            .map(|ch| is_html_space(ch) || is_leading_punctuation_char(ch))
                            .unwrap_or(true)
                    {
                        space_before = last.text.push(' ').is_err();
                    }
                    let used = append_text_until_full(&mut last.text, text);
                    rest = &text[used..];
                    space_before |= last.text.ends_with(' ') || rest.starts_with(is_html_space);
                    // Only the end: the block was trimmed when it was made,
                    // and a space it opens with is a split's separator.
                    while last.text.ends_with(' ') {
                        last.text.pop();
                    }
                }
            }
            // Whatever the last block had no room for, and a fragment longer
            // than one block, goes on in further blocks rather than being cut.
            let mut result = Ok(());
            while !rest.trim_matches(is_html_space).is_empty() {
                let mut owned = heapless::String::<BLOCK_LEN>::new();
                if space_before {
                    let _ = owned.push(' ');
                }
                let used = append_text_until_full(&mut owned, rest);
                if used == 0 {
                    // Not one character fits an empty block.
                    result = Err(XhtmlError::TooManyRuns);
                    break;
                }
                rest = &rest[used..];
                let space_after = owned.ends_with(' ') || rest.starts_with(is_html_space);
                if space_before {
                    while owned.len() > 1 && owned.ends_with(' ') {
                        owned.pop();
                    }
                } else {
                    trim_owned(&mut owned);
                }
                space_before = space_after;
                if owned.trim_matches(is_html_space).is_empty() {
                    continue;
                }
                result = push_owned_block(owned, role, style, align, self.output);
                if result.is_err() {
                    break;
                }
            }
            self.continuation_open = !paragraph_end;
            self.pending_space = text.chars().next_back().map(is_html_space).unwrap_or(false);
            result
        }
    }

    let mut sink = VecSink {
        output,
        continuation_open: false,
        pending_space: false,
    };
    xhtml_blocks_to_sink(xhtml, css, &mut sink)
}

pub fn xhtml_blocks_to_sink(
    xhtml: &str,
    css: Option<&CssRules>,
    sink: &mut impl XhtmlBlockSink,
) -> Result<(), XhtmlError> {
    let body_required = xhtml.contains("<body") || xhtml.contains(":body");
    let mut parser = XhtmlBlockStreamParser::new(!body_required);
    let mut cursor = XmlCursor::new(xhtml);
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) => parser.handle_start(tag, css, sink)?,
            Token::End(tag) => parser.handle_end(tag, sink)?,
            Token::Text(text) => parser.handle_text(text, sink)?,
            Token::Cdata(text) => parser.handle_cdata(text, sink)?,
        }
    }
    parser.finish(sink)
}

const SKIP_TAG_NAME_BYTES: usize = 24;

/// Resumable form of the XHTML block state machine: the one extraction
/// path behind both [`xhtml_blocks_to_sink`] (whole document in RAM) and
/// the streamed front-end on [`StreamingXmlTokenizer`], which lets
/// firmware decode spine members of any size through a bounded window.
pub struct XhtmlBlockStreamParser {
    block: heapless::String<384>,
    role: TextRole,
    align: TextAlign,
    bold_depth: u8,
    italic_depth: u8,
    in_body: bool,
    skip_depth: u8,
    skip_tag: Option<heapless::String<SKIP_TAG_NAME_BYTES>>,
    list_kind_stack: [ListKind; 8],
    list_count_stack: [u16; 8],
    list_depth: usize,
    li_depth: u8,
    table_align_stack: [TextAlign; 4],
    table_depth: usize,
    /// Bytes of this spine item's logical content stream already emitted.
    /// Reset by construction, and a parser is built per spine item, so an
    /// offset is always relative to the item a stored anchor names.
    stream_offset: u32,
}

/// Returns the next decoded or raw character from `rest` and the byte count to advance.
/// When `decode_entities` is true, HTML entities like `&amp;` or `&#x2014;` are decoded
/// and the advance encompasses the entity including its trailing semicolon if present.
fn next_text_char(rest: &str, decode_entities: bool) -> Option<(char, usize)> {
    if decode_entities {
        if let Some(decoded) = decode_html_entity(rest) {
            return Some((decoded, rest.find(';').map_or(1, |index| index + 1)));
        }
    }
    let ch = rest.chars().next()?;
    Some((ch, ch.len_utf8()))
}

impl XhtmlBlockStreamParser {
    /// `assume_in_body` mirrors the whole-document behavior for inputs
    /// without a `<body>` element: callers that cannot scan ahead decide
    /// from whatever prefix they have.
    pub fn new(assume_in_body: bool) -> Self {
        Self {
            block: heapless::String::new(),
            role: TextRole::Body,
            align: TextAlign::Justify,
            bold_depth: 0,
            italic_depth: 0,
            in_body: assume_in_body,
            skip_depth: 0,
            skip_tag: None,
            list_kind_stack: [ListKind::Unordered; 8],
            list_count_stack: [0u16; 8],
            list_depth: 0,
            li_depth: 0,
            table_align_stack: [TextAlign::Justify; 4],
            table_depth: 0,
            stream_offset: 0,
        }
    }

    /// Bytes of the logical content stream emitted so far, which is where the
    /// next block will start. Exposed for tests that check the coordinate
    /// space directly rather than through a sink.
    pub fn stream_offset(&self) -> u32 {
        self.stream_offset
    }

    pub fn handle_start(
        &mut self,
        tag: &str,
        css: Option<&CssRules>,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        if self.skip_depth > 0 {
            // Only a matching close ends this skipped subtree. Nested
            // elements with the same name need their own matching closes;
            // empty elements do not contribute to the outstanding depth.
            if self.skip_tag_matches(tag) && !tag_is_void(tag) {
                self.skip_depth = self
                    .skip_depth
                    .checked_add(1)
                    .ok_or(XhtmlError::NestingTooDeep)?;
            }
            return Ok(());
        }
        let self_closing = tag.trim_end().ends_with('/');
        if tag_name_is(tag, "body") {
            self.in_body = !self_closing;
            return Ok(());
        }
        if self.skip_depth == 0
            && (tag_name_is(tag, "head")
                || tag_name_is(tag, "style")
                || tag_name_is(tag, "script")
                || tag_name_is(tag, "svg")
                || tag_name_is(tag, "nav")
                || tag_is_hidden(tag)
                || tag_is_pagebreak(tag))
        {
            if !tag_is_void(tag) {
                self.skip_tag = tag_local_name(tag).map(bounded_skip_name);
                self.skip_depth = self.skip_depth.saturating_add(1);
            }
            return Ok(());
        }
        if !self.in_body || self.skip_depth > 0 {
            return Ok(());
        }
        let prev_role = self.role;
        let prev_align = self.align;
        let prev_bold_depth = self.bold_depth;
        let prev_italic_depth = self.italic_depth;
        let prev_table_depth = self.table_depth;
        let prev_list_depth = self.list_depth;
        let prev_li_depth = self.li_depth;
        if tag_name_is(tag, "table") {
            self.flush(sink)?;
            if self.table_depth < self.table_align_stack.len() {
                self.table_align_stack[self.table_depth] =
                    table_align_for_tag(tag, css).unwrap_or(TextAlign::Justify);
                self.table_depth += 1;
            }
        } else if tag_name_is(tag, "td") || tag_name_is(tag, "th") {
            append_table_cell_separator(&mut self.block);
        } else if tag_name_is(tag, "br") {
            self.flush(sink)?;
        } else if tag_name_is(tag, "hr") {
            self.flush(sink)?;
            push_block_at(
                sink,
                &mut self.stream_offset,
                "* * *",
                TextRole::Body,
                FontStyle::Regular,
                TextAlign::Center,
                true,
            )?;
        } else if tag_name_is(tag, "img") {
            self.flush(sink)?;
            let placeholder = attr_value(tag, "alt").unwrap_or("[Image]");
            push_block_at(
                sink,
                &mut self.stream_offset,
                placeholder,
                TextRole::Body,
                FontStyle::Italic,
                TextAlign::Center,
                true,
            )?;
        } else if tag_name_is(tag, "ul") || tag_name_is(tag, "ol") {
            self.flush(sink)?;
            if self.list_depth < self.list_kind_stack.len() {
                self.list_kind_stack[self.list_depth] = if tag_name_is(tag, "ol") {
                    ListKind::Ordered
                } else {
                    ListKind::Unordered
                };
                self.list_count_stack[self.list_depth] = 0;
                self.list_depth += 1;
            }
        } else if tag_starts_block(tag) {
            if tag_name_is(tag, "li") {
                self.flush(sink)?;
                self.li_depth = self.li_depth.saturating_add(1);
                append_list_marker(
                    &mut self.block,
                    &mut self.list_count_stack,
                    &self.list_kind_stack,
                    self.list_depth,
                );
            } else if !(self.li_depth > 0 && self.block_holds_only_list_marker()) {
                self.flush(sink)?;
            }
            self.align = block_align_for_tag(tag, css)
                .or_else(|| current_table_align(&self.table_align_stack, self.table_depth))
                .unwrap_or(TextAlign::Justify);
        } else if tag_name_is(tag, "h1") {
            self.flush(sink)?;
            self.role = TextRole::Heading1;
            self.align = TextAlign::Center;
            self.bold_depth = self.bold_depth.saturating_add(1);
        } else if tag_name_is(tag, "h2") {
            self.flush(sink)?;
            self.role = TextRole::Heading2;
            self.align = TextAlign::Center;
            self.bold_depth = self.bold_depth.saturating_add(1);
        } else if tag_name_is(tag, "h3")
            || tag_name_is(tag, "h4")
            || tag_name_is(tag, "h5")
            || tag_name_is(tag, "h6")
        {
            self.flush(sink)?;
            self.role = TextRole::Heading3;
            self.align = TextAlign::Center;
            self.bold_depth = self.bold_depth.saturating_add(1);
        } else if tag_name_is(tag, "blockquote") {
            self.flush(sink)?;
            self.role = TextRole::BlockQuote;
            self.align = block_align_for_tag(tag, css).unwrap_or(TextAlign::Left);
            self.italic_depth = self.italic_depth.saturating_add(1);
        } else if tag_name_is(tag, "strong") || tag_name_is(tag, "b") {
            self.flush_continue(sink)?;
            self.bold_depth = self.bold_depth.saturating_add(1);
        } else if tag_is_italic(tag) {
            self.flush_continue(sink)?;
            self.italic_depth = self.italic_depth.saturating_add(1);
        }
        // XML empty elements have both an opening and a closing effect.
        // Hidden/void skips returned above without opening any state, while
        // formatting, list, table and block starts here must be balanced.
        // A self-closing element's scope ends immediately, so enclosing
        // container state (role, alignment, table and list nesting, and inline
        // style depths) must not be discarded or underflow when synthetic
        // close runs.
        if self_closing {
            self.handle_end(tag, sink)?;
            self.table_depth = prev_table_depth;
            self.list_depth = prev_list_depth;
            self.li_depth = prev_li_depth;
            self.bold_depth = prev_bold_depth;
            self.italic_depth = prev_italic_depth;
            self.role = prev_role;
            self.align = prev_align;
        }
        Ok(())
    }

    pub fn handle_end(
        &mut self,
        tag: &str,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        if tag_name_is(tag, "body") {
            self.flush(sink)?;
            self.in_body = false;
            return Ok(());
        }
        if self.skip_tag_matches(tag) {
            self.skip_depth = self.skip_depth.saturating_sub(1);
            if self.skip_depth == 0 {
                self.skip_tag = None;
            }
            return Ok(());
        }
        if !self.in_body || self.skip_depth > 0 {
            return Ok(());
        }
        if tag_name_is(tag, "table") {
            self.flush(sink)?;
            self.table_depth = self.table_depth.saturating_sub(1);
        } else if tag_name_is(tag, "ul") || tag_name_is(tag, "ol") {
            self.flush(sink)?;
            self.list_depth = self.list_depth.saturating_sub(1);
        } else if tag_name_is(tag, "h1")
            || tag_name_is(tag, "h2")
            || tag_name_is(tag, "h3")
            || tag_name_is(tag, "h4")
            || tag_name_is(tag, "h5")
            || tag_name_is(tag, "h6")
        {
            self.flush(sink)?;
            self.role = TextRole::Body;
            self.bold_depth = self.bold_depth.saturating_sub(1);
            self.align = TextAlign::Justify;
        } else if tag_name_is(tag, "blockquote") {
            self.flush(sink)?;
            self.role = TextRole::Body;
            self.italic_depth = self.italic_depth.saturating_sub(1);
            self.align = TextAlign::Justify;
        } else if tag_name_is(tag, "strong") || tag_name_is(tag, "b") {
            self.flush_continue(sink)?;
            self.bold_depth = self.bold_depth.saturating_sub(1);
        } else if tag_is_italic(tag) {
            self.flush_continue(sink)?;
            self.italic_depth = self.italic_depth.saturating_sub(1);
        } else if tag_ends_block(tag) {
            if tag_name_is(tag, "li") {
                self.li_depth = self.li_depth.saturating_sub(1);
            }
            self.flush(sink)?;
            self.align = TextAlign::Justify;
        }
        Ok(())
    }

    pub fn handle_text(
        &mut self,
        text: &str,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        if !self.in_body || self.skip_depth > 0 {
            return Ok(());
        }
        self.append_text_to_sink(text, sink, true)
    }

    pub fn handle_cdata(
        &mut self,
        text: &str,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        if !self.in_body || self.skip_depth > 0 {
            return Ok(());
        }
        self.append_text_to_sink(text, sink, false)
    }

    fn append_text_to_sink(
        &mut self,
        text: &str,
        sink: &mut impl XhtmlBlockSink,
        decode_entities: bool,
    ) -> Result<(), XhtmlError> {
        let style = style_for(self.bold_depth, self.italic_depth);
        let mut previous_space = self
            .block
            .as_str()
            .chars()
            .last()
            .map(is_html_space)
            .unwrap_or(false);
        let mut cursor = 0usize;
        while cursor < text.len() {
            let rest = &text[cursor..];
            let Some((ch, advance)) = next_text_char(rest, decode_entities) else {
                break;
            };
            let should_push_space = is_html_space(ch) && !previous_space;
            let should_push_char = !is_html_space(ch);
            if should_push_space {
                if self.block.push(' ').is_err() {
                    flush_sink_block_at_word_boundary(
                        &mut self.block,
                        self.role,
                        style,
                        self.align,
                        sink,
                        &mut self.stream_offset,
                    )?;
                    if self.block.push(' ').is_err() {
                        flush_sink_block_continue(
                            &mut self.block,
                            self.role,
                            style,
                            self.align,
                            sink,
                            &mut self.stream_offset,
                        )?;
                        let _ = self.block.push(' ');
                    }
                }
                previous_space = true;
            } else if should_push_char {
                if self.block.push(ch).is_err() {
                    flush_sink_block_at_word_boundary(
                        &mut self.block,
                        self.role,
                        style,
                        self.align,
                        sink,
                        &mut self.stream_offset,
                    )?;
                    if self.block.push(ch).is_err() {
                        flush_sink_block_continue(
                            &mut self.block,
                            self.role,
                            style,
                            self.align,
                            sink,
                            &mut self.stream_offset,
                        )?;
                        if self.block.push(ch).is_err() {
                            break;
                        }
                    }
                }
                previous_space = false;
            }
            cursor += advance;
        }
        Ok(())
    }

    pub fn finish(&mut self, sink: &mut impl XhtmlBlockSink) -> Result<(), XhtmlError> {
        self.flush(sink)
    }

    fn flush(&mut self, sink: &mut impl XhtmlBlockSink) -> Result<(), XhtmlError> {
        flush_sink_block(
            &mut self.block,
            self.role,
            style_for(self.bold_depth, self.italic_depth),
            self.align,
            sink,
            &mut self.stream_offset,
        )
    }

    fn flush_continue(&mut self, sink: &mut impl XhtmlBlockSink) -> Result<(), XhtmlError> {
        flush_sink_block_continue(
            &mut self.block,
            self.role,
            style_for(self.bold_depth, self.italic_depth),
            self.align,
            sink,
            &mut self.stream_offset,
        )
    }

    fn skip_tag_matches(&self, tag: &str) -> bool {
        let Some(stored) = self.skip_tag.as_ref() else {
            return false;
        };
        tag_local_name(tag)
            .map(|name| bounded_skip_name(name).as_str() == stored.as_str())
            .unwrap_or(false)
    }

    fn block_holds_only_list_marker(&self) -> bool {
        let trimmed = self.block.as_str().trim_matches(is_html_space);
        if trimmed == "-" {
            return true;
        }
        if let Some(digits) = trimmed.strip_suffix('.') {
            return !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        }
        false
    }
}

/// Skip-tag names are stored owned (streaming input has no stable slice
/// to borrow); both the opening and closing side truncate identically so
/// overlong names still pair up.
fn bounded_skip_name(name: &str) -> heapless::String<SKIP_TAG_NAME_BYTES> {
    let mut out = heapless::String::new();
    for ch in name.chars() {
        if out.push(ch).is_err() {
            break;
        }
    }
    out
}

/// Whether a nav-document start tag opens a subtree whose links are not
/// table-of-contents entries. Both EPUB 3 nav parsers share it, so the
/// buffered and streamed paths agree on what a TOC holds.
fn nav_skip_starts(tag: &str) -> bool {
    tag_name_is(tag, "head")
        || tag_name_is(tag, "style")
        || tag_name_is(tag, "script")
        || tag_name_is(tag, "svg")
        || tag_is_hidden_from_view(tag)
}

pub fn parse_epub3_nav_to_sink(xhtml: &str, sink: &mut impl EpubTocSink) -> Result<(), TocError> {
    let mut cursor = XmlCursor::new(xhtml);
    let body_required = xhtml.contains("<body") || xhtml.contains(":body");
    let mut in_body = !body_required;
    let mut in_nav = false;
    let mut skip_depth = 0u8;
    let mut skip_tag: Option<&str> = None;
    let mut list_depth = 0u8;
    let mut href: Option<&str> = None;
    let mut title = heapless::String::<160>::new();
    let mut level = 0u8;

    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "body") => in_body = true,
            Token::End(tag) if tag_name_is(tag, "body") => in_body = false,
            Token::Start(tag) if skip_depth > 0 => {
                // Only a matching close ends this skipped subtree. Nested
                // elements with the same name need their own matching closes;
                // empty elements do not contribute to the outstanding depth.
                if skip_tag.is_some_and(|name| tag_name_is(tag, name)) && !tag_is_void(tag) {
                    skip_depth = skip_depth.checked_add(1).ok_or(TocError::NestingTooDeep)?;
                }
            }
            Token::Start(tag) if skip_depth == 0 && nav_skip_starts(tag) && !tag_is_void(tag) => {
                skip_tag = tag_local_name(tag);
                skip_depth = skip_depth.saturating_add(1);
            }
            Token::End(tag) if skip_tag.map(|name| tag_name_is(tag, name)).unwrap_or(false) => {
                skip_depth = skip_depth.saturating_sub(1);
                if skip_depth == 0 {
                    skip_tag = None;
                }
            }
            _ if !in_body || skip_depth > 0 => {}
            Token::Start(tag) if tag_name_is(tag, "nav") => in_nav = nav_start_is_toc(tag),
            Token::End(tag) if tag_name_is(tag, "nav") => in_nav = false,
            Token::Start(tag) if in_nav && tag_name_is(tag, "ol") => {
                list_depth = list_depth.saturating_add(1);
            }
            Token::End(tag) if in_nav && tag_name_is(tag, "ol") => {
                list_depth = list_depth.saturating_sub(1);
            }
            Token::Start(tag) if in_nav && tag_name_is(tag, "a") => {
                href = attr_value(tag, "href");
                title.clear();
                level = list_depth.max(1);
            }
            Token::Text(text) if href.is_some() => append_owned_text(&mut title, text),
            Token::Cdata(text) if href.is_some() => {
                append_text_until_full(&mut title, text);
            }
            Token::End(tag) if href.is_some() && tag_name_is(tag, "a") => {
                if let Some(found_href) = href.take() {
                    let text = title.as_str().trim_matches(is_html_space);
                    if !text.is_empty() && !found_href.is_empty() {
                        sink.push_toc(text, found_href, level)?;
                    }
                }
                title.clear();
            }
            _ => {}
        }
    }

    Ok(())
}

pub fn parse_epub2_ncx_to_sink(ncx: &str, sink: &mut impl EpubTocSink) -> Result<(), TocError> {
    let mut cursor = XmlCursor::new(ncx);
    let mut nav_depth = 0u8;
    let mut item_open = false;
    let mut item_pushed = false;
    let mut in_text = false;
    let mut title = heapless::String::<160>::new();
    let mut href: Option<&str> = None;

    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "navPoint") => {
                if item_open && !item_pushed {
                    push_pending_ncx_item(sink, title.as_str(), href, nav_depth)?;
                }
                nav_depth = nav_depth.saturating_add(1);
                item_open = true;
                item_pushed = false;
                in_text = false;
                title.clear();
                href = None;
            }
            Token::End(tag) if tag_name_is(tag, "navPoint") => {
                if item_open && !item_pushed {
                    push_pending_ncx_item(sink, title.as_str(), href, nav_depth)?;
                }
                nav_depth = nav_depth.saturating_sub(1);
                item_open = nav_depth > 0;
                item_pushed = false;
                in_text = false;
                title.clear();
                href = None;
            }
            Token::Start(tag) if item_open && tag_name_is(tag, "text") => in_text = true,
            Token::End(tag) if tag_name_is(tag, "text") => in_text = false,
            Token::Start(tag) if item_open && tag_name_is(tag, "content") => {
                href = attr_value(tag, "src");
                if !item_pushed && !title.as_str().trim_matches(is_html_space).is_empty() {
                    push_pending_ncx_item(sink, title.as_str(), href, nav_depth)?;
                    item_pushed = true;
                }
            }
            Token::Text(text) if in_text => append_owned_text(&mut title, text),
            Token::Cdata(text) if in_text => {
                append_text_until_full(&mut title, text);
            }
            _ => {}
        }
    }

    Ok(())
}

fn push_pending_ncx_item(
    sink: &mut impl EpubTocSink,
    title: &str,
    href: Option<&str>,
    level: u8,
) -> Result<(), TocError> {
    let title = title.trim_matches(is_html_space);
    let href = href.unwrap_or("").trim_matches(is_html_space);
    if title.is_empty() || href.is_empty() {
        return Ok(());
    }
    sink.push_toc(title, href, level.max(1))
}

// === Streaming TOC parsing ===
//
// The slice-based parsers above require the full NCX/nav file to fit in a
// single RAM buffer. For books with large tables of contents (HPMOR's NCX is
// 53 KB, well over the 24 KB XHTML scratch on the C3) that's a hard ceiling.
// The streaming variants below consume bytes incrementally from a `ByteStream`
// using a small XML tokenizer with bounded internal buffers, so peak memory is
// a function of the longest single tag/title rather than the file size.

const STREAM_TAG_BUF_BYTES: usize = 1024;
const STREAM_TEXT_CHUNK_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TocStreamError {
    Tokenizer,
    Toc(TocError),
}

impl From<TocError> for TocStreamError {
    fn from(value: TocError) -> Self {
        Self::Toc(value)
    }
}

#[derive(Clone, Copy)]
enum TokState {
    Text,
    Tag,
    TagQuoted(u8),
    Comment(u8), // Consecutive trailing dashes, capped at two.
    Cdata(u8),   // Consecutive trailing brackets, capped at two.
}

enum TokEvent<'a> {
    StartTag(&'a str),
    EndTag(&'a str),
    Text(&'a str),
    Cdata(&'a str),
}

/// Longest text suffix held back at a forced mid-text flush so an HTML
/// entity is never split across two Text events ("&#x10FFFF;" is 10
/// bytes; named entities top out around "&hellip;").
const STREAM_ENTITY_HOLDBACK_BYTES: usize = 12;

/// Byte-level XML tokenizer. Emits StartTag/EndTag/Text events with all
/// content held in bounded internal buffers — never borrows from the caller's
/// input. Comments are skipped through `-->`, including across input chunks;
/// their quotes and markup are not parsed. PI/doctype handling remains limited
/// to a single tag.
///
/// Text accumulates as raw bytes: a forced flush when the buffer fills
/// holds back incomplete UTF-8 sequences and incomplete entities, so the
/// emitted `&str` chunks are always valid UTF-8 and never split an
/// entity, regardless of how the input is chunked.
pub struct StreamingXmlTokenizer {
    state: TokState,
    /// The tag's bytes as they arrive, read as UTF-8 once the tag closes:
    /// pushed one byte per `char` they would come out Latin-1, and an
    /// `alt` or `href` with a character past ASCII would name something
    /// else.
    tag_buf: heapless::Vec<u8, STREAM_TAG_BUF_BYTES>,
    text_buf: heapless::Vec<u8, STREAM_TEXT_CHUNK_BYTES>,
    tag_overflow: bool,
}

impl Default for StreamingXmlTokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingXmlTokenizer {
    pub fn new() -> Self {
        Self {
            state: TokState::Text,
            tag_buf: heapless::Vec::new(),
            text_buf: heapless::Vec::new(),
            tag_overflow: false,
        }
    }

    pub fn feed_ncx(
        &mut self,
        bytes: &[u8],
        parser: &mut NcxStreamParser,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        self.feed(bytes, &mut |event| parser.handle(event, sink))
    }

    pub fn finish_ncx(
        &mut self,
        parser: &mut NcxStreamParser,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        self.finish(&mut |event| parser.handle(event, sink))
    }

    pub fn feed_nav(
        &mut self,
        bytes: &[u8],
        parser: &mut Epub3NavStreamParser,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        self.feed(bytes, &mut |event| parser.handle(event, sink))
    }

    pub fn finish_nav(
        &mut self,
        parser: &mut Epub3NavStreamParser,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        self.finish(&mut |event| parser.handle(event, sink))
    }

    pub fn feed_xhtml_blocks(
        &mut self,
        bytes: &[u8],
        parser: &mut XhtmlBlockStreamParser,
        css: Option<&CssRules>,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        self.feed(bytes, &mut |event| match event {
            TokEvent::StartTag(tag) => parser.handle_start(tag, css, sink),
            TokEvent::EndTag(tag) => parser.handle_end(tag, sink),
            TokEvent::Text(text) => parser.handle_text(text, sink),
            TokEvent::Cdata(text) => parser.handle_cdata(text, sink),
        })
    }

    pub fn finish_xhtml_blocks(
        &mut self,
        parser: &mut XhtmlBlockStreamParser,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        self.finish(&mut |event| match event {
            TokEvent::StartTag(_) | TokEvent::EndTag(_) => Ok(()),
            TokEvent::Text(text) => parser.handle_text(text, sink),
            TokEvent::Cdata(text) => parser.handle_cdata(text, sink),
        })?;
        parser.finish(sink)
    }

    fn feed<F, E>(&mut self, bytes: &[u8], emit: &mut F) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        for &byte in bytes {
            self.consume(byte, emit)?;
        }
        Ok(())
    }

    fn finish<F, E>(&mut self, emit: &mut F) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        if let TokState::Cdata(brackets) = self.state {
            for _ in 0..brackets {
                self.push_text_byte(b']', emit)?;
            }
            self.flush_text(emit, true)?;
        } else if matches!(self.state, TokState::Text) {
            self.flush_text(emit, true)?;
        }
        Ok(())
    }

    fn consume<F, E>(&mut self, byte: u8, emit: &mut F) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        match self.state {
            TokState::Text => {
                if byte == b'<' {
                    self.flush_text(emit, true)?;
                    self.state = TokState::Tag;
                    self.tag_buf.clear();
                    self.tag_overflow = false;
                } else {
                    self.push_text_byte(byte, emit)?;
                }
            }
            TokState::Tag => {
                if byte == b'>' {
                    if !self.tag_overflow {
                        let tag = match core::str::from_utf8(&self.tag_buf) {
                            Ok(tag) => tag,
                            Err(_) => {
                                self.drop_invalid_tag_bytes();
                                core::str::from_utf8(&self.tag_buf).unwrap_or_default()
                            }
                        };
                        let tag = tag.trim();
                        if !(tag.starts_with('!') || tag.starts_with('?')) {
                            if let Some(name) = tag.strip_prefix('/') {
                                emit(TokEvent::EndTag(name.trim()))?;
                            } else if !tag.is_empty() {
                                emit(TokEvent::StartTag(tag))?;
                            }
                        }
                    }
                    self.tag_buf.clear();
                    self.tag_overflow = false;
                    self.state = TokState::Text;
                } else if (byte == b'"' || byte == b'\'')
                    && !matches!(self.tag_buf.first(), Some(b'!' | b'?'))
                {
                    // Quotes in a comment or instruction are text, as in `tag_end`.
                    self.push_tag_byte(byte);
                    self.state = TokState::TagQuoted(byte);
                } else {
                    self.push_tag_byte(byte);
                    if self.tag_buf.as_slice() == b"!--" {
                        self.tag_buf.clear();
                        self.state = TokState::Comment(0);
                    } else if self.tag_buf.as_slice() == b"![CDATA[" {
                        self.tag_buf.clear();
                        self.state = TokState::Cdata(0);
                    }
                }
            }
            TokState::Comment(dashes) => {
                self.state = if byte == b'-' {
                    TokState::Comment((dashes + 1).min(2))
                } else if byte == b'>' && dashes == 2 {
                    TokState::Text
                } else {
                    TokState::Comment(0)
                };
            }
            TokState::Cdata(brackets) => {
                if byte == b']' {
                    if brackets == 2 {
                        self.push_text_byte(b']', emit)?;
                    }
                    self.state = TokState::Cdata((brackets + 1).min(2));
                } else if byte == b'>' && brackets == 2 {
                    self.flush_text(emit, true)?;
                    self.state = TokState::Text;
                } else {
                    for _ in 0..brackets {
                        self.push_text_byte(b']', emit)?;
                    }
                    self.push_text_byte(byte, emit)?;
                    self.state = TokState::Cdata(0);
                }
            }
            TokState::TagQuoted(quote) => {
                self.push_tag_byte(byte);
                if byte == quote {
                    self.state = TokState::Tag;
                }
            }
        }
        Ok(())
    }

    fn push_tag_byte(&mut self, byte: u8) {
        if self.tag_overflow {
            return;
        }
        if self.tag_buf.push(byte).is_err() {
            self.tag_overflow = true;
        }
    }

    /// Remove the bytes of a tag that are not UTF-8, as text skips them, so
    /// the element and its other attributes still read. Dropping the whole
    /// tag would lose a heading's role, or let a page-break marker's number
    /// into the text.
    fn drop_invalid_tag_bytes(&mut self) {
        let mut kept = 0usize;
        let mut at = 0usize;
        while let Some(rest) = self.tag_buf.get(at..).filter(|rest| !rest.is_empty()) {
            let (valid, invalid) = match core::str::from_utf8(rest) {
                Ok(_) => (rest.len(), 0),
                Err(error) => (
                    error.valid_up_to(),
                    error
                        .error_len()
                        .unwrap_or(rest.len() - error.valid_up_to()),
                ),
            };
            // `valid` never exceeds `rest`, so both ranges stay in the buffer.
            self.tag_buf
                .as_mut_slice()
                .copy_within(at..at + valid, kept);
            kept += valid;
            at += valid + invalid;
        }
        self.tag_buf.truncate(kept);
    }

    fn push_text_byte<F, E>(&mut self, byte: u8, emit: &mut F) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        if self.text_buf.push(byte).is_err() {
            self.flush_text(emit, false)?;
            let _ = self.text_buf.push(byte);
        }
        Ok(())
    }

    /// Emit buffered text as valid UTF-8. At a tag boundary the whole
    /// buffer drains (a malformed trailing sequence cannot be completed by
    /// later bytes and is dropped). At a forced mid-text flush, incomplete
    /// UTF-8 sequences and incomplete trailing entities are held back so
    /// later bytes can complete them. Invalid bytes are skipped.
    fn flush_text<F, E>(&mut self, emit: &mut F, at_tag_boundary: bool) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        let is_cdata = matches!(self.state, TokState::Cdata(_));
        loop {
            if self.text_buf.is_empty() {
                return Ok(());
            }
            let (valid_len, invalid_skip) = match core::str::from_utf8(&self.text_buf) {
                Ok(_) => (self.text_buf.len(), 0),
                Err(error) => (error.valid_up_to(), error.error_len().unwrap_or(0)),
            };
            if invalid_skip > 0 {
                if let Ok(text) = core::str::from_utf8(&self.text_buf[..valid_len]) {
                    if !text.is_empty() {
                        if is_cdata {
                            emit(TokEvent::Cdata(text))?;
                        } else {
                            emit(TokEvent::Text(text))?;
                        }
                    }
                }
                self.drain_text_prefix(valid_len + invalid_skip);
                continue;
            }

            let mut emit_len = valid_len;
            if !at_tag_boundary && !is_cdata {
                if let Ok(text) = core::str::from_utf8(&self.text_buf[..valid_len]) {
                    if let Some(amp) = text.rfind('&') {
                        let tail = &text[amp..];
                        if !tail.contains(';') && tail.len() <= STREAM_ENTITY_HOLDBACK_BYTES {
                            emit_len = amp;
                        }
                    }
                }
            }
            if emit_len > 0 {
                if let Ok(text) = core::str::from_utf8(&self.text_buf[..emit_len]) {
                    if !text.is_empty() {
                        if is_cdata {
                            emit(TokEvent::Cdata(text))?;
                        } else {
                            emit(TokEvent::Text(text))?;
                        }
                    }
                }
            }
            if at_tag_boundary {
                self.text_buf.clear();
            } else {
                self.drain_text_prefix(emit_len);
            }
            return Ok(());
        }
    }

    fn drain_text_prefix(&mut self, prefix_len: usize) {
        let len = self.text_buf.len();
        let prefix_len = prefix_len.min(len);
        self.text_buf.as_mut_slice().copy_within(prefix_len.., 0);
        self.text_buf.truncate(len - prefix_len);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NcxItemState {
    Closed,
    Open,
    Pushed,
}

pub struct NcxStreamParser {
    nav_depth: u8,
    item: NcxItemState,
    in_text: bool,
    title: heapless::String<160>,
    href: heapless::String<256>,
    href_set: bool,
}

impl Default for NcxStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NcxStreamParser {
    pub fn new() -> Self {
        Self {
            nav_depth: 0,
            item: NcxItemState::Closed,
            in_text: false,
            title: heapless::String::new(),
            href: heapless::String::new(),
            href_set: false,
        }
    }

    fn handle(
        &mut self,
        event: TokEvent<'_>,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        match event {
            TokEvent::StartTag(tag) if tag_name_is(tag, "navPoint") => {
                if matches!(self.item, NcxItemState::Open) {
                    self.flush_to_sink(sink)?;
                }
                self.nav_depth = self.nav_depth.saturating_add(1);
                self.item = NcxItemState::Open;
                self.in_text = false;
                self.title.clear();
                self.href.clear();
                self.href_set = false;
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "navPoint") => {
                if matches!(self.item, NcxItemState::Open) {
                    self.flush_to_sink(sink)?;
                }
                self.nav_depth = self.nav_depth.saturating_sub(1);
                self.item = if self.nav_depth > 0 {
                    NcxItemState::Open
                } else {
                    NcxItemState::Closed
                };
                self.in_text = false;
                self.title.clear();
                self.href.clear();
                self.href_set = false;
            }
            TokEvent::StartTag(tag)
                if matches!(self.item, NcxItemState::Open | NcxItemState::Pushed)
                    && tag_name_is(tag, "text") =>
            {
                self.in_text = true;
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "text") => {
                self.in_text = false;
            }
            TokEvent::StartTag(tag)
                if matches!(self.item, NcxItemState::Open) && tag_name_is(tag, "content") =>
            {
                if let Some(src) = attr_value(tag, "src") {
                    self.href.clear();
                    let _ = push_str_bounded(&mut self.href, src);
                    self.href_set = true;
                }
                if !self.title.as_str().trim_matches(is_html_space).is_empty() && self.href_set {
                    self.flush_to_sink(sink)?;
                    self.item = NcxItemState::Pushed;
                }
            }
            TokEvent::Text(text) if self.in_text => {
                append_owned_text(&mut self.title, text);
            }
            TokEvent::Cdata(text) if self.in_text => {
                append_text_until_full(&mut self.title, text);
            }
            _ => {}
        }
        Ok(())
    }

    fn flush_to_sink(&mut self, sink: &mut impl EpubTocSink) -> Result<(), TocStreamError> {
        let title = self.title.as_str().trim_matches(is_html_space);
        let href = self.href.as_str().trim_matches(is_html_space);
        if title.is_empty() || href.is_empty() {
            return Ok(());
        }
        sink.push_toc(title, href, self.nav_depth.max(1))?;
        Ok(())
    }
}

pub struct Epub3NavStreamParser {
    in_body: bool,
    body_required: bool,
    in_nav: bool,
    skip_depth: u8,
    skip_tag: Option<heapless::String<SKIP_TAG_NAME_BYTES>>,
    list_depth: u8,
    href: heapless::String<256>,
    href_set: bool,
    title: heapless::String<160>,
    level: u8,
}

impl Default for Epub3NavStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Epub3NavStreamParser {
    pub fn new() -> Self {
        Self {
            in_body: true,
            body_required: false,
            in_nav: false,
            skip_depth: 0,
            skip_tag: None,
            list_depth: 0,
            href: heapless::String::new(),
            href_set: false,
            title: heapless::String::new(),
            level: 0,
        }
    }

    /// Case-insensitive, like `tag_name_is` in [`parse_epub3_nav_to_sink`];
    /// both sides go through [`bounded_skip_name`], so an overlong name still
    /// pairs with its close.
    fn skip_tag_matches(&self, tag: &str) -> bool {
        let Some(stored) = self.skip_tag.as_ref() else {
            return false;
        };
        tag_local_name(tag)
            .map(|name| bounded_skip_name(name).eq_ignore_ascii_case(stored))
            .unwrap_or(false)
    }

    fn handle(
        &mut self,
        event: TokEvent<'_>,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        match event {
            TokEvent::StartTag(tag) if tag_name_is(tag, "body") => {
                self.body_required = true;
                self.in_body = true;
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "body") => {
                self.in_body = false;
            }
            TokEvent::StartTag(tag) if self.skip_depth > 0 => {
                // Only a matching close ends this skipped subtree. Nested
                // elements with the same name need their own matching closes;
                // empty elements do not contribute to the outstanding depth.
                if self.skip_tag_matches(tag) && !tag_is_void(tag) {
                    self.skip_depth = self
                        .skip_depth
                        .checked_add(1)
                        .ok_or(TocError::NestingTooDeep)?;
                }
            }
            TokEvent::StartTag(tag) if nav_skip_starts(tag) && !tag_is_void(tag) => {
                self.skip_tag = tag_local_name(tag).map(bounded_skip_name);
                self.skip_depth = 1;
            }
            TokEvent::EndTag(tag) if self.skip_tag_matches(tag) => {
                self.skip_depth = self.skip_depth.saturating_sub(1);
                if self.skip_depth == 0 {
                    self.skip_tag = None;
                }
            }
            _ if (self.body_required && !self.in_body) || self.skip_depth > 0 => {}
            TokEvent::StartTag(tag) if tag_name_is(tag, "nav") => {
                self.in_nav = nav_start_is_toc(tag);
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "nav") => self.in_nav = false,
            TokEvent::StartTag(tag) if self.in_nav && tag_name_is(tag, "ol") => {
                self.list_depth = self.list_depth.saturating_add(1);
            }
            TokEvent::EndTag(tag) if self.in_nav && tag_name_is(tag, "ol") => {
                self.list_depth = self.list_depth.saturating_sub(1);
            }
            TokEvent::StartTag(tag) if self.in_nav && tag_name_is(tag, "a") => {
                self.href.clear();
                self.href_set = false;
                if let Some(value) = attr_value(tag, "href") {
                    let _ = push_str_bounded(&mut self.href, value);
                    self.href_set = true;
                }
                self.title.clear();
                self.level = self.list_depth.max(1);
            }
            TokEvent::Text(text) if self.href_set => {
                append_owned_text(&mut self.title, text);
            }
            TokEvent::Cdata(text) if self.href_set => {
                append_text_until_full(&mut self.title, text);
            }
            TokEvent::EndTag(tag) if self.href_set && tag_name_is(tag, "a") => {
                let title = self.title.as_str().trim_matches(is_html_space);
                let href = self.href.as_str().trim_matches(is_html_space);
                if !title.is_empty() && !href.is_empty() {
                    sink.push_toc(title, href, self.level)?;
                }
                self.title.clear();
                self.href.clear();
                self.href_set = false;
            }
            _ => {}
        }
        Ok(())
    }
}

fn push_str_bounded<const N: usize>(out: &mut heapless::String<N>, value: &str) -> Result<(), ()> {
    for ch in value.chars() {
        if out.push(ch).is_err() {
            return Err(());
        }
    }
    Ok(())
}

/// Parse an EPUB 2 NCX directly from a byte stream into an [`EpubTocSink`].
/// Memory use is bounded by the tokenizer's tag/text buffers; the whole file
/// is never resident in RAM.
pub fn parse_epub2_ncx_stream<R>(
    reader: &mut R,
    sink: &mut impl EpubTocSink,
) -> Result<(), TocStreamError>
where
    R: ByteStream,
{
    let mut tokenizer = StreamingXmlTokenizer::new();
    let mut parser = NcxStreamParser::new();
    let mut buf = [0u8; 512];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|_| TocStreamError::Tokenizer)?;
        if n == 0 {
            break;
        }
        let bytes = &buf[..n];
        tokenizer.feed(bytes, &mut |event| parser.handle(event, sink))?;
    }
    tokenizer.finish(&mut |event| parser.handle(event, sink))?;
    Ok(())
}

/// Parse an EPUB 3 nav document directly from a byte stream.
pub fn parse_epub3_nav_stream<R>(
    reader: &mut R,
    sink: &mut impl EpubTocSink,
) -> Result<(), TocStreamError>
where
    R: ByteStream,
{
    let mut tokenizer = StreamingXmlTokenizer::new();
    let mut parser = Epub3NavStreamParser::new();
    let mut buf = [0u8; 512];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|_| TocStreamError::Tokenizer)?;
        if n == 0 {
            break;
        }
        let bytes = &buf[..n];
        tokenizer.feed(bytes, &mut |event| parser.handle(event, sink))?;
    }
    tokenizer.finish(&mut |event| parser.handle(event, sink))?;
    Ok(())
}

/// Slice-backed [`ByteStream`] used by tests and the slice-based parser
/// wrappers below.
pub struct SliceByteStream<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> SliceByteStream<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }
}

impl ByteStream for SliceByteStream<'_> {
    type Error = ();

    fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
        let remaining = self.bytes.len().saturating_sub(self.cursor);
        let take = remaining.min(out.len());
        out[..take].copy_from_slice(&self.bytes[self.cursor..self.cursor + take]);
        self.cursor += take;
        Ok(take)
    }
}

pub fn parse_css_text_align(css: &str, rules: &mut CssRules) {
    let mut cursor = 0usize;
    while let Some(open_rel) = css[cursor..].find('{') {
        let selector_text = css[cursor..cursor + open_rel].trim();
        let body_start = cursor + open_rel + 1;
        let Some(close_rel) = css[body_start..].find('}') else {
            break;
        };
        let body = &css[body_start..body_start + close_rel];
        if let Some(align) = parse_text_align_property(body) {
            for selector in selector_text.split(',') {
                rules.push_text_align(selector, align);
            }
        }
        cursor = body_start + close_rel + 1;
    }
}

fn tag_name_is(tag: &str, expected: &str) -> bool {
    tag_local_name(tag)
        .map(|name| name.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

fn tag_local_name(tag: &str) -> Option<&str> {
    let tag = tag.split_whitespace().next().unwrap_or(tag);
    Some(tag.trim_end_matches('/').rsplit(':').next().unwrap_or(tag))
}

/// Whether this `<nav>` start tag is the reading table of contents.
///
/// An EPUB 3 navigation document usually holds several `<nav>` elements:
/// `toc`, `page-list` (page markers like `i`, `ii`, `1`, `2`), landmarks, and
/// sometimes back-matter indices. Only the reading TOC lists chapters. EPUBs
/// are not consistent about where they put that meaning: some use `epub:type`,
/// others use ARIA/doc roles, labels, ids, or classes, so keep both the
/// positive and negative checks here.
fn nav_start_is_toc(tag: &str) -> bool {
    if let Some(value) = attr_value(tag, "epub:type") {
        return value
            .split_ascii_whitespace()
            .any(|word| word.eq_ignore_ascii_case("toc"));
    }

    if nav_attr_has_any(tag, "role", &["doc-toc", "toc"])
        || nav_attr_has_any(tag, "aria-label", &["toc", "contents", "tableofcontents"])
        || nav_attr_has_any(tag, "id", &["toc", "contents", "tableofcontents"])
        || nav_attr_has_any(tag, "class", &["toc", "contents", "tableofcontents"])
    {
        return true;
    }

    !nav_attr_has_any(
        tag,
        "role",
        &[
            "doc-pagelist",
            "pagelist",
            "page-list",
            "doc-index",
            "index",
            "doc-landmarks",
            "landmarks",
            "doc-loi",
            "loi",
            "doc-lot",
            "lot",
        ],
    ) && !nav_attr_has_any(
        tag,
        "aria-label",
        &[
            "pagelist",
            "page-list",
            "pages",
            "index",
            "landmarks",
            "listofillustrations",
            "listoftables",
        ],
    ) && !nav_attr_has_any(
        tag,
        "id",
        &[
            "pagelist",
            "page-list",
            "page_list",
            "pages",
            "index",
            "landmarks",
            "loi",
            "lot",
        ],
    ) && !nav_attr_has_any(
        tag,
        "class",
        &[
            "pagelist",
            "page-list",
            "page_list",
            "pages",
            "index",
            "landmarks",
            "loi",
            "lot",
        ],
    )
}

fn nav_attr_has_any(tag: &str, attr: &str, markers: &[&str]) -> bool {
    let Some(value) = attr_value(tag, attr) else {
        return false;
    };
    markers
        .iter()
        .any(|marker| ascii_contains_normalized(value, marker))
}

fn ascii_contains_normalized(value: &str, marker: &str) -> bool {
    let marker_len = marker
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .count();
    if marker_len == 0 {
        return false;
    }
    let mut matched = 0usize;
    for byte in value.bytes() {
        if !byte.is_ascii_alphanumeric() {
            continue;
        }
        let Some(expected) = normalized_marker_byte(marker, matched) else {
            return true;
        };
        if byte.eq_ignore_ascii_case(&expected) {
            matched += 1;
            if matched == marker_len {
                return true;
            }
        } else {
            let first = normalized_marker_byte(marker, 0).unwrap_or(expected);
            matched = usize::from(byte.eq_ignore_ascii_case(&first));
        }
    }
    false
}

fn normalized_marker_byte(marker: &str, index: usize) -> Option<u8> {
    marker
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .nth(index)
}

fn tag_is_hidden(tag: &str) -> bool {
    has_attribute(tag, "hidden") || tag_is_hidden_from_view(tag)
}

/// Hidden by inline style or from assistive technology, leaving out the
/// `hidden` attribute. The navigation document reads only these: there
/// `hidden` keeps a list out of the content flow, and a reading system's own
/// table of contents still lists it.
fn tag_is_hidden_from_view(tag: &str) -> bool {
    style_hides(tag)
        || attr_value(tag, "aria-hidden")
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

/// Whether the inline `style` leaves the element hidden: the winning
/// `display` is `none`, or the winning `visibility` is `hidden` or
/// `collapse`. Each property's winner is its last valid `!important`
/// declaration, or else its last valid one, so `display: none; display:
/// block` shows the element and `display: none; display: foo` does not. The
/// `all` shorthand, the only other property that sets either, counts as a
/// declaration of both.
///
/// The declarations come from [`style_declarations`], which reads the
/// attribute as CSS Syntax does; the host test
/// `style_declarations_agree_with_cssparser` checks it against Servo's
/// tokenizer, and the cascade has its own tests.
fn style_hides(tag: &str) -> bool {
    let Some(style) = attr_value(tag, "style") else {
        return false;
    };
    let mut cascade = StyleCascade::default();
    style_declarations(style, |declaration| cascade.apply(&declaration));
    cascade.hides()
}

/// Every `display`, `visibility` and `all` declaration in a style attribute, in
/// order, read as CSS Syntax reads one: tokens, not text. So the words in a
/// `title` or `alt` declare nothing, and neither does a comment or a string;
/// names and keywords match case-insensitively and through escapes (`n\one`);
/// and a declaration CSS would drop (`display:` with no `:`) is dropped up to
/// its `;` outside any block.
fn style_declarations(style: &str, mut found: impl FnMut(StyleDeclaration)) {
    let mut tokens = CssTokens {
        input: CssInput { rest: style },
    };
    loop {
        match tokens.next() {
            None => return,
            Some(CssToken::Space | CssToken::Semicolon | CssToken::Close('}')) => {}
            Some(CssToken::AtKeyword) => {
                // An at-rule ends at its `;` or after its `{}` block.
                loop {
                    match tokens.next() {
                        None | Some(CssToken::Semicolon) => break,
                        Some(CssToken::Open('{')) => {
                            tokens.skip_block('{');
                            break;
                        }
                        Some(CssToken::Open(open)) => {
                            tokens.skip_block(open);
                        }
                        Some(CssToken::Function(_)) => {
                            tokens.skip_block('(');
                        }
                        Some(_) => {}
                    }
                }
            }
            Some(CssToken::Ident(name)) => {
                if let Some(declaration) = tokens.declaration(name) {
                    found(declaration);
                }
            }
            Some(other) => tokens.drop_declaration(Some(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StyleProperty {
    Display,
    Visibility,
    /// Resets every property, these two included.
    All,
}

/// The most keywords a `display` value takes (`inline flow-root list-item`).
const STYLE_VALUE_WORDS: usize = 3;

/// A declaration's value, as far as the cascade needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StyleValue {
    /// One to [`STYLE_VALUE_WORDS`] identifiers, ASCII-lowercased.
    Keywords(heapless::Vec<heapless::String<STYLE_WORD_BYTES>, STYLE_VALUE_WORDS>),
    /// Holds a substitution function (`var()`, `env()`, `attr()`, `if()`),
    /// so CSS accepts it when parsing and decides it only once substituted.
    Substituted,
    /// Anything else, which is not a `display` or `visibility` value.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StyleDeclaration {
    property: StyleProperty,
    value: StyleValue,
    important: bool,
}

fn css_is_substitution(name: &str) -> bool {
    ["var", "env", "attr", "if"]
        .iter()
        .any(|function| name.eq_ignore_ascii_case(function))
}

fn css_wide_keyword(word: &str) -> bool {
    matches!(
        word,
        "initial" | "inherit" | "unset" | "revert" | "revert-layer"
    )
}

/// Whether a `display` value hides, or `None` when CSS Display 3 does not
/// accept it, so the declaration is dropped. Keywords are lowercase.
fn display_hides(words: &[heapless::String<STYLE_WORD_BYTES>]) -> Option<bool> {
    const OUTSIDE: [&str; 3] = ["block", "inline", "run-in"];
    const INSIDE: [&str; 6] = ["flow", "flow-root", "table", "flex", "grid", "ruby"];
    const ALONE: [&str; 18] = [
        "contents",
        "list-item",
        "inline-block",
        "inline-table",
        "inline-flex",
        "inline-grid",
        "table-row-group",
        "table-header-group",
        "table-footer-group",
        "table-row",
        "table-cell",
        "table-column-group",
        "table-column",
        "table-caption",
        "ruby-base",
        "ruby-text",
        "ruby-base-container",
        "ruby-text-container",
    ];
    if let [word] = words {
        let word = word.as_str();
        if word == "none" {
            return Some(true);
        }
        let valid = css_wide_keyword(word)
            || word == "math"
            || ALONE.contains(&word)
            || OUTSIDE.contains(&word)
            || INSIDE.contains(&word);
        return valid.then_some(false);
    }
    // `<display-outside> || <display-inside>`, or `<display-outside>? &&
    // [ flow | flow-root ]? && list-item`: each part once, in any order.
    let (mut outside, mut inside, mut list_item, mut list_inside) = (0, 0, 0, true);
    for word in words {
        let word = word.as_str();
        if OUTSIDE.contains(&word) {
            outside += 1;
        } else if INSIDE.contains(&word) {
            inside += 1;
            list_inside &= matches!(word, "flow" | "flow-root");
        } else if word == "list-item" {
            list_item += 1;
        } else {
            return None;
        }
    }
    let valid = if list_item == 1 {
        outside <= 1 && inside <= 1 && list_inside
    } else {
        list_item == 0 && outside == 1 && inside == 1
    };
    valid.then_some(false)
}

/// Whether an `all` value hides, or `None` when CSS does not accept it. It
/// takes only a CSS-wide keyword, and each leaves `display` and `visibility`
/// showing the element: `initial` and `unset` give `inline` and `visible`,
/// and `inherit` or `revert` take the parent's or the user agent's values,
/// which show it as far as this reader can tell.
fn all_hides(words: &[heapless::String<STYLE_WORD_BYTES>]) -> Option<bool> {
    let [word] = words else {
        return None;
    };
    css_wide_keyword(word).then_some(false)
}

/// Whether a `visibility` value hides, or `None` when CSS does not accept it.
fn visibility_hides(words: &[heapless::String<STYLE_WORD_BYTES>]) -> Option<bool> {
    let [word] = words else {
        return None;
    };
    match word.as_str() {
        "hidden" | "collapse" => Some(true),
        "visible" => Some(false),
        word => css_wide_keyword(word).then_some(false),
    }
}

/// One property's winners so far: whether its last valid declaration hides,
/// and the same for its last valid `!important` one.
#[derive(Clone, Copy, Default)]
struct StyleWinner {
    normal: Option<bool>,
    important: Option<bool>,
}

impl StyleWinner {
    fn hides(self) -> bool {
        self.important.or(self.normal).unwrap_or(false)
    }
}

#[derive(Default)]
struct StyleCascade {
    display: StyleWinner,
    visibility: StyleWinner,
}

impl StyleCascade {
    fn apply(&mut self, declaration: &StyleDeclaration) {
        let hides = match &declaration.value {
            // Unresolved here. Showing the element is the safer guess: hiding
            // would drop text a reader may need.
            StyleValue::Substituted => Some(false),
            StyleValue::Keywords(words) => match declaration.property {
                StyleProperty::Display => display_hides(words),
                StyleProperty::Visibility => visibility_hides(words),
                StyleProperty::All => all_hides(words),
            },
            StyleValue::Other => None,
        };
        let Some(hides) = hides else {
            return;
        };
        let set = |winner: &mut StyleWinner| {
            if declaration.important {
                winner.important = Some(hides);
            } else {
                winner.normal = Some(hides);
            }
        };
        match declaration.property {
            StyleProperty::Display => set(&mut self.display),
            StyleProperty::Visibility => set(&mut self.visibility),
            StyleProperty::All => {
                set(&mut self.display);
                set(&mut self.visibility);
            }
        }
    }

    fn hides(&self) -> bool {
        self.display.hides() || self.visibility.hides()
    }
}

/// A declaration value as it streams in: its keywords, whether anything else
/// or a substitution turned up, and how much of a trailing `! important` has
/// been seen. CSS takes the priority only from the value's last two tokens.
#[derive(Default)]
struct StyleValueScan {
    words: heapless::Vec<heapless::String<STYLE_WORD_BYTES>, STYLE_VALUE_WORDS>,
    other: bool,
    substituted: bool,
    /// 1 after a `!`, 2 after `! important`.
    priority: u8,
}

impl StyleValueScan {
    /// A token after a `!` or `! important` shows they were not the value's
    /// last two, so they are part of the value.
    fn settle_priority(&mut self) {
        if self.priority != 0 {
            self.other = true;
            self.priority = 0;
        }
    }

    fn bang(&mut self) {
        self.settle_priority();
        self.priority = 1;
    }

    fn ident(&mut self, word: CssWord) {
        if self.priority == 1
            && word
                .as_deref()
                .is_some_and(|word| word.eq_ignore_ascii_case("important"))
        {
            self.priority = 2;
            return;
        }
        self.settle_priority();
        let Some(word) = word else {
            self.other = true;
            return;
        };
        let mut lower = heapless::String::new();
        for ch in word.chars() {
            // Same capacity, and lowercasing ASCII keeps every length.
            let _ = lower.push(ch.to_ascii_lowercase());
        }
        if self.words.push(lower).is_err() {
            self.other = true;
        }
    }

    fn other(&mut self) {
        self.settle_priority();
        self.other = true;
    }

    fn finish(mut self, property: StyleProperty) -> StyleDeclaration {
        let important = self.priority == 2;
        if self.priority == 1 {
            self.other = true;
        }
        let value = if self.substituted {
            StyleValue::Substituted
        } else if self.other || self.words.is_empty() {
            StyleValue::Other
        } else {
            StyleValue::Keywords(self.words)
        };
        StyleDeclaration {
            property,
            value,
            important,
        }
    }
}

/// The longest name or keyword the cascade compares (`ruby-text-container`),
/// with room to spare. A longer identifier only has to fail to match.
const STYLE_WORD_BYTES: usize = 24;
/// How deeply nested `(`, `[` and `{` blocks are followed inside a style.
/// Past it the rest of the attribute is consumed as one block.
const STYLE_BLOCK_DEPTH: usize = 32;

/// An identifier, decoded; `None` when it outgrew [`STYLE_WORD_BYTES`].
type CssWord = Option<heapless::String<STYLE_WORD_BYTES>>;

/// The tokens [`style_declarations`] tells apart. Everything else (numbers,
/// hashes, strings, URLs, other delimiters) is `Other`: no keyword is spelled
/// with it.
enum CssToken {
    Ident(CssWord),
    /// A function's name and its `(`, which opens a block.
    Function(CssWord),
    AtKeyword,
    Space,
    Colon,
    Semicolon,
    Bang,
    /// `(`, `[` or `{`.
    Open(char),
    Close(char),
    Other,
}

/// A style attribute's characters as CSS sees them: entities decoded (the raw
/// attribute writes a quote inside `style="..."` as `&quot;`, the same rule
/// [`decode_html_entity`] applies to text), then CSS input preprocessing
/// (`\r\n`, `\r` and form feed become `\n`; NUL becomes U+FFFD).
#[derive(Clone)]
struct CssInput<'a> {
    rest: &'a str,
}

impl CssInput<'_> {
    fn decoded(&mut self) -> Option<char> {
        // `decode_html_entity` reads `&nbsp;` as a space for text, but CSS
        // takes U+00A0 as a name character: `none&nbsp;` is no `none`.
        let (ch, advance) = if self.rest.starts_with("&nbsp;") {
            ('\u{a0}', "&nbsp;".len())
        } else if let Some(decoded) = decode_html_entity(self.rest) {
            (
                decoded,
                self.rest.find(';').map(|index| index + 1).unwrap_or(1),
            )
        } else {
            let ch = self.rest.chars().next()?;
            (ch, ch.len_utf8())
        };
        self.rest = &self.rest[advance..];
        Some(ch)
    }

    fn peek(&self) -> Option<char> {
        self.clone().next()
    }

    fn peek2(&self) -> (Option<char>, Option<char>) {
        let mut ahead = self.clone();
        (ahead.next(), ahead.next())
    }
}

impl Iterator for CssInput<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        Some(match self.decoded()? {
            '\r' => {
                let mut ahead = self.clone();
                if ahead.decoded() == Some('\n') {
                    *self = ahead;
                }
                '\n'
            }
            '\x0c' => '\n',
            '\0' => '\u{fffd}',
            ch => ch,
        })
    }
}

fn css_is_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n')
}

fn css_name_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || !ch.is_ascii()
}

fn css_name_char(ch: char) -> bool {
    css_name_start(ch) || ch.is_ascii_digit() || ch == '-'
}

/// A `\` and what follows it make an escape unless a newline follows.
fn css_valid_escape(first: Option<char>, second: Option<char>) -> bool {
    first == Some('\\') && second != Some('\n')
}

/// Whether these three characters would start an identifier.
fn css_starts_ident(first: Option<char>, second: Option<char>, third: Option<char>) -> bool {
    match first {
        Some('-') => {
            second.is_some_and(|ch| css_name_start(ch) || ch == '-')
                || css_valid_escape(second, third)
        }
        Some('\\') => css_valid_escape(first, second),
        Some(ch) => css_name_start(ch),
        None => false,
    }
}

struct CssTokens<'a> {
    input: CssInput<'a>,
}

impl CssTokens<'_> {
    fn next(&mut self) -> Option<CssToken> {
        let ch = self.input.next()?;
        Some(match ch {
            ch if css_is_space(ch) => {
                while self.input.peek().is_some_and(css_is_space) {
                    self.input.next();
                }
                CssToken::Space
            }
            '/' if self.input.peek() == Some('*') => {
                self.input.next();
                // CSS ends an unclosed comment at the end of the input.
                let mut star = false;
                for ch in self.input.by_ref() {
                    if star && ch == '/' {
                        break;
                    }
                    star = ch == '*';
                }
                CssToken::Space
            }
            '"' | '\'' => {
                self.string(ch);
                CssToken::Other
            }
            ';' => CssToken::Semicolon,
            ':' => CssToken::Colon,
            '!' => CssToken::Bang,
            '(' | '[' | '{' => CssToken::Open(ch),
            ')' | ']' | '}' => CssToken::Close(ch),
            '@' => {
                let (second, third) = self.input.peek2();
                let fourth = self.input.clone().nth(2);
                if css_starts_ident(second, third, fourth) {
                    self.name(None);
                    CssToken::AtKeyword
                } else {
                    CssToken::Other
                }
            }
            '#' => {
                // A hash token takes the name after it, so `#var(` is no
                // function.
                let (second, third) = self.input.peek2();
                if second.is_some_and(css_name_char) || css_valid_escape(second, third) {
                    self.name(None);
                }
                CssToken::Other
            }
            ch if ch.is_ascii_digit() => {
                self.number();
                CssToken::Other
            }
            '+' | '-' | '.' if self.starts_number(ch) => {
                self.number();
                CssToken::Other
            }
            ch => {
                let (second, third) = self.input.peek2();
                if css_starts_ident(Some(ch), second, third) {
                    self.ident(ch)
                } else {
                    CssToken::Other
                }
            }
        })
    }

    fn starts_number(&self, first: char) -> bool {
        let (second, third) = self.input.peek2();
        let digit = |ch: Option<char>| ch.is_some_and(|ch| ch.is_ascii_digit());
        match first {
            '+' | '-' => digit(second) || (second == Some('.') && digit(third)),
            _ => digit(second),
        }
    }

    /// An escape's character, the `\` already consumed.
    fn escape(&mut self) -> char {
        let Some(first) = self.input.next() else {
            return '\u{fffd}';
        };
        let Some(mut value) = first.to_digit(16) else {
            return first;
        };
        for _ in 1..6 {
            match self.input.peek().and_then(|ch| ch.to_digit(16)) {
                Some(digit) => {
                    self.input.next();
                    value = value * 16 + digit;
                }
                None => break,
            }
        }
        if self.input.peek().is_some_and(css_is_space) {
            self.input.next();
        }
        match char::from_u32(value) {
            Some('\0') | None => '\u{fffd}',
            Some(ch) => ch,
        }
    }

    /// The rest of a name after `first`, decoded into `word` when given.
    fn name(&mut self, mut word: Option<&mut CssWord>) {
        loop {
            let (next, after) = self.input.peek2();
            let ch = match next {
                Some(ch) if css_name_char(ch) => {
                    self.input.next();
                    ch
                }
                Some('\\') if css_valid_escape(next, after) => {
                    self.input.next();
                    self.escape()
                }
                _ => return,
            };
            if let Some(word) = word.as_deref_mut() {
                if word.as_mut().is_some_and(|text| text.push(ch).is_err()) {
                    *word = None;
                }
            }
        }
    }

    fn ident(&mut self, first: char) -> CssToken {
        let first = if first == '\\' { self.escape() } else { first };
        let mut word: CssWord = Some(heapless::String::new());
        if word.as_mut().is_some_and(|text| text.push(first).is_err()) {
            word = None;
        }
        self.name(Some(&mut word));
        if self.input.peek() != Some('(') {
            return CssToken::Ident(word);
        }
        self.input.next();
        let is_url = word
            .as_ref()
            .is_some_and(|text| text.eq_ignore_ascii_case("url"));
        let mut ahead = self.input.clone();
        while ahead.peek().is_some_and(css_is_space) {
            ahead.next();
        }
        if is_url && !matches!(ahead.peek(), Some('"' | '\'')) {
            self.input = ahead;
            self.url();
            CssToken::Other
        } else {
            CssToken::Function(word)
        }
    }

    /// The rest of a string, the opening quote consumed. A newline ends a
    /// bad string and stays for the next token, as in CSS.
    fn string(&mut self, quote: char) {
        loop {
            match self.input.peek() {
                None => return,
                Some('\n') => return,
                Some(ch) => {
                    self.input.next();
                    if ch == quote {
                        return;
                    }
                    if ch == '\\' {
                        match self.input.peek() {
                            None => return,
                            Some('\n') => {
                                self.input.next();
                            }
                            Some(_) => {
                                self.escape();
                            }
                        }
                    }
                }
            }
        }
    }

    /// The rest of an unquoted `url(...)`, whose body is literal: a `/*` or
    /// `;` inside it is part of the URL.
    fn url(&mut self) {
        loop {
            let (next, after) = self.input.peek2();
            let Some(ch) = next else {
                return;
            };
            self.input.next();
            match ch {
                ')' => return,
                ch if css_is_space(ch) => {
                    while self.input.peek().is_some_and(css_is_space) {
                        self.input.next();
                    }
                    if matches!(self.input.peek(), None | Some(')')) {
                        self.input.next();
                        return;
                    }
                    return self.bad_url();
                }
                '\\' if css_valid_escape(next, after) => {
                    self.escape();
                }
                '"' | '\'' | '(' | '\\' => return self.bad_url(),
                ch if ch <= '\x08' || ch == '\x0b' || ('\x0e'..='\x1f').contains(&ch) => {
                    return self.bad_url()
                }
                '\x7f' => return self.bad_url(),
                _ => {}
            }
        }
    }

    fn bad_url(&mut self) {
        loop {
            let (next, after) = self.input.peek2();
            let Some(ch) = next else {
                return;
            };
            self.input.next();
            if ch == ')' {
                return;
            }
            if css_valid_escape(next, after) {
                self.escape();
            }
        }
    }

    fn number(&mut self) {
        let digits = |input: &mut CssInput<'_>| {
            while input.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                input.next();
            }
        };
        digits(&mut self.input);
        if let (Some('.'), Some(ch)) = self.input.peek2() {
            if ch.is_ascii_digit() {
                self.input.next();
                digits(&mut self.input);
            }
        }
        let mut ahead = self.input.clone();
        if matches!(ahead.next(), Some('e' | 'E')) {
            let (sign, digit) = ahead.peek2();
            let exponent = match sign {
                Some('+' | '-') => digit.is_some_and(|ch| ch.is_ascii_digit()),
                Some(ch) => ch.is_ascii_digit(),
                None => false,
            };
            if exponent {
                if matches!(sign, Some('+' | '-')) {
                    ahead.next();
                }
                self.input = ahead;
                digits(&mut self.input);
            }
        }
        let (first, second) = self.input.peek2();
        let third = self.input.clone().nth(2);
        if css_starts_ident(first, second, third) {
            self.name(None);
        } else if first == Some('%') {
            self.input.next();
        }
    }

    fn next_non_space(&mut self) -> Option<CssToken> {
        loop {
            match self.next() {
                Some(CssToken::Space) => {}
                token => return token,
            }
        }
    }

    /// The rest of a block opened by `open`, nested blocks included: whether
    /// it holds a substitution function. A close of another kind is only a
    /// token inside it.
    fn skip_block(&mut self, open: char) -> bool {
        let mut stack = heapless::Vec::<char, STYLE_BLOCK_DEPTH>::new();
        let _ = stack.push(open);
        let mut substituted = false;
        while let Some(token) = self.next() {
            let close = match token {
                CssToken::Function(name) => {
                    substituted |= name.as_deref().is_some_and(css_is_substitution);
                    if stack.push('(').is_err() {
                        while self.next().is_some() {}
                        return substituted;
                    }
                    continue;
                }
                CssToken::Open(open) => {
                    if stack.push(open).is_err() {
                        while self.next().is_some() {}
                        return substituted;
                    }
                    continue;
                }
                CssToken::Close(close) => close,
                _ => continue,
            };
            let opens = match close {
                ')' => '(',
                ']' => '[',
                _ => '{',
            };
            if stack.last() == Some(&opens) {
                stack.pop();
                if stack.is_empty() {
                    return substituted;
                }
            }
        }
        substituted
    }

    /// Drop the declaration `token` is part of, through its `;` outside any
    /// block.
    fn drop_declaration(&mut self, mut token: Option<CssToken>) {
        loop {
            match token {
                None | Some(CssToken::Semicolon) => return,
                Some(CssToken::Open(open)) => {
                    self.skip_block(open);
                }
                Some(CssToken::Function(_)) => {
                    self.skip_block('(');
                }
                Some(_) => {}
            }
            token = self.next();
        }
    }

    /// The rest of a declaration named `name`, consumed through its `;`: the
    /// declaration, when it is a `display`, `visibility` or `all` one CSS
    /// keeps.
    fn declaration(&mut self, name: CssWord) -> Option<StyleDeclaration> {
        let property = match name.as_deref() {
            Some(name) if name.eq_ignore_ascii_case("display") => StyleProperty::Display,
            Some(name) if name.eq_ignore_ascii_case("visibility") => StyleProperty::Visibility,
            Some(name) if name.eq_ignore_ascii_case("all") => StyleProperty::All,
            _ => {
                self.drop_declaration(Some(CssToken::Other));
                return None;
            }
        };
        let token = self.next_non_space();
        if !matches!(token, Some(CssToken::Colon)) {
            self.drop_declaration(token);
            return None;
        }
        let mut value = StyleValueScan::default();
        loop {
            match self.next_non_space() {
                None | Some(CssToken::Semicolon) => return Some(value.finish(property)),
                Some(CssToken::Bang) => value.bang(),
                Some(CssToken::Ident(word)) => value.ident(word),
                Some(CssToken::Function(name)) => {
                    value.other();
                    value.substituted |= name.as_deref().is_some_and(css_is_substitution);
                    value.substituted |= self.skip_block('(');
                }
                Some(CssToken::Open(open)) => {
                    value.other();
                    value.substituted |= self.skip_block(open);
                }
                Some(_) => value.other(),
            }
        }
    }
}

/// Whether the tag carries an attribute named `name`, bare or with any
/// value. Walks the attributes, so the word inside a value (`alt="the hidden
/// door"`, `title="hidden gems"`) is not taken for one.
fn has_attribute(tag: &str, name: &str) -> bool {
    let bytes = tag.as_bytes();
    let mut at = 0;
    // Past the element name.
    while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
        at += 1;
    }
    loop {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
            at += 1;
        }
        if at >= bytes.len() {
            return false;
        }
        let start = at;
        while at < bytes.len() && is_attr_name_byte(bytes[at]) {
            at += 1;
        }
        if at == start {
            // Not an attribute name: step over the byte and go on.
            at += 1;
            continue;
        }
        let found = bytes[start..at].eq_ignore_ascii_case(name.as_bytes());
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at < bytes.len() && bytes[at] == b'=' {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if at < bytes.len() && (bytes[at] == b'"' || bytes[at] == b'\'') {
                let quote = bytes[at];
                at += 1;
                while at < bytes.len() && bytes[at] != quote {
                    at += 1;
                }
                at += 1;
            } else {
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                    at += 1;
                }
            }
        }
        if found {
            return true;
        }
    }
}

fn tag_is_pagebreak(tag: &str) -> bool {
    tag_contains_class_word(tag, "pagebreak")
        || tag_contains_class_word(tag, "pagenum")
        || attr_value(tag, "role")
            .map(|value| value.eq_ignore_ascii_case("doc-pagebreak"))
            .unwrap_or(false)
        || attr_value(tag, "epub:type")
            .map(|value| {
                value
                    .split_ascii_whitespace()
                    .any(|word| word.eq_ignore_ascii_case("pagebreak"))
            })
            .unwrap_or(false)
}

fn tag_is_void(tag: &str) -> bool {
    tag.trim_end().ends_with('/')
        || tag_name_is(tag, "br")
        || tag_name_is(tag, "hr")
        || tag_name_is(tag, "img")
        || tag_name_is(tag, "meta")
        || tag_name_is(tag, "link")
        || tag_name_is(tag, "input")
}

fn tag_is_italic(tag: &str) -> bool {
    tag_name_is(tag, "em")
        || tag_name_is(tag, "i")
        || tag_name_is(tag, "code")
        || tag_name_is(tag, "tt")
        || tag_name_is(tag, "kbd")
        || tag_name_is(tag, "samp")
}

fn block_align_for_tag(tag: &str, css: Option<&CssRules>) -> Option<TextAlign> {
    if tag_contains_attr_value(tag, "align", "center")
        || tag_contains_class_word(tag, "center")
        || tag_contains_class_word(tag, "centered")
        || tag_contains_class_word(tag, "title")
        || tag_contains_style_value(tag, "text-align", "center")
    {
        Some(TextAlign::Center)
    } else {
        css.and_then(|rules| rules.alignment_for(tag))
    }
}

fn table_align_for_tag(tag: &str, css: Option<&CssRules>) -> Option<TextAlign> {
    if tag_contains_style_value(tag, "margin-left", "auto")
        && tag_contains_style_value(tag, "margin-right", "auto")
    {
        Some(TextAlign::Center)
    } else {
        block_align_for_tag(tag, css)
    }
}

fn current_table_align(stack: &[TextAlign; 4], depth: usize) -> Option<TextAlign> {
    if depth == 0 {
        None
    } else {
        Some(stack[depth.min(stack.len()) - 1])
    }
}

fn selector_is_supported(selector: &str) -> bool {
    !selector.is_empty()
        && !selector
            .as_bytes()
            .iter()
            .any(|byte| matches!(*byte, b'+' | b'>' | b'[' | b':' | b'#' | b'~' | b'*' | b' '))
}

fn parse_text_align_property(body: &str) -> Option<TextAlign> {
    for declaration in body.split(';') {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("text-align") {
            continue;
        }
        let value = value.trim();
        if value.eq_ignore_ascii_case("center") {
            return Some(TextAlign::Center);
        }
        if value.eq_ignore_ascii_case("left")
            || value.eq_ignore_ascii_case("right")
            || value.eq_ignore_ascii_case("start")
            || value.eq_ignore_ascii_case("end")
        {
            return Some(TextAlign::Left);
        }
        if value.eq_ignore_ascii_case("justify") {
            return Some(TextAlign::Justify);
        }
    }
    None
}

fn tag_contains_attr_value(tag: &str, attr: &str, value: &str) -> bool {
    attr_value(tag, attr)
        .map(|found| found.eq_ignore_ascii_case(value))
        .unwrap_or(false)
}

fn tag_contains_class_word(tag: &str, word: &str) -> bool {
    attr_value(tag, "class")
        .map(|classes| {
            classes
                .split_ascii_whitespace()
                .any(|class| class.eq_ignore_ascii_case(word))
        })
        .unwrap_or(false)
}

fn tag_contains_style_value(tag: &str, property: &str, expected: &str) -> bool {
    let Some(style) = attr_value(tag, "style") else {
        return false;
    };
    style.split(';').any(|declaration| {
        let Some((name, value)) = declaration.split_once(':') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case(property)
            && value
                .trim()
                .split_ascii_whitespace()
                .next()
                .map(|value| value.eq_ignore_ascii_case(expected))
                .unwrap_or(false)
    })
}

fn tag_starts_block(tag: &str) -> bool {
    tag_name_is(tag, "p")
        || tag_name_is(tag, "li")
        || tag_name_is(tag, "div")
        || tag_name_is(tag, "section")
        || tag_name_is(tag, "article")
        || tag_name_is(tag, "pre")
        || tag_name_is(tag, "dt")
        || tag_name_is(tag, "dd")
        || tag_name_is(tag, "tr")
}

fn tag_ends_block(tag: &str) -> bool {
    tag_name_is(tag, "p")
        || tag_name_is(tag, "li")
        || tag_name_is(tag, "div")
        || tag_name_is(tag, "section")
        || tag_name_is(tag, "article")
        || tag_name_is(tag, "pre")
        || tag_name_is(tag, "dt")
        || tag_name_is(tag, "dd")
        || tag_name_is(tag, "tr")
}

fn append_owned_text<const N: usize>(out: &mut heapless::String<N>, text: &str) {
    append_owned_text_until_full(out, text);
}

/// Append `text` with its whitespace collapsed, and optionally entities decoded, up to
/// what `out` has room for. Returns the bytes of `text` taken, so a caller
/// can carry the rest into another block.
fn append_owned_text_until_full_inner<const N: usize>(
    out: &mut heapless::String<N>,
    text: &str,
    decode_entities: bool,
) -> usize {
    let mut previous_space = out
        .as_str()
        .chars()
        .last()
        .map(is_html_space)
        .unwrap_or(false);
    let mut cursor = 0usize;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let Some((ch, advance)) = next_text_char(rest, decode_entities) else {
            break;
        };
        if is_html_space(ch) {
            if !previous_space && out.push(' ').is_err() {
                break;
            }
            previous_space = true;
        } else {
            if out.push(ch).is_err() {
                break;
            }
            previous_space = false;
        }
        cursor += advance;
    }
    cursor
}

fn append_owned_text_until_full<const N: usize>(
    out: &mut heapless::String<N>,
    text: &str,
) -> usize {
    append_owned_text_until_full_inner(out, text, true)
}

fn append_text_until_full<const N: usize>(out: &mut heapless::String<N>, text: &str) -> usize {
    append_owned_text_until_full_inner(out, text, false)
}

fn is_leading_punctuation_char(ch: char) -> bool {
    matches!(
        ch,
        ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{2019}' | '\u{201D}'
    )
}

/// Push one block and advance the spine item's stream cursor past it.
///
/// The cursor advances only on a push that succeeded, so a refused block
/// leaves no gap in the coordinate space.
fn push_block_at(
    sink: &mut impl XhtmlBlockSink,
    offset: &mut u32,
    text: &str,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    paragraph_end: bool,
) -> Result<(), XhtmlError> {
    sink.push_block(text, role, style, align, paragraph_end, *offset)?;
    *offset = offset.saturating_add(crate::anchor::block_stream_len(text.len()));
    Ok(())
}

fn flush_sink_block_at_word_boundary<const N: usize>(
    block: &mut heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    sink: &mut impl XhtmlBlockSink,
    offset: &mut u32,
) -> Result<(), XhtmlError> {
    // Only a break with a word before it splits the block. A block whose one
    // space is the one it opens with would emit that space alone, and the
    // layout sink drops a blank fragment and the rest of its paragraph.
    let split = block
        .as_str()
        .trim_end_matches(is_html_space)
        .rfind(is_html_space)
        .filter(|&split| {
            block
                .as_str()
                .get(..split)
                .is_some_and(|word| !word.trim_matches(is_html_space).is_empty())
        });
    let Some(split) = split else {
        // No such break in the block: it goes out whole, and the text after
        // it continues the same paragraph.
        return flush_sink_block_continue(block, role, style, align, sink, offset);
    };
    let carry_start = block.as_str()[split..]
        .char_indices()
        .find(|(_, ch)| !is_html_space(*ch))
        .map(|(index, _)| split + index)
        .unwrap_or(block.len());
    let mut carry = heapless::String::<N>::new();
    let _ = carry.push_str(&block.as_str()[carry_start..]);
    let mut emit = heapless::String::<N>::new();
    let _ = emit.push_str(block.as_str()[..split].trim_end_matches(is_html_space));
    let _ = emit.push(' ');
    if !emit.is_empty() {
        push_block_at(sink, offset, emit.as_str(), role, style, align, false)?;
    }
    *block = carry;
    Ok(())
}

fn flush_sink_block_continue<const N: usize>(
    block: &mut heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    sink: &mut impl XhtmlBlockSink,
    offset: &mut u32,
) -> Result<(), XhtmlError> {
    if block.as_str().trim_matches(is_html_space).is_empty() {
        block.clear();
        return Ok(());
    }
    let result = push_block_at(sink, offset, block.as_str(), role, style, align, false);
    block.clear();
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListKind {
    Ordered,
    Unordered,
}

fn append_list_marker<const N: usize>(
    out: &mut heapless::String<N>,
    counts: &mut [u16; 8],
    kinds: &[ListKind; 8],
    depth: usize,
) {
    if depth == 0 {
        append_owned_text(out, "- ");
        return;
    }
    let index = depth - 1;
    counts[index] = counts[index].saturating_add(1);
    match kinds[index] {
        ListKind::Unordered => append_owned_text(out, "- "),
        ListKind::Ordered => {
            append_u16(out, counts[index]);
            let _ = out.push('.');
            let _ = out.push(' ');
        }
    }
}

fn append_table_cell_separator<const N: usize>(out: &mut heapless::String<N>) {
    if out
        .as_str()
        .chars()
        .last()
        .map(|ch| !is_html_space(ch))
        .unwrap_or(false)
    {
        let _ = out.push(' ');
    }
}

fn append_u16<const N: usize>(out: &mut heapless::String<N>, value: u16) {
    let mut digits = [0u8; 5];
    let mut len = 0usize;
    let mut remaining = value;
    loop {
        digits[len] = b'0' + (remaining % 10) as u8;
        len += 1;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    while len > 0 {
        len -= 1;
        let _ = out.push(digits[len] as char);
    }
}

fn flush_sink_block<const N: usize>(
    block: &mut heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    sink: &mut impl XhtmlBlockSink,
    offset: &mut u32,
) -> Result<(), XhtmlError> {
    if block.as_str().trim_matches(is_html_space).is_empty() {
        block.clear();
        return push_block_at(sink, offset, "", role, style, align, true);
    }
    let result = push_block_at(sink, offset, block.as_str(), role, style, align, true);
    block.clear();
    result
}

/// HTML whitespace (` `, `\t`, `\n`, `\r`, `\x0C`). Unlike `char::is_whitespace`,
/// this leaves `\u{00A0}` (non-breaking space) intact so `Mr.&nbsp;Utterson`
/// stays bound across wraps and leading `&nbsp;` indents are preserved.
fn is_html_space(ch: char) -> bool {
    ch.is_ascii_whitespace()
}

/// Decode the HTML entity at the start of `input` (`&amp;`, `&#x2014;`, ...).
/// Shared by firmware cache building and host preview so EPUB text renders
/// identically on both sides.
pub fn decode_html_entity(input: &str) -> Option<char> {
    if let Some(decoded) = decode_numeric_entity(input) {
        return Some(decoded);
    }
    let rest = input.strip_prefix('&')?;
    let end = rest.bytes().take(10).position(|b| b == b';')?;
    match &rest[..end] {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        "ensp" | "emsp" | "thinsp" => Some(' '),
        "shy" => Some('\u{00AD}'),
        "zwnj" => Some('\u{200C}'),
        "zwj" => Some('\u{200D}'),
        "lrm" => Some('\u{200E}'),
        "rlm" => Some('\u{200F}'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "lsquo" => Some('‘'),
        "rsquo" => Some('’'),
        "sbquo" => Some('‚'),
        "ldquo" => Some('“'),
        "rdquo" => Some('”'),
        "bdquo" => Some('„'),
        "dagger" => Some('†'),
        "Dagger" => Some('‡'),
        "bull" => Some('•'),
        "hellip" => Some('…'),
        "permil" => Some('‰'),
        "prime" => Some('′'),
        "Prime" => Some('″'),
        "lsaquo" => Some('‹'),
        "rsaquo" => Some('›'),
        "oline" => Some('‾'),
        "frasl" => Some('⁄'),
        "euro" => Some('€'),
        "trade" => Some('™'),
        "fnof" => Some('ƒ'),
        "circ" => Some('ˆ'),
        "tilde" => Some('˜'),
        "OElig" => Some('Œ'),
        "oelig" => Some('œ'),
        "Scaron" => Some('Š'),
        "scaron" => Some('š'),
        "Yuml" => Some('Ÿ'),
        // HTML 4 / XHTML 1.0 Latin-1 supplement entities (U+00A1..U+00FF).
        "iexcl" => Some('\u{00A1}'),
        "cent" => Some('\u{00A2}'),
        "pound" => Some('\u{00A3}'),
        "curren" => Some('\u{00A4}'),
        "yen" => Some('\u{00A5}'),
        "brvbar" => Some('\u{00A6}'),
        "sect" => Some('\u{00A7}'),
        "uml" => Some('\u{00A8}'),
        "copy" => Some('\u{00A9}'),
        "ordf" => Some('\u{00AA}'),
        "laquo" => Some('\u{00AB}'),
        "not" => Some('\u{00AC}'),
        "reg" => Some('\u{00AE}'),
        "macr" => Some('\u{00AF}'),
        "deg" => Some('\u{00B0}'),
        "plusmn" => Some('\u{00B1}'),
        "sup2" => Some('\u{00B2}'),
        "sup3" => Some('\u{00B3}'),
        "acute" => Some('\u{00B4}'),
        "micro" => Some('\u{00B5}'),
        "para" => Some('\u{00B6}'),
        "middot" => Some('\u{00B7}'),
        "cedil" => Some('\u{00B8}'),
        "sup1" => Some('\u{00B9}'),
        "ordm" => Some('\u{00BA}'),
        "raquo" => Some('\u{00BB}'),
        "frac14" => Some('\u{00BC}'),
        "frac12" => Some('\u{00BD}'),
        "frac34" => Some('\u{00BE}'),
        "iquest" => Some('\u{00BF}'),
        "Agrave" => Some('\u{00C0}'),
        "Aacute" => Some('\u{00C1}'),
        "Acirc" => Some('\u{00C2}'),
        "Atilde" => Some('\u{00C3}'),
        "Auml" => Some('\u{00C4}'),
        "Aring" => Some('\u{00C5}'),
        "AElig" => Some('\u{00C6}'),
        "Ccedil" => Some('\u{00C7}'),
        "Egrave" => Some('\u{00C8}'),
        "Eacute" => Some('\u{00C9}'),
        "Ecirc" => Some('\u{00CA}'),
        "Euml" => Some('\u{00CB}'),
        "Igrave" => Some('\u{00CC}'),
        "Iacute" => Some('\u{00CD}'),
        "Icirc" => Some('\u{00CE}'),
        "Iuml" => Some('\u{00CF}'),
        "ETH" => Some('\u{00D0}'),
        "Ntilde" => Some('\u{00D1}'),
        "Ograve" => Some('\u{00D2}'),
        "Oacute" => Some('\u{00D3}'),
        "Ocirc" => Some('\u{00D4}'),
        "Otilde" => Some('\u{00D5}'),
        "Ouml" => Some('\u{00D6}'),
        "times" => Some('\u{00D7}'),
        "Oslash" => Some('\u{00D8}'),
        "Ugrave" => Some('\u{00D9}'),
        "Uacute" => Some('\u{00DA}'),
        "Ucirc" => Some('\u{00DB}'),
        "Uuml" => Some('\u{00DC}'),
        "Yacute" => Some('\u{00DD}'),
        "THORN" => Some('\u{00DE}'),
        "szlig" => Some('\u{00DF}'),
        "agrave" => Some('\u{00E0}'),
        "aacute" => Some('\u{00E1}'),
        "acirc" => Some('\u{00E2}'),
        "atilde" => Some('\u{00E3}'),
        "auml" => Some('\u{00E4}'),
        "aring" => Some('\u{00E5}'),
        "aelig" => Some('\u{00E6}'),
        "ccedil" => Some('\u{00E7}'),
        "egrave" => Some('\u{00E8}'),
        "eacute" => Some('\u{00E9}'),
        "ecirc" => Some('\u{00EA}'),
        "euml" => Some('\u{00EB}'),
        "igrave" => Some('\u{00EC}'),
        "iacute" => Some('\u{00ED}'),
        "icirc" => Some('\u{00EE}'),
        "iuml" => Some('\u{00EF}'),
        "eth" => Some('\u{00F0}'),
        "ntilde" => Some('\u{00F1}'),
        "ograve" => Some('\u{00F2}'),
        "oacute" => Some('\u{00F3}'),
        "ocirc" => Some('\u{00F4}'),
        "otilde" => Some('\u{00F5}'),
        "ouml" => Some('\u{00F6}'),
        "divide" => Some('\u{00F7}'),
        "oslash" => Some('\u{00F8}'),
        "ugrave" => Some('\u{00F9}'),
        "uacute" => Some('\u{00FA}'),
        "ucirc" => Some('\u{00FB}'),
        "uuml" => Some('\u{00FC}'),
        "yacute" => Some('\u{00FD}'),
        "thorn" => Some('\u{00FE}'),
        "yuml" => Some('\u{00FF}'),
        // HTML 4 Greek characters.
        "Alpha" => Some('\u{0391}'),
        "Beta" => Some('\u{0392}'),
        "Gamma" => Some('\u{0393}'),
        "Delta" => Some('\u{0394}'),
        "Epsilon" => Some('\u{0395}'),
        "Zeta" => Some('\u{0396}'),
        "Eta" => Some('\u{0397}'),
        "Theta" => Some('\u{0398}'),
        "Iota" => Some('\u{0399}'),
        "Kappa" => Some('\u{039A}'),
        "Lambda" => Some('\u{039B}'),
        "Mu" => Some('\u{039C}'),
        "Nu" => Some('\u{039D}'),
        "Xi" => Some('\u{039E}'),
        "Omicron" => Some('\u{039F}'),
        "Pi" => Some('\u{03A0}'),
        "Rho" => Some('\u{03A1}'),
        "Sigma" => Some('\u{03A3}'),
        "Tau" => Some('\u{03A4}'),
        "Upsilon" => Some('\u{03A5}'),
        "Phi" => Some('\u{03A6}'),
        "Chi" => Some('\u{03A7}'),
        "Psi" => Some('\u{03A8}'),
        "Omega" => Some('\u{03A9}'),
        "alpha" => Some('\u{03B1}'),
        "beta" => Some('\u{03B2}'),
        "gamma" => Some('\u{03B3}'),
        "delta" => Some('\u{03B4}'),
        "epsilon" => Some('\u{03B5}'),
        "zeta" => Some('\u{03B6}'),
        "eta" => Some('\u{03B7}'),
        "theta" => Some('\u{03B8}'),
        "iota" => Some('\u{03B9}'),
        "kappa" => Some('\u{03BA}'),
        "lambda" => Some('\u{03BB}'),
        "mu" => Some('\u{03BC}'),
        "nu" => Some('\u{03BD}'),
        "xi" => Some('\u{03BE}'),
        "omicron" => Some('\u{03BF}'),
        "pi" => Some('\u{03C0}'),
        "rho" => Some('\u{03C1}'),
        "sigmaf" => Some('\u{03C2}'),
        "sigma" => Some('\u{03C3}'),
        "tau" => Some('\u{03C4}'),
        "upsilon" => Some('\u{03C5}'),
        "phi" => Some('\u{03C6}'),
        "chi" => Some('\u{03C7}'),
        "psi" => Some('\u{03C8}'),
        "omega" => Some('\u{03C9}'),
        "thetasym" => Some('\u{03D1}'),
        "upsih" => Some('\u{03D2}'),
        "piv" => Some('\u{03D6}'),
        // HTML 4 Symbols, Arrows, and Math.
        "weierp" => Some('\u{2118}'),
        "image" => Some('\u{2111}'),
        "real" => Some('\u{211C}'),
        "alefsym" => Some('\u{2135}'),
        "larr" => Some('\u{2190}'),
        "uarr" => Some('\u{2191}'),
        "rarr" => Some('\u{2192}'),
        "darr" => Some('\u{2193}'),
        "harr" => Some('\u{2194}'),
        "crarr" => Some('\u{21B5}'),
        "lArr" => Some('\u{21D0}'),
        "uArr" => Some('\u{21D1}'),
        "rArr" => Some('\u{21D2}'),
        "dArr" => Some('\u{21D3}'),
        "hArr" => Some('\u{21D4}'),
        "forall" => Some('\u{2200}'),
        "part" => Some('\u{2202}'),
        "exist" => Some('\u{2203}'),
        "empty" => Some('\u{2205}'),
        "nabla" => Some('\u{2207}'),
        "isin" => Some('\u{2208}'),
        "notin" => Some('\u{2209}'),
        "ni" => Some('\u{220B}'),
        "prod" => Some('\u{220F}'),
        "sum" => Some('\u{2211}'),
        "minus" => Some('\u{2212}'),
        "lowast" => Some('\u{2217}'),
        "radic" => Some('\u{221A}'),
        "prop" => Some('\u{221D}'),
        "infin" => Some('\u{221E}'),
        "ang" => Some('\u{2220}'),
        "and" => Some('\u{2227}'),
        "or" => Some('\u{2228}'),
        "cap" => Some('\u{2229}'),
        "cup" => Some('\u{222A}'),
        "int" => Some('\u{222B}'),
        "there4" => Some('\u{2234}'),
        "sim" => Some('\u{223C}'),
        "cong" => Some('\u{2245}'),
        "asymp" => Some('\u{2248}'),
        "ne" => Some('\u{2260}'),
        "equiv" => Some('\u{2261}'),
        "le" => Some('\u{2264}'),
        "ge" => Some('\u{2265}'),
        "sub" => Some('\u{2282}'),
        "sup" => Some('\u{2283}'),
        "nsub" => Some('\u{2284}'),
        "sube" => Some('\u{2286}'),
        "supe" => Some('\u{2287}'),
        "oplus" => Some('\u{2295}'),
        "otimes" => Some('\u{2297}'),
        "perp" => Some('\u{22A5}'),
        "sdot" => Some('\u{22C5}'),
        "lceil" => Some('\u{2308}'),
        "rceil" => Some('\u{2309}'),
        "lfloor" => Some('\u{230A}'),
        "rfloor" => Some('\u{230B}'),
        "lang" => Some('\u{2329}'),
        "rang" => Some('\u{232A}'),
        "loz" => Some('\u{25CA}'),
        "spades" => Some('\u{2660}'),
        "clubs" => Some('\u{2663}'),
        "hearts" => Some('\u{2665}'),
        "diams" => Some('\u{2666}'),
        _ => None,
    }
}

/// Decode a numeric character reference (`&#8212;` or `&#x2014;`) at the
/// start of `input`.
pub fn decode_numeric_entity(input: &str) -> Option<char> {
    let rest = input.strip_prefix("&#")?;
    let end = rest.find(';')?;
    let entity = &rest[..end];
    let value = if let Some(hex) = entity
        .strip_prefix('x')
        .or_else(|| entity.strip_prefix('X'))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        entity.parse::<u32>().ok()?
    };
    char::from_u32(value)
}

fn push_owned_block<const N: usize, const M: usize>(
    text: heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    output: &mut Vec<TextBlock<N>, M>,
) -> Result<(), XhtmlError> {
    output
        .push(TextBlock::new(text, role, style, align))
        .map_err(|_| XhtmlError::TooManyRuns)
}

fn trim_owned<const N: usize>(text: &mut heapless::String<N>) {
    while text.as_str().as_bytes().last().copied() == Some(b' ') {
        text.pop();
    }
    let trim_len = text
        .as_str()
        .char_indices()
        .find(|(_, ch)| !is_html_space(*ch))
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    if trim_len == 0 {
        return;
    }
    let mut trimmed = heapless::String::<N>::new();
    let _ = trimmed.push_str(&text.as_str()[trim_len..]);
    *text = trimmed;
}

fn parse_central_entry(bytes: &[u8], cursor: usize) -> Result<(ZipEntry<'_>, usize), ZipError> {
    if read_u32(bytes, cursor)? != 0x0201_4b50 {
        return Err(ZipError::BadCentralDirectory);
    }
    let compression_method = read_u16(bytes, cursor + 10)?;
    let compressed_size = read_u32(bytes, cursor + 20)?;
    let uncompressed_size = read_u32(bytes, cursor + 24)?;
    let name_len = read_u16(bytes, cursor + 28)? as usize;
    let extra_len = read_u16(bytes, cursor + 30)? as usize;
    let comment_len = read_u16(bytes, cursor + 32)? as usize;
    let local_header_offset = read_u32(bytes, cursor + 42)?;
    let name_start = cursor + 46;
    let name_end = name_start
        .checked_add(name_len)
        .ok_or(ZipError::BadCentralDirectory)?;
    let next = name_end
        .checked_add(extra_len)
        .and_then(|value| value.checked_add(comment_len))
        .ok_or(ZipError::BadCentralDirectory)?;
    let name_bytes = bytes
        .get(name_start..name_end)
        .ok_or(ZipError::BadCentralDirectory)?;
    if name_len > MAX_ENTRY_NAME_BYTES {
        return Err(ZipError::NameTooLong);
    }
    let name = core::str::from_utf8(name_bytes).map_err(|_| ZipError::BadCentralDirectory)?;
    Ok((
        ZipEntry {
            name,
            compression_method,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        },
        next,
    ))
}

fn find_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 22 {
        return None;
    }
    let mut cursor = bytes.len() - 22;
    loop {
        if bytes.get(cursor..cursor + 4) == Some(&[0x50, 0x4b, 0x05, 0x06]) {
            return Some(cursor);
        }
        if cursor == 0 {
            return None;
        }
        cursor -= 1;
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ZipError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(ZipError::BadCentralDirectory)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ZipError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(ZipError::BadCentralDirectory)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_exact_at<R>(reader: &mut R, offset: u32, out: &mut [u8]) -> Result<(), ZipError>
where
    R: ReadAt,
{
    let mut filled = 0;
    while filled < out.len() {
        let count = reader
            .read_at(offset + filled as u32, &mut out[filled..])
            .map_err(|_| ZipError::Io)?;
        if count == 0 {
            return Err(ZipError::Io);
        }
        filled += count;
    }
    Ok(())
}

fn read_exact_stream<R>(reader: &mut R, mut out: &mut [u8]) -> Result<(), ZipError>
where
    R: ByteStream,
{
    while !out.is_empty() {
        let count = reader.read(out).map_err(|_| ZipError::Io)?;
        if count == 0 {
            return Err(ZipError::Io);
        }
        let tmp = out;
        out = &mut tmp[count..];
    }
    Ok(())
}

fn skip_stream<R>(reader: &mut R, mut bytes: usize) -> Result<(), ZipError>
where
    R: ByteStream,
{
    let mut scratch = [0u8; 512];
    while bytes > 0 {
        let count = bytes.min(scratch.len());
        read_exact_stream(reader, &mut scratch[..count])?;
        bytes -= count;
    }
    Ok(())
}

fn next_start_tag<'a>(xml: &'a str, name: &str, from: usize) -> Option<(&'a str, usize)> {
    let mut cursor = XmlCursor::new(xml.get(from..)?);
    while let Some(token) = cursor.next_token() {
        if let Token::Start(tag) = token {
            if tag_name_is(tag, name) {
                return Some((tag, from + cursor.cursor));
            }
        }
    }
    None
}

/// Strip a `#fragment` suffix from an href so it can be compared against
/// spine and manifest paths.
pub fn strip_fragment(value: &str) -> &str {
    value.split('#').next().unwrap_or(value)
}

fn find_attr_value<'a>(xml: &'a str, tag_name: &str, attr: &str) -> Option<&'a str> {
    let mut cursor = 0;
    while let Some((tag, next)) = next_start_tag(xml, tag_name, cursor) {
        if let Some(value) = attr_value(tag, attr) {
            return Some(value);
        }
        cursor = next;
    }
    None
}

/// Read a quoted attribute value by walking whole attributes. A substring
/// inside another value is text, even when it looks like `href='chapter'`.
fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut at = bytes.iter().position(u8::is_ascii_whitespace)?;
    while at < bytes.len() {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let start = at;
        while at < bytes.len() && is_attr_name_byte(bytes[at]) {
            at += 1;
        }
        if start == at {
            return None;
        }
        let matches = &bytes[start..at] == name.as_bytes();
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            continue; // A bare attribute has no value; inspect the next one.
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let quote = *bytes.get(at)?;
        if !matches!(quote, b'\'' | b'"') {
            // XML requires quotes. Skip the entire unsupported value rather
            // than finding a pretend attribute inside it.
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            continue;
        }
        at += 1;
        let value_start = at;
        let value_end = at + bytes[at..].iter().position(|byte| *byte == quote)?;
        at = value_end + 1;
        if matches {
            // Both boundaries sit beside ASCII quotes in a valid UTF-8 str.
            return Some(&tag[value_start..value_end]);
        }
    }
    None
}

fn is_attr_name_byte(byte: u8) -> bool {
    !byte.is_ascii_whitespace() && !matches!(byte, b'=' | b'/' | b'\'' | b'"')
}

fn style_for(bold_depth: u8, italic_depth: u8) -> FontStyle {
    match (bold_depth > 0, italic_depth > 0) {
        (true, true) => FontStyle::BoldItalic,
        (true, false) => FontStyle::Bold,
        (false, true) => FontStyle::Italic,
        (false, false) => FontStyle::Regular,
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box as StdBox;
    use std::vec::Vec as StdVec;

    struct SliceReader<'a> {
        bytes: &'a [u8],
    }

    impl ReadAt for SliceReader<'_> {
        type Error = ();

        fn len(&mut self) -> Result<u32, Self::Error> {
            Ok(self.bytes.len() as u32)
        }

        fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
            let offset = offset as usize;
            let Some(rest) = self.bytes.get(offset..) else {
                return Ok(0);
            };
            let count = out.len().min(rest.len());
            out[..count].copy_from_slice(&rest[..count]);
            Ok(count)
        }
    }

    struct SliceStream<'a> {
        bytes: &'a [u8],
        cursor: usize,
    }

    impl ByteStream for SliceStream<'_> {
        type Error = ();

        fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
            let take = out.len().min(self.bytes.len() - self.cursor);
            out[..take].copy_from_slice(&self.bytes[self.cursor..self.cursor + take]);
            self.cursor += take;
            Ok(take)
        }
    }

    #[test]
    fn xml_quoted_greater_than_preserves_tags_and_metadata() {
        let xml = r#"<p title="3 > 2" data-note='a > b'>Words</p>"#;
        let mut cursor = XmlCursor::new(xml);
        assert_eq!(
            cursor.next_token(),
            Some(Token::Start(r#"p title="3 > 2" data-note='a > b'"#))
        );
        assert_eq!(cursor.next_token(), Some(Token::Text("Words")));
        assert_eq!(cursor.next_token(), Some(Token::End("p")));
        assert_eq!(cursor.next_token(), None);
        let container =
            r#"<container><rootfile note="3 > 2" full-path="OPS/book.opf"/></container>"#;
        assert_eq!(
            find_attr_value(container, "rootfile", "full-path"),
            Some("OPS/book.opf")
        );
        // A comment ends at `-->`, so a rootfile inside one is no rootfile.
        let commented = r#"<container><!-- old > <rootfile full-path="old.opf"/> --><rootfile full-path="OPS/book.opf"/></container>"#;
        assert_eq!(
            find_attr_value(commented, "rootfile", "full-path"),
            Some("OPS/book.opf")
        );
        let opf = r#"<package><metadata><dc:title data-note="3 > 2">Real title</dc:title></metadata><manifest><item data-note='a > b' id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="chapter"/></spine></package>"#;
        let mut arena = [0u8; 4096];
        let package = parse_opf(
            opf.as_bytes(),
            &mut arena,
            BookId(7),
            "/books/book.epub",
            1234,
            "OPS/book.opf",
        )
        .unwrap();
        assert_eq!(package.meta.title, "Real title");
        assert_eq!(package.spine.len(), 1);
        assert_eq!(
            package.spine.get(0).unwrap().href.of(package.opf_text),
            "chapter.xhtml"
        );
    }

    #[test]
    fn xml_quoted_greater_than_matches_streamed_xhtml() {
        let xhtml = r#"<body><p title="3 > 2">Words</p><p title='a > b'>More words</p></body>"#;
        let mut whole = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(xhtml, None, &mut whole).unwrap();
        assert_eq!(whole.fragments.len(), 2);
        assert_eq!(whole.fragments[0].0, "Words");
        assert_eq!(whole.fragments[1].0, "More words");
        for chunk_len in [1, 7, 4096] {
            let mut streamed = RecordingSink {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .unwrap();
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .unwrap();
            assert_eq!(streamed.fragments, whole.fragments);
        }
        assert_eq!(
            XmlCursor::new(r#"<p title="unterminated > text"#).next_token(),
            None
        );
        // An apostrophe in an opaque declaration/comment is not an attribute.
        assert_eq!(
            XmlCursor::new("<!-- don't stop --><p>text</p>").next_token(),
            Some(Token::Start("p"))
        );
    }

    #[test]
    fn parses_nested_opf_path_and_spine() {
        let opf = r#"
            <package>
              <metadata>
                <dc:title>Flowers for Algernon</dc:title>
                <dc:creator>Daniel Keyes</dc:creator>
              </metadata>
              <manifest>
                <item id="chap1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
                <item id="cover" href="images/cover.jpg" media-type="image/jpeg"/>
              </manifest>
              <spine><itemref idref="chap1"/></spine>
            </package>
        "#;

        let mut arena = [0u8; 4096];
        let package = parse_opf(
            opf.as_bytes(),
            &mut arena,
            BookId(7),
            "/books/flowers.epub",
            1234,
            "OPS/package.opf",
        )
        .expect("opf parses");

        assert_eq!(package.meta.title, "Flowers for Algernon");
        assert_eq!(package.meta.author, "Daniel Keyes");
        assert_eq!(package.meta.cover_status, CoverStatus::Present);
        assert_eq!(package.cover_href, Some("images/cover.jpg"));
        assert_eq!(package.spine.len(), 1);
        assert_eq!(
            package.spine.get(0).unwrap().href.of(package.opf_text),
            "text/ch1.xhtml"
        );
    }

    struct RecordingSink {
        fragments: StdVec<(std::string::String, TextRole, FontStyle, TextAlign, bool)>,
    }

    impl XhtmlBlockSink for RecordingSink {
        fn push_block(
            &mut self,
            text: &str,
            role: TextRole,
            style: FontStyle,
            align: TextAlign,
            paragraph_end: bool,
            _logical_offset: u32,
        ) -> Result<(), XhtmlError> {
            if !text.is_empty() {
                self.fragments
                    .push((text.into(), role, style, align, paragraph_end));
            }
            Ok(())
        }
    }

    /// A text node with one short word and then a run without a space to
    /// within a few bytes of the block: the block fills, the word-boundary
    /// flush splits after that word and hands nearly the whole run back as
    /// carry, and the character that did not fit still does not. Every byte
    /// of the node is still delivered.
    #[test]
    fn a_run_that_overfills_the_block_twice_keeps_the_rest_of_the_node() {
        let mut run = std::string::String::from("<p>a ");
        for _ in 0..127 {
            run.push('世');
        }
        run.push('1');
        run.push_str("界界界 and the rest of the paragraph.</p>");
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(&run, None, &mut sink).expect("parses");
        let joined: std::string::String = sink
            .fragments
            .iter()
            .map(|fragment| fragment.0.as_str())
            .collect();
        let expected = run
            .strip_prefix("<p>")
            .and_then(|body| body.strip_suffix("</p>"))
            .expect("fixture has paragraph tags");
        assert_eq!(joined, expected, "lost text");
        let (last, body) = sink.fragments.split_last().expect("blocks");
        assert!(last.4, "the paragraph ends with its last block");
        assert!(
            body.iter().all(|fragment| !fragment.4),
            "no block before the last ends the paragraph: {:?}",
            sink.fragments
        );
    }

    /// The same run opening a pretty-printed paragraph, after the newline
    /// and indent that follow its tag. The block's only space is the one it
    /// opens with, so it goes out whole: that space alone would be a blank
    /// fragment, and the layout sink drops a blank fragment and the rest of
    /// its paragraph.
    #[test]
    fn a_run_after_leading_whitespace_leaves_no_blank_fragment() {
        let mut run = std::string::String::from("<p>\n  ");
        for _ in 0..127 {
            run.push('世');
        }
        run.push('1');
        run.push_str("界界界 and the rest of the paragraph.</p>");
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(&run, None, &mut sink).expect("parses");
        assert!(
            sink.fragments
                .iter()
                .all(|fragment| !fragment.0.trim().is_empty()),
            "a blank fragment: {:?}",
            sink.fragments
        );
        let joined: std::string::String = sink
            .fragments
            .iter()
            .map(|fragment| fragment.0.as_str())
            .collect();
        let expected = run
            .strip_prefix("<p>")
            .and_then(|body| body.strip_suffix("</p>"))
            .expect("fixture has paragraph tags")
            .trim_start();
        assert_eq!(joined.trim_start(), expected, "lost text");
        let (last, body) = sink.fragments.split_last().expect("blocks");
        assert!(last.4, "the paragraph ends with its last block");
        assert!(
            body.iter().all(|fragment| !fragment.4),
            "no block before the last ends the paragraph: {:?}",
            sink.fragments
        );
    }

    /// A run with no whitespace at all, longer than one block: it crosses
    /// blocks as one paragraph, the way a run with spaces does.
    #[test]
    fn an_unbroken_run_longer_than_a_block_stays_one_paragraph() {
        let mut run = std::string::String::from("<p>");
        for index in 0..900 {
            run.push(char::from(b'a' + (index % 26) as u8));
        }
        run.push_str("</p>");
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(&run, None, &mut sink).expect("parses");
        assert!(sink.fragments.len() > 1, "{:?}", sink.fragments.len());
        let joined: std::string::String = sink
            .fragments
            .iter()
            .map(|fragment| fragment.0.as_str())
            .collect();
        let expected = run
            .strip_prefix("<p>")
            .and_then(|body| body.strip_suffix("</p>"))
            .expect("fixture has paragraph tags");
        assert_eq!(joined, expected, "every byte delivered, in order");
        let (last, body) = sink.fragments.split_last().expect("blocks");
        assert!(last.4);
        assert!(
            body.iter().all(|fragment| !fragment.4),
            "{:?}",
            sink.fragments
        );
    }

    #[test]
    fn xml_attributes_ignore_names_inside_quoted_values() {
        let tag = r#"a title="an href='wrong.xhtml' example" data-href="also-wrong.xhtml" href = 'chapter.xhtml'"#;
        assert_eq!(attr_value(tag, "href"), Some("chapter.xhtml"));
        assert_eq!(attr_value(r#"a title="href='wrong.xhtml'""#, "href"), None);
        assert_eq!(
            attr_value(
                r#"p title='an align="center" example' align="right""#,
                "align"
            ),
            Some("right")
        );
        assert_eq!(
            attr_value(r#"p title="class='note'" disabled class="body""#, "class"),
            Some("body")
        );
        assert_eq!(
            attr_value(r#"p title="unterminated href='wrong.xhtml'"#, "href"),
            None
        );
    }

    #[test]
    fn xml_tags_end_outside_quoted_values() {
        let xml =
            r#"<rootfile title="1 > 0, full-path='wrong.opf'" full-path="OEBPS/content.opf"/>"#;
        assert_eq!(
            find_attr_value(xml, "rootfile", "full-path"),
            Some("OEBPS/content.opf")
        );
        let mut cursor = XmlCursor::new(
            r#"<!-- it's --><?foo it's okay?><a title='1 > 0' href="c.xhtml">C</a>"#,
        );
        assert_eq!(
            cursor.next_token(),
            Some(Token::Start(r#"a title='1 > 0' href="c.xhtml""#))
        );
        assert_eq!(cursor.next_token(), Some(Token::Text("C")));
    }

    #[test]
    fn xhtml_image_alt_comes_from_its_own_attribute() {
        let xhtml =
            r#"<body><img title="alt='Wrong description'" alt="Actual description"/></body>"#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 4>::new();
        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Actual description");
    }

    #[test]
    fn xhtml_sink_emits_styled_fragments() {
        let xhtml = "<body><h1>Chapter</h1><p>Hello <em>soft</em> <strong>bold</strong></p></body>";
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };

        xhtml_blocks_to_sink(xhtml, None, &mut sink).expect("xhtml parses");

        assert_eq!(
            sink.fragments[0],
            (
                "Chapter".into(),
                TextRole::Heading1,
                FontStyle::Bold,
                TextAlign::Center,
                true
            )
        );
        assert_eq!(
            sink.fragments[1],
            (
                "Hello ".into(),
                TextRole::Body,
                FontStyle::Regular,
                TextAlign::Justify,
                false
            )
        );
        assert_eq!(
            sink.fragments[2],
            (
                "soft".into(),
                TextRole::Body,
                FontStyle::Italic,
                TextAlign::Justify,
                false
            )
        );
        assert_eq!(
            sink.fragments[3],
            (
                "bold".into(),
                TextRole::Body,
                FontStyle::Bold,
                TextAlign::Justify,
                false
            )
        );
        assert_eq!(sink.fragments.len(), 4);
    }

    /// A paragraph whose last content is a style run terminates on a fragment
    /// carrying no text at all, because `</em>` flushes the words and `</p>`
    /// then flushes what is left, which is nothing.
    ///
    /// Sinks have to honour `paragraph_end` on that empty fragment or the next
    /// paragraph runs into this one. Both shipped sinks got it wrong the same
    /// way -- they reject empty text before reading the flag -- and neither
    /// lives anywhere the test suites reach: `fw` has no test modules and is
    /// excluded from the host runs, and `tools/preview` is its own workspace
    /// outside CI. So the contract they depend on is pinned here instead.
    #[test]
    fn a_paragraph_ending_in_a_style_run_still_terminates() {
        struct EveryFragment(StdVec<(std::string::String, bool)>);

        impl XhtmlBlockSink for EveryFragment {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                paragraph_end: bool,
                _logical_offset: u32,
            ) -> Result<(), XhtmlError> {
                self.0.push((text.into(), paragraph_end));
                Ok(())
            }
        }

        for markup in [
            "<body><p>Charlie ends <em>italic</em></p><p>Delta follows.</p></body>",
            "<body><p>Golf ends <strong>bold</strong></p><p>Hotel follows.</p></body>",
        ] {
            let mut sink = EveryFragment(StdVec::new());
            xhtml_blocks_to_sink(markup, None, &mut sink).expect("xhtml parses");

            let words = sink
                .0
                .iter()
                .position(|(text, _)| text == "italic" || text == "bold");
            let words = words.expect("the style run is emitted");
            let next = sink
                .0
                .iter()
                .position(|(text, _)| text.starts_with("Delta") || text.starts_with("Hotel"))
                .expect("the following paragraph is emitted");

            // The style run does not end the paragraph; something between it
            // and the next paragraph must, and it carries no text.
            assert!(!sink.0[words].1, "{markup}: {:?}", sink.0);
            let terminator = sink.0[words + 1..next]
                .iter()
                .find(|(_, paragraph_end)| *paragraph_end)
                .unwrap_or_else(|| {
                    panic!("{markup}: nothing terminates the paragraph: {:?}", sink.0)
                });
            assert!(
                terminator.0.is_empty(),
                "{markup}: the terminator carries text, so a sink that drops empty \
                 fragments would still see it: {:?}",
                sink.0
            );
        }
    }

    #[test]
    fn xhtml_blocks_skip_head_style_and_script_text() {
        let xhtml = r#"
            <html>
              <head>
                <title>Not reader text</title>
                <style>body { font-family: serif; } .calibre { margin: 0; }</style>
                <script>console.log("skip");</script>
              </head>
              <body><p>Actual chapter text.</p></body>
            </html>
        "#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Actual chapter text.");
        assert_eq!(blocks[0].role, TextRole::Body);
        assert_eq!(blocks[0].align, TextAlign::Justify);
    }

    /// An unbroken run longer than the parser's block reaches the vec sink
    /// as a continuation after a block it has already filled, and a run
    /// longer than a vec block arrives at all: neither is cut short.
    #[test]
    fn xhtml_text_blocks_keep_text_past_a_full_block() {
        let mut xhtml = std::string::String::from("<p>");
        for index in 0..500 {
            xhtml.push(char::from(b'a' + (index % 26) as u8));
        }
        xhtml.push_str(" end.</p>");
        let mut blocks = heapless::Vec::<TextBlock<64>, 16>::new();

        xhtml_text_blocks_with_css(&xhtml, None, &mut blocks).expect("blocks fit");

        let joined: std::string::String = blocks.iter().map(|block| block.text.as_str()).collect();
        let expected = xhtml
            .strip_prefix("<p>")
            .and_then(|body| body.strip_suffix("</p>"))
            .expect("fixture has paragraph tags");
        assert_eq!(joined, expected, "lost text");
        assert!(blocks.iter().all(|block| block.role == TextRole::Body));
    }

    /// A vec block that fills at a space, whether the space fits and the
    /// next character does not or the space itself does not: the space
    /// still separates the two words once the blocks are joined.
    #[test]
    fn xhtml_text_blocks_keep_the_space_a_split_falls_on() {
        for paragraph in ["abcdefghijklmno pqrst", "abcdefghijklmnop qrst"] {
            let xhtml = std::format!("<p>{paragraph}</p>");
            let mut blocks = heapless::Vec::<TextBlock<16>, 8>::new();

            xhtml_text_blocks_with_css(&xhtml, None, &mut blocks).expect("blocks fit");

            assert_eq!(blocks.len(), 2, "{blocks:?}");
            let joined: std::string::String =
                blocks.iter().map(|block| block.text.as_str()).collect();
            assert_eq!(joined, paragraph);
        }
    }

    /// A paragraph longer than the parser's block reaches the vec sink in
    /// fragments, so a block that a split opened with a space can be the one
    /// a later fragment continues. Shifting the words moves every boundary;
    /// at none of them does a space go missing.
    #[test]
    fn xhtml_text_blocks_keep_spaces_across_parser_fragments() {
        for shift in 0..40 {
            let mut paragraph = std::string::String::new();
            for _ in 0..shift {
                paragraph.push('x');
            }
            for index in 0..200 {
                paragraph.push(' ');
                for _ in 0..=(index % 7) {
                    paragraph.push(char::from(b'a' + (index % 26) as u8));
                }
            }
            let paragraph = paragraph.trim_start();
            let xhtml = std::format!("<p>{paragraph}</p>");
            let mut blocks = heapless::Vec::<TextBlock<32>, 64>::new();

            xhtml_text_blocks_with_css(&xhtml, None, &mut blocks).expect("blocks fit");

            let joined: std::string::String =
                blocks.iter().map(|block| block.text.as_str()).collect();
            assert_eq!(joined, paragraph, "shift {shift}");
        }
    }

    #[test]
    fn xml_comments_hide_markup_without_swallowing_later_content() {
        let xhtml = r#"<body><p>Before</p><!-- don't treat > or <p>comment text</p> as content --><p>After</p></body>"#;
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(xhtml, None, &mut sink).unwrap();
        assert_eq!(
            sink.fragments
                .iter()
                .map(|item| item.0.as_str())
                .collect::<StdVec<_>>(),
            ["Before", "After"]
        );
    }

    #[test]
    fn streamed_xml_comments_hide_markup_without_swallowing_later_content() {
        let xhtml = r#"<body><p>Before</p><!-- don't treat > or <p>comment text</p> as content --><p>After</p></body>"#;
        for chunk_len in [1, 2, 3, 7, 1024] {
            let mut sink = RecordingSink {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut sink)
                    .unwrap();
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut sink)
                .unwrap();
            assert_eq!(
                sink.fragments
                    .iter()
                    .map(|item| item.0.as_str())
                    .collect::<StdVec<_>>(),
                ["Before", "After"],
                "chunk size {chunk_len}"
            );
        }
    }

    #[test]
    fn xml_comments_can_exceed_tag_buffer_and_unterminated_ones_stay_hidden() {
        for closed in [false, true] {
            let mut xhtml = std::string::String::from("<body><p>Before</p><!--");
            xhtml.push_str(&"don't > render <p>this</p> ".repeat(100));
            if closed {
                xhtml.push_str("--><p>After</p></body>");
            }
            let mut whole = RecordingSink {
                fragments: StdVec::new(),
            };
            xhtml_blocks_to_sink(&xhtml, None, &mut whole).unwrap();
            assert_eq!(whole.fragments.len(), if closed { 2 } else { 1 });
            let mut streamed = RecordingSink {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml.as_bytes().chunks(3) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .unwrap();
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .unwrap();
            assert_eq!(streamed.fragments, whole.fragments);
        }
    }

    #[test]
    fn xhtml_blocks_skip_nav_and_hidden_content() {
        let xhtml = r#"
            <html>
              <body>
                <nav>Table of contents</nav>
                <p style="display:none">Hidden paragraph</p>
                <p>Visible text</p>
              </body>
            </html>
        "#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Visible text");
    }

    /// `hidden` is an attribute, bare or with any value, and only that: the
    /// word inside another attribute's value hides nothing.
    #[test]
    fn xhtml_blocks_hide_by_the_hidden_attribute_not_the_word() {
        let xhtml = r#"<body>
            <p hidden>gone</p>
            <p hidden="">gone</p>
            <p hidden="hidden">gone</p>
            <p class="note" hidden >gone</p>
            <p HIDDEN="until-found">gone</p>
            <p title="hidden gems">kept</p>
            <img src="d.jpg" alt="the hidden door"/>
            <p><a href="x.html" title="hidden">link text</a></p>
            <p class="hidden-ish">kept too</p>
        </body>"#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 16>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        let texts: StdVec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(texts, ["kept", "the hidden door", "link text", "kept too"]);
    }

    /// Only the `style` attribute's declarations hide an element, and CSS
    /// reads them case-insensitively, with or without `!important`.
    #[test]
    fn xhtml_blocks_hide_by_style_declarations_not_the_words() {
        let xhtml = r#"<body>
            <p style="DISPLAY: None">gone</p>
            <p style="color: red; display:none !important">gone</p>
            <p style="Visibility:HIDDEN">gone</p>
            <p title="CSS example: display:none">kept</p>
            <p data-note="visibility: hidden">kept too</p>
            <p style="display: block">shown</p>
        </body>"#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 16>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        let texts: StdVec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(texts, ["kept", "kept too", "shown"]);
    }

    /// A CSS comment reads as a space, wherever it falls and whatever it
    /// holds, and a declaration inside one declares nothing.
    #[test]
    fn xhtml_blocks_read_style_through_css_comments() {
        let xhtml = r#"<body>
            <p style="display:none/* comment */">gone</p>
            <p style="visibility:hidden/* comment */">gone</p>
            <p style="color: red; visibility:/* a; b: c */hidden">gone</p>
            <p style="/**/display/* x */ :/**/ none /* y */ !important">gone</p>
            <p style="display:none/* never closed">gone</p>
            <p style="/* display:none */ color: red">shown</p>
            <p style="display: /* none */ block">shown too</p>
            <p style="display:no/**/ne">split</p>
        </body>"#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 16>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        let texts: StdVec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(texts, ["shown", "shown too", "split"]);
    }

    /// Text follows the cascade's winner: a later declaration overrides an
    /// earlier one, `!important` beats both, and a value CSS rejects changes
    /// nothing.
    #[test]
    fn xhtml_blocks_follow_the_winning_style_declaration() {
        let xhtml = r#"<body>
            <p style="display:none; display:block">shown</p>
            <p style="display:block; display:none">gone</p>
            <p style="display:none !important; display:block">gone</p>
            <p style="display:none; display:foo">gone</p>
            <p style="visibility:hidden; visibility:visible">visible</p>
            <p style="visibility:collapse">gone</p>
            <p style="display:none; display:var(--d)">substituted</p>
            <p style="display:none; all:initial">reset</p>
            <p style="all:unset; display:none">gone</p>
        </body>"#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 16>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        let texts: StdVec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(texts, ["shown", "visible", "substituted", "reset"]);
    }

    /// A quoted string is literal, quoted with `'` or as `&quot;`: a `/*`,
    /// `;` or `:` inside it is text, and no string is a keyword.
    #[test]
    fn xhtml_blocks_read_style_strings_literally() {
        let xhtml = r#"<body>
            <p style="--marker:'/*';display:none">gone</p>
            <p style="--marker:&quot;/*&quot;;display:none">gone</p>
            <p style="content:'it\'s /*';visibility:hidden">gone</p>
            <p style="content:'a;display:none'">shown</p>
            <p style="display:'none'">quoted</p>
            <p style="--x:'unclosed&#10;display:none">bad string</p>
            <p style="display:none\9">escaped</p>
        </body>"#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 16>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        let texts: StdVec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(texts, ["shown", "quoted", "bad string", "escaped"]);
    }

    #[test]
    fn xhtml_nested_hidden_subtrees_stay_hidden_until_the_outer_close() {
        let xhtml = r#"<body><div aria-hidden="true"><div>hidden inner</div><div/>hidden tail<span>more hidden</span></div><p>Visible</p></body>"#;
        let mut whole = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(xhtml, None, &mut whole).expect("whole document parses");
        assert_eq!(whole.fragments.len(), 1);
        assert_eq!(whole.fragments[0].0, "Visible");

        for chunk_len in [1, 2, 7, 64, 4096] {
            let mut streamed = RecordingSink {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .expect("chunk parses");
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .expect("stream ends");
            assert_eq!(
                streamed.fragments, whole.fragments,
                "chunk size {chunk_len}"
            );
        }
    }

    #[test]
    fn xhtml_hidden_nesting_overflow_is_a_recoverable_error() {
        let mut parser = XhtmlBlockStreamParser::new(true);
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        parser
            .handle_start(r#"div aria-hidden="true""#, None, &mut sink)
            .unwrap();
        for _ in 1..u8::MAX {
            parser.handle_start("div", None, &mut sink).unwrap();
        }
        assert_eq!(
            parser.handle_start("div", None, &mut sink),
            Err(XhtmlError::NestingTooDeep)
        );
        assert!(sink.fragments.is_empty());
    }

    #[test]
    fn xhtml_blocks_number_ordered_list_items() {
        let xhtml = "<body><ol><li>One</li><li>Two</li></ol></body>";
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "1. One");
        assert_eq!(blocks[1].text, "2. Two");
    }

    #[test]
    fn xhtml_blocks_mark_center_aligned_blocks() {
        let xhtml =
            r#"<body><p class="center">Title</p><p style="text-align: center">Author</p></body>"#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(blocks[0].align, TextAlign::Center);
        assert_eq!(blocks[1].text, "Author");
        assert_eq!(blocks[1].align, TextAlign::Center);
    }

    #[test]
    fn xhtml_empty_elements_do_not_style_the_following_text() {
        let xhtml = r#"<body><p>Before<em/>after<strong />still normal</p><table align="center"/><p>Ordinary paragraph</p></body>"#;
        let mut whole = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(xhtml, None, &mut whole).expect("whole document parses");
        assert!(!whole.fragments.is_empty());
        for (text, role, style, align, _) in &whole.fragments {
            assert_eq!(*role, TextRole::Body, "{text}");
            assert_eq!(*style, FontStyle::Regular, "{text}");
            assert_eq!(*align, TextAlign::Justify, "{text}");
        }
        assert_eq!(
            whole
                .fragments
                .iter()
                .map(|fragment| fragment.0.as_str())
                .collect::<std::string::String>(),
            "Beforeafterstill normalOrdinary paragraph"
        );
        for chunk_len in [1, 3, 16, 4096] {
            let mut streamed = RecordingSink {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .expect("chunk parses");
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .expect("stream finishes");
            assert_eq!(
                streamed.fragments, whole.fragments,
                "chunk size {chunk_len}"
            );
        }
    }

    #[test]
    fn xhtml_empty_elements_preserve_enclosing_styles_and_body_boundaries() {
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink("<body/><p>Outside the body</p>", None, &mut sink).unwrap();
        assert!(sink.fragments.is_empty());
        xhtml_blocks_to_sink(
            "<body><p><strong>Bold<em/>still bold</strong>regular</p></body>",
            None,
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.fragments.len(), 3);
        assert_eq!(sink.fragments[0].2, FontStyle::Bold);
        assert_eq!(sink.fragments[1].2, FontStyle::Bold);
        assert_eq!(sink.fragments[2].2, FontStyle::Regular);
    }

    #[test]
    fn xhtml_empty_blockquotes_preserve_enclosing_role_and_alignment() {
        let xhtml = "<body><blockquote>Before<blockquote/>After</blockquote></body>";
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(xhtml, None, &mut sink).unwrap();
        assert_eq!(sink.fragments.len(), 2);
        assert_eq!(sink.fragments[0].0, "Before");
        assert_eq!(sink.fragments[0].1, TextRole::BlockQuote);
        assert_eq!(sink.fragments[0].2, FontStyle::Italic);
        assert_eq!(sink.fragments[0].3, TextAlign::Left);
        assert_eq!(sink.fragments[1].0, "After");
        assert_eq!(sink.fragments[1].1, TextRole::BlockQuote);
        assert_eq!(sink.fragments[1].2, FontStyle::Italic);
        assert_eq!(sink.fragments[1].3, TextAlign::Left);

        for chunk_len in [1, 3, 16, 4096] {
            let mut streamed = RecordingSink {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .expect("chunk parses");
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .expect("stream finishes");
            assert_eq!(streamed.fragments, sink.fragments, "chunk size {chunk_len}");
        }
    }

    #[test]
    fn xhtml_empty_elements_at_stack_limits_do_not_pop_enclosing_containers() {
        // Table capacity is 4. An empty table at capacity must not pop the 4th table.
        let xhtml_table = r#"<body>
            <table align="left"><tr><td>
            <table align="center"><tr><td>
            <table align="left"><tr><td>
            <table align="center"><tr><td>
                <table align="right"/>
                <p>Cell in table 4</p>
            </td></tr></table>
            </td></tr></table>
            </td></tr></table>
            </td></tr></table>
        </body>"#;
        let mut sink_table = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(xhtml_table, None, &mut sink_table).unwrap();
        let cell = sink_table
            .fragments
            .iter()
            .find(|fragment| fragment.0 == "Cell in table 4")
            .expect("cell found");
        assert_eq!(cell.3, TextAlign::Center);

        for chunk_len in [1, 3, 16, 4096] {
            let mut streamed = RecordingSink {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml_table.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .expect("chunk parses");
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .expect("stream finishes");
            assert_eq!(
                streamed.fragments, sink_table.fragments,
                "chunk size {chunk_len}"
            );
        }

        // List capacity is 8. An empty list at capacity must not pop the 8th list.
        let xhtml_list = r#"<body>
            <ol><li>Level 1
            <ol><li>Level 2
            <ol><li>Level 3
            <ol><li>Level 4
            <ol><li>Level 5
            <ol><li>Level 6
            <ol><li>Level 7
            <ol><li>First item at level 8</li><ul/><li>Second item at level 8</li></ol>
            </li></ol>
            </li></ol>
            </li></ol>
            </li></ol>
            </li></ol>
            </li></ol>
            </li></ol>
        </body>"#;
        let mut sink_list = RecordingSink {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(xhtml_list, None, &mut sink_list).unwrap();
        let texts: StdVec<&str> = sink_list
            .fragments
            .iter()
            .map(|fragment| fragment.0.as_str())
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("2. Second item at level 8")),
            "texts: {texts:?}"
        );

        for chunk_len in [1, 3, 16, 4096] {
            let mut streamed = RecordingSink {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml_list.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .expect("chunk parses");
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .expect("stream finishes");
            assert_eq!(
                streamed.fragments, sink_list.fragments,
                "chunk size {chunk_len}"
            );
        }
    }

    #[test]
    fn xhtml_empty_elements_at_saturation_depth_do_not_decrement_styles() {
        let mut parser = XhtmlBlockStreamParser::new(true);
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        // Saturate bold and italic depths to 255.
        for _ in 0..255 {
            parser.handle_start("strong", None, &mut sink).unwrap();
            parser.handle_start("em", None, &mut sink).unwrap();
        }
        // Self-closing elements at saturation limit must not prematurely decrement when closed.
        parser.handle_start("strong/", None, &mut sink).unwrap();
        parser.handle_start("em/", None, &mut sink).unwrap();
        // Unwind 254 enclosing levels: exactly one enclosing level remains open.
        for _ in 0..254 {
            parser.handle_end("strong", &mut sink).unwrap();
            parser.handle_end("em", &mut sink).unwrap();
        }
        parser
            .handle_text("still bold and italic", &mut sink)
            .unwrap();
        parser.finish(&mut sink).unwrap();
        assert_eq!(sink.fragments.len(), 1);
        assert_eq!(sink.fragments[0].2, FontStyle::BoldItalic);
    }

    #[test]
    fn xhtml_blocks_emit_simple_table_rows() {
        let xhtml = r#"
            <body>
              <h1>CONTENTS</h1>
              <table style="margin-left: auto; margin-right: auto;">
                <tr><td>I</td><td>Introduction</td></tr>
                <tr><td>II</td><td>The Machine</td></tr>
              </table>
            </body>
        "#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks[0].text, "CONTENTS");
        assert_eq!(blocks[0].align, TextAlign::Center);
        assert_eq!(blocks[1].text, "I Introduction");
        assert_eq!(blocks[1].align, TextAlign::Center);
        assert_eq!(blocks[2].text, "II The Machine");
        assert_eq!(blocks[2].align, TextAlign::Center);
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn epub3_nav_emits_flat_toc_records() {
        struct Sink {
            items: Vec<(heapless::String<32>, heapless::String<48>, u8), 8>,
        }
        impl EpubTocSink for Sink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut stored_title = heapless::String::<32>::new();
                let mut stored_href = heapless::String::<48>::new();
                stored_title
                    .push_str(title)
                    .map_err(|_| TocError::TooManyItems)?;
                stored_href
                    .push_str(href)
                    .map_err(|_| TocError::TooManyItems)?;
                self.items
                    .push((stored_title, stored_href, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a href="chapter1.xhtml#start">Introduction</a></li>
                <li><a href="chapter2.xhtml">The Machine</a>
                  <ol><li><a href="chapter2.xhtml#part">A room</a></li></ol>
                </li>
              </ol></nav>
            </body></html>
        "#;
        let mut sink = Sink { items: Vec::new() };
        parse_epub3_nav_to_sink(nav, &mut sink).expect("nav parses");

        assert_eq!(sink.items.len(), 3);
        assert_eq!(sink.items[0].0.as_str(), "Introduction");
        assert_eq!(sink.items[0].1.as_str(), "chapter1.xhtml#start");
        assert_eq!(sink.items[0].2, 1);
        assert_eq!(sink.items[2].0.as_str(), "A room");
        assert_eq!(sink.items[2].2, 2);
    }

    #[test]
    fn epub2_ncx_emits_flat_toc_records() {
        struct Sink {
            items: Vec<(heapless::String<32>, heapless::String<48>, u8), 8>,
        }
        impl EpubTocSink for Sink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut stored_title = heapless::String::<32>::new();
                let mut stored_href = heapless::String::<48>::new();
                stored_title
                    .push_str(title)
                    .map_err(|_| TocError::TooManyItems)?;
                stored_href
                    .push_str(href)
                    .map_err(|_| TocError::TooManyItems)?;
                self.items
                    .push((stored_title, stored_href, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let ncx = r#"
            <ncx><navMap>
              <navPoint><navLabel><text>Introduction</text></navLabel><content src="chapter1.xhtml"/>
                <navPoint><navLabel><text>Part A</text></navLabel><content src="chapter1.xhtml#a"/></navPoint>
              </navPoint>
              <navPoint><navLabel><text>The Machine</text></navLabel><content src="chapter2.xhtml"/></navPoint>
            </navMap></ncx>
        "#;
        let mut sink = Sink { items: Vec::new() };
        parse_epub2_ncx_to_sink(ncx, &mut sink).expect("ncx parses");

        assert_eq!(sink.items.len(), 3);
        assert_eq!(sink.items[0].0.as_str(), "Introduction");
        assert_eq!(sink.items[0].1.as_str(), "chapter1.xhtml");
        assert_eq!(sink.items[0].2, 1);
        assert_eq!(sink.items[1].0.as_str(), "Part A");
        assert_eq!(sink.items[1].2, 2);
        assert_eq!(sink.items[2].0.as_str(), "The Machine");
    }

    #[test]
    fn ncx_stream_keeps_multibyte_titles_across_any_chunking() {
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 16>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let ncx = "<ncx><navMap>\
            <navPoint><navLabel><text>Pr\u{e9}face \u{2014} \u{4e16}\u{754c}</text></navLabel>\
            <content src=\"ch1.xhtml\"/></navPoint>\
            <navPoint><navLabel><text>Caf\u{e9} &amp; r\u{ea}ve</text></navLabel>\
            <content src=\"ch2.xhtml\"/></navPoint>\
            </navMap></ncx>";

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub2_ncx_to_sink(ncx, &mut slice_sink).expect("slice parses");
        assert_eq!(slice_sink.0.len(), 2);

        // One byte at a time: every UTF-8 sequence and the entity get
        // split across feed calls, which the old char-per-byte text
        // accumulation turned into mojibake.
        let mut tokenizer = StreamingXmlTokenizer::new();
        let mut parser = NcxStreamParser::new();
        let mut stream_sink = CountingSink(Vec::new());
        for byte in ncx.as_bytes() {
            tokenizer
                .feed_ncx(core::slice::from_ref(byte), &mut parser, &mut stream_sink)
                .expect("byte feeds");
        }
        tokenizer
            .finish_ncx(&mut parser, &mut stream_sink)
            .expect("finish");

        assert_eq!(slice_sink.0.len(), stream_sink.0.len());
        for (a, b) in slice_sink.0.iter().zip(stream_sink.0.iter()) {
            assert_eq!(a.0.as_str(), b.0.as_str());
            assert_eq!(a.1.as_str(), b.1.as_str());
        }
    }

    #[test]
    fn epub2_ncx_stream_matches_slice_parser() {
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 256>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let ncx = r#"<?xml version='1.0' encoding='utf-8'?>
            <ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
              <head><meta name="dtb:uid" content="x"/></head>
              <navMap>
                <navPoint><navLabel><text>Introduction</text></navLabel><content src="chapter1.xhtml"/>
                  <navPoint><navLabel><text>Part A</text></navLabel><content src="chapter1.xhtml#a"/></navPoint>
                </navPoint>
                <navPoint><navLabel><text>The Machine</text></navLabel><content src="chapter2.xhtml"/></navPoint>
              </navMap>
            </ncx>"#;

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub2_ncx_to_sink(ncx, &mut slice_sink).expect("slice parses");

        let mut stream = SliceByteStream::new(ncx.as_bytes());
        let mut stream_sink = CountingSink(Vec::new());
        parse_epub2_ncx_stream(&mut stream, &mut stream_sink).expect("stream parses");

        assert_eq!(slice_sink.0.len(), stream_sink.0.len());
        for (a, b) in slice_sink.0.iter().zip(stream_sink.0.iter()) {
            assert_eq!(a.0.as_str(), b.0.as_str());
            assert_eq!(a.1.as_str(), b.1.as_str());
            assert_eq!(a.2, b.2);
        }
    }

    #[test]
    fn epub3_nav_stream_matches_slice_parser() {
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 16>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <!-- the reader's contents -->
                <?foo it's okay?>
                <li><a title="1 > 0, href='wrong.xhtml'" href="chapter1.xhtml#start">Introduction</a></li>
                <li><a href="chapter2.xhtml">The Machine</a>
                  <ol><li><a href="chapter2.xhtml#part">A room</a></li></ol>
                </li>
              </ol></nav>
            </body></html>
        "#;

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub3_nav_to_sink(nav, &mut slice_sink).expect("slice parses");

        let mut stream = SliceByteStream::new(nav.as_bytes());
        let mut stream_sink = CountingSink(Vec::new());
        parse_epub3_nav_stream(&mut stream, &mut stream_sink).expect("stream parses");

        assert_eq!(slice_sink.0.len(), 3);
        assert_eq!(slice_sink.0[0].1.as_str(), "chapter1.xhtml#start");
        assert_eq!(slice_sink.0.len(), stream_sink.0.len());
        for (a, b) in slice_sink.0.iter().zip(stream_sink.0.iter()) {
            assert_eq!(a.0.as_str(), b.0.as_str());
            assert_eq!(a.1.as_str(), b.1.as_str());
            assert_eq!(a.2, b.2);
        }
    }

    #[test]
    fn epub2_ncx_cdata_preserves_literal_entity_syntax() {
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 16>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let ncx = r#"<?xml version='1.0' encoding='utf-8'?>
            <ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
              <navMap>
                <navPoint>
                  <navLabel><text><![CDATA[Chapter &lt; 10]]></text></navLabel>
                  <content src="chapter1.xhtml"/>
                </navPoint>
                <navPoint>
                  <navLabel><text>Chapter &lt; 11</text></navLabel>
                  <content src="chapter2.xhtml"/>
                </navPoint>
              </navMap>
            </ncx>"#;

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub2_ncx_to_sink(ncx, &mut slice_sink).expect("slice parses");
        assert_eq!(slice_sink.0.len(), 2);
        assert_eq!(slice_sink.0[0].0.as_str(), "Chapter &lt; 10");
        assert_eq!(slice_sink.0[1].0.as_str(), "Chapter < 11");

        let mut stream = SliceByteStream::new(ncx.as_bytes());
        let mut stream_sink = CountingSink(Vec::new());
        parse_epub2_ncx_stream(&mut stream, &mut stream_sink).expect("stream parses");
        assert_eq!(stream_sink.0.len(), 2);
        assert_eq!(stream_sink.0[0].0.as_str(), "Chapter &lt; 10");
        assert_eq!(stream_sink.0[1].0.as_str(), "Chapter < 11");
    }

    #[test]
    fn epub3_nav_cdata_preserves_literal_entity_syntax() {
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 16>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a href="chapter1.xhtml"><![CDATA[Chapter &lt; 10]]></a></li>
                <li><a href="chapter2.xhtml">Chapter &lt; 11</a></li>
              </ol></nav>
            </body></html>
        "#;

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub3_nav_to_sink(nav, &mut slice_sink).expect("slice parses");
        assert_eq!(slice_sink.0.len(), 2);
        assert_eq!(slice_sink.0[0].0.as_str(), "Chapter &lt; 10");
        assert_eq!(slice_sink.0[1].0.as_str(), "Chapter < 11");

        let mut stream = SliceByteStream::new(nav.as_bytes());
        let mut stream_sink = CountingSink(Vec::new());
        parse_epub3_nav_stream(&mut stream, &mut stream_sink).expect("stream parses");
        assert_eq!(stream_sink.0.len(), 2);
        assert_eq!(stream_sink.0[0].0.as_str(), "Chapter &lt; 10");
        assert_eq!(stream_sink.0[1].0.as_str(), "Chapter < 11");
    }

    #[test]
    fn epub3_nav_skips_page_list_and_landmarks() {
        // A real EPUB 3 nav document carries the `toc` alongside a `page-list`
        // (roman-numeral / numeric page markers) and `landmarks` navs. Only the
        // toc lists chapters; the others must not leak in as phantom chapters.
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 16>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a href="chapter1.xhtml">Introduction</a></li>
                <li><a href="chapter2.xhtml">The Machine</a></li>
              </ol></nav>
              <nav epub:type="page-list" role="doc-pagelist"><ol>
                <li><a href="front.xhtml#pi">i</a></li>
                <li><a href="front.xhtml#pii">ii</a></li>
                <li><a href="front.xhtml#piii">iii</a></li>
                <li><a href="chapter1.xhtml#p1">1</a></li>
                <li><a href="chapter1.xhtml#p2">2</a></li>
              </ol></nav>
              <nav epub:type="landmarks"><ol>
                <li><a epub:type="cover" href="cover.xhtml">Cover</a></li>
                <li><a epub:type="bodymatter" href="chapter1.xhtml">Start Reading</a></li>
              </ol></nav>
              <nav role="doc-pagelist"><ol>
                <li><a href="back.xhtml#pix">ix</a></li>
                <li><a href="back.xhtml#px">x</a></li>
              </ol></nav>
              <nav aria-label="Index"><ol>
                <li><a href="back.xhtml#index-a">A</a></li>
                <li><a href="back.xhtml#index-b">B</a></li>
              </ol></nav>
              <nav id="loi"><ol>
                <li><a href="figures.xhtml#f1">Figure 1</a></li>
              </ol></nav>
              <nav class="page_list"><ol>
                <li><a href="back.xhtml#pxi">xi</a></li>
              </ol></nav>
              <nav epub:type="lot"><ol>
                <li><a href="tables.xhtml#t1">Table 1</a></li>
              </ol></nav>
            </body></html>
        "#;

        let expected = [
            ("Introduction", "chapter1.xhtml"),
            ("The Machine", "chapter2.xhtml"),
        ];

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub3_nav_to_sink(nav, &mut slice_sink).expect("slice parses");
        assert_eq!(slice_sink.0.len(), expected.len());
        for (item, (title, href)) in slice_sink.0.iter().zip(expected.iter()) {
            assert_eq!(item.0.as_str(), *title);
            assert_eq!(item.1.as_str(), *href);
        }

        // The streaming parser (the one that runs on-device) must agree.
        let mut stream = SliceByteStream::new(nav.as_bytes());
        let mut stream_sink = CountingSink(Vec::new());
        parse_epub3_nav_stream(&mut stream, &mut stream_sink).expect("stream parses");
        assert_eq!(stream_sink.0.len(), expected.len());
        for (item, (title, href)) in stream_sink.0.iter().zip(expected.iter()) {
            assert_eq!(item.0.as_str(), *title);
            assert_eq!(item.1.as_str(), *href);
        }
    }

    #[test]
    fn epub3_nav_without_epub_type_is_still_read() {
        // Older/minimal single-nav documents omit `epub:type`; those must still
        // parse as the table of contents.
        struct CountingSink(Vec<heapless::String<64>, 8>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, _href: &str, _level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let _ = t.push_str(title);
                self.0.push(t).map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav><ol>
                <li><a href="chapter1.xhtml">Introduction</a></li>
              </ol></nav>
            </body></html>
        "#;

        let mut sink = CountingSink(Vec::new());
        parse_epub3_nav_to_sink(nav, &mut sink).expect("nav parses");
        assert_eq!(sink.0.len(), 1);
        assert_eq!(sink.0[0].as_str(), "Introduction");
    }

    /// In a navigation document the `hidden` attribute keeps a list out of
    /// the content flow only, and a reading system's own table of contents
    /// still lists it. Both nav parsers read a hidden toc and its hidden
    /// sublist.
    #[test]
    fn epub3_nav_reads_entries_hidden_only_from_the_content_flow() {
        struct Titles(StdVec<std::string::String>);
        impl EpubTocSink for Titles {
            fn push_toc(&mut self, title: &str, _href: &str, _level: u8) -> Result<(), TocError> {
                self.0.push(title.into());
                Ok(())
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc" hidden=""><ol>
                <li><a href="chapter1.xhtml">One</a>
                  <ol hidden><li><a href="chapter1.xhtml#a">One A</a></li></ol>
                </li>
              </ol></nav>
            </body></html>
        "#;

        let mut slice = Titles(StdVec::new());
        parse_epub3_nav_to_sink(nav, &mut slice).expect("slice parses");
        let mut stream = Titles(StdVec::new());
        parse_epub3_nav_stream(&mut SliceByteStream::new(nav.as_bytes()), &mut stream)
            .expect("stream parses");
        assert_eq!(slice.0, ["One", "One A"]);
        assert_eq!(stream.0, slice.0);
    }

    #[test]
    fn epub3_nav_nested_hidden_subtrees_stay_hidden_until_the_outer_close() {
        struct TitleSink(Vec<heapless::String<64>, 8>);
        impl EpubTocSink for TitleSink {
            fn push_toc(&mut self, title: &str, _href: &str, _level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let _ = t.push_str(title);
                self.0.push(t).map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a href="one.xhtml">One</a></li>
                <li aria-hidden="true"><a href="draft.xhtml">Draft</a>
                  <li><a href="inner.xhtml">Inner</a></li><li/>
                  <a href="tail.xhtml">Tail</a></li>
                <li><a href="two.xhtml">Two</a></li>
              </ol></nav>
            </body></html>
        "#;

        let mut sink = TitleSink(Vec::new());
        parse_epub3_nav_to_sink(nav, &mut sink).expect("nav parses");
        let titles: StdVec<&str> = sink.0.iter().map(|t| t.as_str()).collect();
        assert_eq!(titles, ["One", "Two"]);

        // The firmware builds its TOC through the streamed parser, fed in
        // whatever chunks the inflater hands it.
        for chunk_len in [1, 7, 64, nav.len()] {
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = Epub3NavStreamParser::new();
            let mut sink = TitleSink(Vec::new());
            for chunk in nav.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_nav(chunk, &mut parser, &mut sink)
                    .expect("nav streams");
            }
            tokenizer
                .finish_nav(&mut parser, &mut sink)
                .expect("nav streams");
            let titles: StdVec<&str> = sink.0.iter().map(|t| t.as_str()).collect();
            assert_eq!(titles, ["One", "Two"], "chunk_len={chunk_len}");
        }
    }

    #[test]
    fn epub3_nav_stream_skips_what_the_slice_parser_skips() {
        struct Titles(StdVec<std::string::String>);
        impl EpubTocSink for Titles {
            fn push_toc(&mut self, title: &str, _href: &str, _level: u8) -> Result<(), TocError> {
                self.0.push(title.into());
                Ok(())
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a href="one.xhtml">One</a></li>
                <svg><a href="art.xhtml">Art</a><svg><a href="inner-art.xhtml">Inner art</a></svg>
                  <a href="art-tail.xhtml">Art tail</a></svg>
                <script><a href="script.xhtml">Script</a></script>
                <li><span aria-hidden="true"/><a href="two.xhtml">Two</a></li>
                <DIV aria-hidden="true"><a href="upper.xhtml">Upper</a></div>
                <li title="CSS example: display:none"><a href="css.xhtml">CSS Examples</a></li>
                <li style="DISPLAY: none"><a href="styled.xhtml">Styled away</a></li>
                <li style="display:none/* comment */"><a href="draft.xhtml">Draft</a></li>
                <li style="visibility:hidden/* comment */"><a href="note.xhtml">Note</a></li>
                <li style="--marker:'/*';display:none"><a href="marker.xhtml">Marker</a></li>
                <li style="display:none; display:block"><a href="shown.xhtml">Shown</a></li>
                <li style="display:none; all:initial"><a href="reset.xhtml">Reset</a></li>
                <li><a href="three.xhtml">Three</a></li>
              </ol></nav>
            </body></html>
        "#;

        let mut slice = Titles(StdVec::new());
        parse_epub3_nav_to_sink(nav, &mut slice).expect("slice parses");
        let mut stream = Titles(StdVec::new());
        parse_epub3_nav_stream(&mut SliceByteStream::new(nav.as_bytes()), &mut stream)
            .expect("stream parses");
        assert_eq!(
            slice.0,
            ["One", "Two", "CSS Examples", "Shown", "Reset", "Three"]
        );
        assert_eq!(stream.0, slice.0);
    }

    #[test]
    fn epub3_nav_hidden_nesting_overflow_is_a_recoverable_error() {
        struct NoSink;
        impl EpubTocSink for NoSink {
            fn push_toc(&mut self, _t: &str, _h: &str, _l: u8) -> Result<(), TocError> {
                panic!("nothing in the hidden subtree is a TOC entry");
            }
        }

        let mut nav = std::string::String::from(r#"<body><nav><ol><div aria-hidden="true">"#);
        for _ in 1..u8::MAX {
            nav.push_str("<div>");
        }
        assert_eq!(parse_epub3_nav_to_sink(&nav, &mut NoSink), Ok(()));
        assert_eq!(
            parse_epub3_nav_stream(&mut SliceByteStream::new(nav.as_bytes()), &mut NoSink),
            Ok(())
        );
        nav.push_str("<div>");
        assert_eq!(
            parse_epub3_nav_to_sink(&nav, &mut NoSink),
            Err(TocError::NestingTooDeep)
        );
        assert_eq!(
            parse_epub3_nav_stream(&mut SliceByteStream::new(nav.as_bytes()), &mut NoSink),
            Err(TocStreamError::Toc(TocError::NestingTooDeep))
        );
    }

    #[test]
    fn epub2_ncx_stream_handles_ncx_larger_than_any_scratch_buffer() {
        // Build a synthetic NCX with 200 chapters of descriptive titles —
        // roughly 60 KB of XML, comfortably larger than any realistic in-RAM
        // scratch on the device. The streaming parser should still surface
        // every entry because peak memory is bounded by a single tag/title,
        // not the file size.
        use core::fmt::Write as _;
        let mut xml =
            std::string::String::from("<?xml version='1.0' encoding='utf-8'?><ncx><navMap>");
        for index in 0..200 {
            write!(
                xml,
                "<navPoint><navLabel><text>Chapter {:03}: The Long Title That Inflates File Size</text></navLabel><content src=\"chapter_{:03}.xhtml\"/></navPoint>",
                index, index
            )
            .unwrap();
        }
        xml.push_str("</navMap></ncx>");
        assert!(xml.len() > 24 * 1024, "fixture must exceed XHTML scratch");

        struct Sink {
            count: usize,
            last_title: heapless::String<128>,
        }
        impl EpubTocSink for Sink {
            fn push_toc(&mut self, title: &str, _href: &str, _level: u8) -> Result<(), TocError> {
                self.last_title.clear();
                let _ = self.last_title.push_str(title);
                self.count += 1;
                Ok(())
            }
        }

        let mut stream = SliceByteStream::new(xml.as_bytes());
        let mut sink = Sink {
            count: 0,
            last_title: heapless::String::new(),
        };
        parse_epub2_ncx_stream(&mut stream, &mut sink).expect("large ncx parses");

        assert_eq!(sink.count, 200);
        assert!(sink.last_title.as_str().starts_with("Chapter 199"));
    }

    #[test]
    fn epub2_ncx_stream_survives_tiny_read_chunks() {
        struct CountingSink(usize);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, _t: &str, _h: &str, _l: u8) -> Result<(), TocError> {
                self.0 += 1;
                Ok(())
            }
        }

        // Read one byte at a time to exercise tag-spanning-chunk-boundary.
        struct OneByteStream<'a> {
            bytes: &'a [u8],
            cursor: usize,
        }
        impl ByteStream for OneByteStream<'_> {
            type Error = ();
            fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
                if self.cursor >= self.bytes.len() || out.is_empty() {
                    return Ok(0);
                }
                out[0] = self.bytes[self.cursor];
                self.cursor += 1;
                Ok(1)
            }
        }

        let ncx = r#"<ncx><navMap>
            <navPoint><navLabel><text>One</text></navLabel><content src="a.xhtml"/></navPoint>
            <navPoint><navLabel><text>Two</text></navLabel><content src="b.xhtml"/></navPoint>
            <navPoint><navLabel><text>Three</text></navLabel><content src="c.xhtml"/></navPoint>
        </navMap></ncx>"#;

        let mut stream = OneByteStream {
            bytes: ncx.as_bytes(),
            cursor: 0,
        };
        let mut sink = CountingSink(0);
        parse_epub2_ncx_stream(&mut stream, &mut sink).expect("byte-by-byte parses");
        assert_eq!(sink.0, 3);
    }

    #[test]
    fn opf_parses_only_manifest_and_spine_sections() {
        let opf = r#"
            <package>
              <metadata><dc:title>The Time Machine</dc:title><dc:creator>H. G. Wells</dc:creator></metadata>
              <guide><reference type="cover" title="Cover" href="wrap0000.html"/><reference type="text" title="Start" href="text/start.xhtml#p1"/></guide>
              <manifest>
                <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
                <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
                <item id="chap1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
              </manifest>
              <spine toc="ncx"><itemref idref="chap1"/></spine>
              <item id="outside" href="wrong.xhtml" media-type="application/xhtml+xml"/>
            </package>
        "#;

        let mut arena = [0u8; 4096];
        let package = parse_opf(
            opf.as_bytes(),
            &mut arena,
            BookId(9),
            "/books/time.epub",
            42,
            "OPS/content.opf",
        )
        .expect("opf parses");

        assert_eq!(package.spine.len(), 1);
        assert_eq!(
            package.spine.get(0).unwrap().href.of(package.opf_text),
            "text/ch1.xhtml"
        );
        assert_eq!(package.text_reference_href, Some("text/start.xhtml"));
        assert_eq!(package.cover_page_href, Some("wrap0000.html"));
        assert_eq!(package.nav_href, Some("nav.xhtml"));
        assert_eq!(package.ncx_href, Some("toc.ncx"));
    }

    #[test]
    fn opf_falls_back_to_xhtml_manifest_when_spine_refs_do_not_resolve() {
        let opf = r#"
            <package>
              <metadata><dc:title>Loose Book</dc:title></metadata>
              <manifest>
                <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
                <item id="chap1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
                <item id="style" href="book.css" media-type="text/css"/>
              </manifest>
              <spine><itemref idref="missing"/></spine>
            </package>
        "#;

        let mut arena = [0u8; 4096];
        let package = parse_opf(
            opf.as_bytes(),
            &mut arena,
            BookId(11),
            "/books/loose.epub",
            42,
            "OPS/content.opf",
        )
        .expect("opf parses");

        assert_eq!(package.spine.len(), 1);
        assert_eq!(
            package.spine.get(0).unwrap().href.of(package.opf_text),
            "text/ch1.xhtml"
        );
    }

    /// An OPF with `chapters` spine items plus `extra` manifest-only items
    /// (images, CSS) that the reader never needs.
    fn large_opf(chapters: usize, extra: usize) -> std::string::String {
        let mut opf = std::string::String::from(
            "<?xml version=\"1.0\"?><package><metadata>\
             <dc:title>Pride &amp; Prejudice \u{2014} \u{dc}ber</dc:title>\
             <dc:creator>Jane Austen</dc:creator></metadata><manifest>",
        );
        for index in 0..extra {
            opf.push_str(&std::format!(
                "<item id=\"img{index}\" href=\"images/illustration-{index:05}.jpg\" \
                 media-type=\"image/jpeg\"/>\n"
            ));
        }
        for index in 0..chapters {
            opf.push_str(&std::format!(
                "<item id=\"ch{index}\" href=\"text/chapter-{index:04}.xhtml\" \
                 media-type=\"application/xhtml+xml\"/>\n"
            ));
        }
        opf.push_str("</manifest><spine toc=\"ncx\">");
        for index in 0..chapters {
            opf.push_str(&std::format!("<itemref idref=\"ch{index}\"/>"));
        }
        opf.push_str("</spine><guide><reference type=\"text\" href=\"text/chapter-0002.xhtml#x\"/></guide></package>");
        opf
    }

    fn parse_streamed_in_chunks<'a>(
        opf: &str,
        chunk: usize,
        arena: &'a mut [u8],
    ) -> Result<EpubPackage<'a>, EpubError> {
        parse_opf_streamed(
            |feed| {
                for piece in opf.as_bytes().chunks(chunk) {
                    feed(piece)?;
                }
                Ok(())
            },
            arena,
            BookId(1),
            "/books/big.epub",
            0,
            "OPS/content.opf",
        )
    }

    #[test]
    fn test_capped_opf() {
        let mut manifest = std::string::String::new();
        let mut spine = std::string::String::new();
        for n in 0..682 {
            manifest.push_str(&std::format!(r#"<item id="c{n}" href="{n}.x"/>"#));
            spine.push_str(&std::format!(r#"<itemref idref="c{n}"/>"#));
        }
        let opf = std::format!(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:capped</dc:identifier><dc:title>Capped</dc:title></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
        );
        let mut arena = [0u8; 16_384];
        let package = parse_opf(opf.as_bytes(), &mut arena, BookId(1), "test", 0, "test.opf")
            .expect("parses");
        std::println!(
            "test_capped_opf: spine.len = {}, truncated = {}",
            package.spine.len(),
            package.spine_truncated
        );
        assert_eq!(package.spine.len(), 682);
    }

    #[test]
    fn opf_far_larger_than_the_arena_keeps_every_spine_item() {
        let opf = large_opf(150, 1200);
        assert!(opf.len() > 100_000, "fixture should dwarf the arena");
        for chunk in [1usize, 37, 4096] {
            let mut arena = [0u8; 16_384];
            let package = parse_streamed_in_chunks(&opf, chunk, &mut arena).expect("parses");
            assert_eq!(package.spine.len(), 150, "chunk {chunk}");
            assert!(!package.spine_truncated);
            assert_eq!(
                package.spine.get(0).unwrap().href.of(package.opf_text),
                "text/chapter-0000.xhtml"
            );
            assert_eq!(
                package.spine.get(149).unwrap().href.of(package.opf_text),
                "text/chapter-0149.xhtml"
            );
            assert_eq!(package.meta.title, "Pride & Prejudice \u{2014} \u{dc}ber");
            assert_eq!(package.meta.author, "Jane Austen");
            assert_eq!(package.text_reference_href, Some("text/chapter-0002.xhtml"));
        }
    }

    #[test]
    fn opf_spine_past_the_arena_is_flagged_truncated() {
        let opf = large_opf(1500, 0);
        let mut arena = [0u8; 16_384];
        let package = parse_streamed_in_chunks(&opf, 512, &mut arena).expect("parses");
        assert!(package.spine.len() > 300 && package.spine.len() < 1500);
        assert!(package.spine_truncated);
        // What was kept is the head of the spine, in order.
        let last = package.spine.len() - 1;
        assert_eq!(
            package.spine.get(last).unwrap().href.of(package.opf_text),
            std::format!("text/chapter-{last:04}.xhtml")
        );
    }

    #[test]
    fn a_spine_of_hundreds_of_items_fits_the_device_arena() {
        // An essay collection: 600 items, each its own XHTML file, with
        // bulky manifest ids and hundreds of images the spine never names.
        let mut opf = std::string::String::from(
            r#"<package><metadata><dc:title>Essays</dc:title></metadata><manifest>"#,
        );
        for index in 0..600 {
            opf.push_str(&std::format!(
                r#"<item id="essay-{index:04}-xhtml-content-document" href="text/essay-{index:04}.xhtml" media-type="application/xhtml+xml"/>"#
            ));
            opf.push_str(&std::format!(
                r#"<item id="figure-{index:04}" href="images/figure-{index:04}.png" media-type="image/png"/>"#
            ));
        }
        opf.push_str("</manifest><spine>");
        for index in (0..600).rev() {
            opf.push_str(&std::format!(
                r#"<itemref idref="essay-{index:04}-xhtml-content-document"/>"#
            ));
        }
        // A dangling itemref is skipped, as before.
        opf.push_str(r#"<itemref idref="missing"/></spine></package>"#);
        let mut arena = [0u8; 20 * 1024];
        let package = parse_streamed_in_chunks(&opf, 777, &mut arena).expect("parses");
        assert!(!package.spine_truncated);
        assert_eq!(package.spine.len(), 600);
        // Spine order, not manifest order.
        let href = |index: usize| package.spine.get(index).unwrap().href.of(package.opf_text);
        assert_eq!(href(0), "text/essay-0599.xhtml");
        assert_eq!(href(599), "text/essay-0000.xhtml");
        assert_eq!(
            package
                .spine
                .get(3)
                .unwrap()
                .media_type
                .of(package.opf_text),
            "application/xhtml+xml"
        );
    }

    #[test]
    fn opf_arena_exhaustion_is_flagged_not_silent() {
        let opf = large_opf(40, 0);
        let mut arena = [0u8; 600];
        let package = parse_streamed_in_chunks(&opf, 64, &mut arena).expect("parses");
        assert!(package.spine.len() < 40);
        assert!(package.spine_truncated);
    }

    #[test]
    fn xhtml_skips_pagebreaks_comments_and_aria_hidden() {
        let xhtml = r#"
            <body>
              <!-- comment should not be text -->
              <?xml-stylesheet href="x.css"?>
              <span role="doc-pagebreak" aria-label="12">12</span>
              <span aria-hidden="true">hidden</span>
              <p>Visible&nbsp;text &amp; symbols</p>
            </body>
        "#;
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Visible\u{a0}text & symbols");
    }

    #[test]
    fn xhtml_emits_ordered_list_markers() {
        let xhtml = "<body><ol><li>One</li><li>Two</li></ol></body>";
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks[0].text, "1. One");
        assert_eq!(blocks[1].text, "2. Two");
    }

    #[test]
    fn xhtml_sink_splits_long_paragraph_without_dropping_words() {
        struct CountingSink {
            blocks: usize,
            words: usize,
        }
        impl XhtmlBlockSink for CountingSink {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                paragraph_end: bool,
                _logical_offset: u32,
            ) -> Result<(), XhtmlError> {
                if text.is_empty() {
                    return Ok(());
                }
                self.blocks += 1;
                self.words += text.split_ascii_whitespace().count();
                assert!(self.blocks > 1 || !paragraph_end);
                Ok(())
            }
        }

        let mut xhtml = std::string::String::from("<body><p>");
        for _ in 0..180 {
            xhtml.push_str("word ");
        }
        xhtml.push_str("</p></body>");
        let mut sink = CountingSink {
            blocks: 0,
            words: 0,
        };

        xhtml_blocks_to_sink(&xhtml, None, &mut sink).expect("sink accepts chunks");

        assert!(sink.blocks > 1);
        assert_eq!(sink.words, 180);
    }

    #[test]
    fn xhtml_sink_does_not_split_word_when_buffer_flushes() {
        struct CollectingSink {
            text: std::string::String,
        }
        impl XhtmlBlockSink for CollectingSink {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                _paragraph_end: bool,
                _logical_offset: u32,
            ) -> Result<(), XhtmlError> {
                self.text.push_str(text);
                Ok(())
            }
        }

        let mut xhtml = std::string::String::from("<body><p>");
        for _ in 0..80 {
            xhtml.push_str("filler ");
        }
        xhtml.push_str("Space, and a fourth, Time.</p></body>");
        let mut sink = CollectingSink {
            text: std::string::String::new(),
        };

        xhtml_blocks_to_sink(&xhtml, None, &mut sink).expect("sink accepts chunks");

        assert!(sink.text.contains("Space, and a fourth, Time."));
        assert!(!sink.text.contains(" a nd "));
    }

    #[test]
    fn xhtml_sink_does_not_split_inline_word_when_buffer_flushes() {
        struct CollectingSink {
            text: std::string::String,
        }
        impl XhtmlBlockSink for CollectingSink {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                _paragraph_end: bool,
                _logical_offset: u32,
            ) -> Result<(), XhtmlError> {
                self.text.push_str(text);
                Ok(())
            }
        }

        let mut xhtml = std::string::String::from("<body><p>");
        for _ in 0..80 {
            xhtml.push_str("filler ");
        }
        xhtml.push_str("Space, <em>a</em>nd a fourth, Time.</p></body>");
        let mut sink = CollectingSink {
            text: std::string::String::new(),
        };

        xhtml_blocks_to_sink(&xhtml, None, &mut sink).expect("sink accepts chunks");

        assert!(sink.text.contains("Space, and a fourth, Time."));
        assert!(!sink.text.contains(" a nd "));
    }

    #[test]
    fn xhtml_keeps_inline_italic_inside_paragraph_block() {
        let xhtml = "<body><p>You must <em>follow</em> me carefully.</p></body>";
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "You must follow me carefully.");
        assert_eq!(blocks[0].role, TextRole::Body);
    }

    #[test]
    fn xhtml_attaches_punctuation_after_inline_tag() {
        let xhtml = "<body><p>chairs, being his <em>patents</em>, embraced</p></body>";
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "chairs, being his patents, embraced");
    }

    #[test]
    fn xhtml_does_not_split_word_across_inline_tag() {
        let xhtml = "<body><p>Space, <em>a</em>nd a fourth, Time.</p></body>";
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Space, and a fourth, Time.");
    }

    #[test]
    fn zip_rejects_missing_entry() {
        let zip_bytes = stored_zip(&[("hello.txt", b"hi".as_slice())]);
        let archive = ZipArchive::new(&zip_bytes).expect("zip parses");

        assert_eq!(archive.find("missing.txt"), Err(ZipError::EntryNotFound));
    }

    #[test]
    fn zip_rejects_malformed_central_directory() {
        assert_eq!(
            ZipArchive::new(b"not a zip file").err(),
            Some(ZipError::MissingEndOfCentralDirectory)
        );
    }

    #[test]
    fn zip_reads_stored_entry() {
        let zip_bytes = stored_zip(&[("META-INF/container.xml", b"<container/>".as_slice())]);
        let archive = ZipArchive::new(&zip_bytes).expect("zip parses");
        let entry = archive
            .find("META-INF/container.xml")
            .expect("entry exists");
        let mut output = [0u8; 32];

        let len = archive.read_entry(entry, &mut output).expect("stored read");

        assert_eq!(&output[..len], b"<container/>");
    }

    #[test]
    fn zip_stream_reads_stored_entry_by_offset() {
        let zip_bytes = stored_zip(&[("OPS/package.opf", b"<package/>".as_slice())]);
        let mut tail = [0u8; 512];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/package.opf", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 64];
        let mut output = [0u8; 64];

        let len = stream
            .read_entry(entry, &mut compressed, &mut output)
            .expect("entry read");

        assert_eq!(&output[..len], b"<package/>");
    }

    #[test]
    fn zip_stream_read_entry_to_sink_streams_full_inflate_output() {
        // 1.8 KB of plain text compressed to 54 bytes — small input, large
        // output. The window holds only 32 bytes, so the inflate has to keep
        // flushing buffered output many times after the compressed input is
        // fully consumed. Catches the "stop after first chunk" bug where the
        // inner loop exits on consumed==input_len even when flush=Finish has
        // more buffered output to drain.
        let plain: std::vec::Vec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
            .iter()
            .copied()
            .cycle()
            .take(1800)
            .collect();
        let deflated: &[u8] = &[
            75, 76, 74, 78, 73, 77, 75, 207, 200, 204, 202, 206, 201, 205, 203, 47, 40, 44, 42, 46,
            41, 45, 43, 175, 168, 172, 50, 48, 52, 50, 54, 49, 53, 51, 183, 176, 76, 28, 85, 51,
            170, 102, 84, 205, 168, 154, 81, 53, 163, 106, 134, 169, 26, 0,
        ];
        let zip_bytes = zip_with_methods(&[("OPS/long.xhtml", 8, deflated, &plain)]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/long.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 512];
        let mut window = [0u8; 32];
        let mut inflate = inflate_scratch();
        let mut collected: std::vec::Vec<u8> = std::vec::Vec::new();

        stream
            .read_entry_to_sink(entry, &mut compressed, &mut window, &mut inflate, |chunk| {
                collected.extend_from_slice(chunk);
                Ok(())
            })
            .expect("entry streams");

        assert_eq!(collected, plain);
    }

    #[test]
    fn zip_local_stream_read_entry_to_sink_streams_full_inflate_output() {
        // Forward-only twin of the ZipStream regression test above, with a
        // 16-byte input scratch on top so both dimensions chunk: compressed
        // bytes arrive across several fetches and the 32-byte window drains
        // many times per fetch.
        let plain: StdVec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
            .iter()
            .copied()
            .cycle()
            .take(1800)
            .collect();
        let deflated: &[u8] = &[
            75, 76, 74, 78, 73, 77, 75, 207, 200, 204, 202, 206, 201, 205, 203, 47, 40, 44, 42, 46,
            41, 45, 43, 175, 168, 172, 50, 48, 52, 50, 54, 49, 53, 51, 183, 176, 76, 28, 85, 51,
            170, 102, 84, 205, 168, 154, 81, 53, 163, 106, 134, 169, 26, 0,
        ];
        let zip_bytes = zip_with_methods(&[("OPS/long.xhtml", 8, deflated, &plain)]);
        let mut stream = ZipLocalStream::new(SliceStream {
            bytes: &zip_bytes,
            cursor: 0,
        });
        let entry = stream
            .find_entry("OPS/long.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 16];
        let mut window = [0u8; 32];
        let mut inflate = inflate_scratch();
        let mut collected: StdVec<u8> = StdVec::new();

        stream
            .read_entry_to_sink(entry, &mut compressed, &mut window, &mut inflate, |chunk| {
                collected.extend_from_slice(chunk);
                Ok(())
            })
            .expect("entry streams");

        assert_eq!(collected, plain);
    }

    #[test]
    fn zip_stream_read_entry_to_sink_drains_entries_larger_than_inflate_dictionary() {
        // 100 KB decompressed cycles the 32 KiB ring several times, in
        // partial pushes that do not line up with input chunk boundaries.
        // Catches the engine treating "no input consumed but output
        // written", or a ring-end stop, as failure.
        //
        // The 33-byte buffer no longer *stages* the deflate path — the
        // scratch's own ring does — but it still bounds it: nothing is
        // copied through the buffer, and no emitted chunk may exceed it.
        // Deflated bytes reach `emit` straight out of a 32 KiB ring, so
        // that bound holds only while the engine caps each decode by it.
        let plain: StdVec<u8> = (0u32..100_000)
            .map(|value| (value.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let deflated = miniz_oxide::deflate::compress_to_vec(&plain, 6);
        let zip_bytes = zip_with_methods(&[("OPS/huge.xhtml", 8, &deflated, &plain)]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/huge.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 1000];
        let mut window = [0u8; 33];
        let mut inflate = inflate_scratch();
        let mut collected: StdVec<u8> = StdVec::new();
        let mut widest_chunk = 0usize;

        stream
            .read_entry_to_sink(entry, &mut compressed, &mut window, &mut inflate, |chunk| {
                widest_chunk = widest_chunk.max(chunk.len());
                collected.extend_from_slice(chunk);
                Ok(())
            })
            .expect("entry streams");

        assert_eq!(collected, plain);
        assert!(
            widest_chunk <= window.len(),
            "emitted a {widest_chunk}-byte chunk into a {}-byte window",
            window.len()
        );
    }

    #[test]
    fn zip_stream_to_sink_rejects_a_zero_length_output_window() {
        // Zero makes no progress on either path, and the stored path would
        // spin on it forever: a read of zero bytes never shrinks `remaining`.
        let plain = b"whatever".as_slice();
        let deflated = miniz_oxide::deflate::compress_to_vec(plain, 6);
        let zip_bytes = zip_with_methods(&[
            ("OPS/stored.xhtml", 0, plain, plain),
            ("OPS/deflated.xhtml", 8, &deflated, plain),
        ]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let mut inflate = inflate_scratch();

        for name in ["OPS/stored.xhtml", "OPS/deflated.xhtml"] {
            let entry = stream
                .find_entry(name, &mut [0u8; 46], &mut [0u8; 64])
                .expect("entry exists");
            let result =
                stream.read_entry_to_sink(entry, &mut [0u8; 64], &mut [], &mut inflate, |_| Ok(()));
            assert!(
                matches!(result, Err(ZipError::OutputTooSmall)),
                "{name} accepted an empty window: {result:?}"
            );
        }
    }

    #[test]
    fn zip_stream_does_not_decode_an_entry_against_the_previous_entrys_window() {
        // The 32 KiB ring is deliberately not cleared between entries, so
        // nothing but the decode flags stops a malformed entry from reading
        // what the last one left there. In miniz's wrapping mode a distance
        // is rejected only when it exceeds the whole ring, and the source is
        // computed by wrapping subtraction — so a first-operation match at
        // distance 32768 resolves to ring offset 0 and copies out the
        // previous entry. Zip deflate has no preset dictionary; no legal
        // entry reaches back before its own first byte.
        const MARKER: &[u8] = b"SECRET-PREVIOUS-ENTRY-BYTES\n";
        let mut marker_plain: StdVec<u8> = StdVec::new();
        while marker_plain.len() < 4096 {
            marker_plain.extend_from_slice(MARKER);
        }
        let marker_deflated = miniz_oxide::deflate::compress_to_vec(&marker_plain, 6);
        // Four maximal matches: 1032 bytes of whatever sits at the ring's
        // front, which is where the entry above left its marker.
        let attack_deflated = deflate_of_max_distance_matches(4);
        let attack_plain = [0u8; 4 * 258];

        let zip_bytes = zip_with_methods(&[
            ("OPS/first.xhtml", 8, &marker_deflated, &marker_plain),
            ("OPS/attack.xhtml", 8, &attack_deflated, &attack_plain),
        ]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let mut compressed = [0u8; 512];
        let mut window = [0u8; 512];
        let mut inflate = inflate_scratch();

        let first = stream
            .find_entry("OPS/first.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut collected: StdVec<u8> = StdVec::new();
        stream
            .read_entry_to_sink(first, &mut compressed, &mut window, &mut inflate, |chunk| {
                collected.extend_from_slice(chunk);
                Ok(())
            })
            .expect("the first entry streams");
        assert_eq!(collected, marker_plain, "the ring should hold the marker");

        let attack = stream
            .find_entry("OPS/attack.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut leaked: StdVec<u8> = StdVec::new();
        let result = stream.read_entry_to_sink(
            attack,
            &mut compressed,
            &mut window,
            &mut inflate,
            |chunk| {
                leaked.extend_from_slice(chunk);
                Ok(())
            },
        );

        assert!(
            !leaked.windows(MARKER.len()).any(|run| run == MARKER),
            "the second entry emitted the first entry's bytes"
        );
        assert!(
            matches!(result, Err(ZipError::Inflate)),
            "a match reaching before the stream's first byte must fail it, got {result:?}"
        );
    }

    #[test]
    fn zip_stream_prefix_does_not_decode_against_the_previous_entrys_window() {
        // Same leak, through the prefix path, which runs its own decode loop.
        const MARKER: &[u8] = b"SECRET-PREFIX-ENTRY-BYTES\n";
        let mut marker_plain: StdVec<u8> = StdVec::new();
        while marker_plain.len() < 4096 {
            marker_plain.extend_from_slice(MARKER);
        }
        let marker_deflated = miniz_oxide::deflate::compress_to_vec(&marker_plain, 6);
        let attack_deflated = deflate_of_max_distance_matches(4);
        let attack_plain = [0u8; 4 * 258];

        let zip_bytes = zip_with_methods(&[
            ("OPS/first.xhtml", 8, &marker_deflated, &marker_plain),
            ("OPS/attack.xhtml", 8, &attack_deflated, &attack_plain),
        ]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let mut compressed = [0u8; 512];
        let mut inflate = inflate_scratch();

        let first = stream
            .find_entry("OPS/first.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut resident = [0u8; 4200];
        let len = stream
            .read_entry_streamed(first, &mut compressed, &mut resident, &mut inflate)
            .expect("the first entry reads");
        assert_eq!(&resident[..len], &marker_plain[..]);

        let attack = stream
            .find_entry("OPS/attack.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut leaked = [0u8; 4 * 258];
        let result = stream.read_entry_streamed(attack, &mut compressed, &mut leaked, &mut inflate);

        assert!(
            !leaked.windows(MARKER.len()).any(|run| run == MARKER),
            "the prefix read emitted the first entry's bytes"
        );
        assert!(
            matches!(result, Err(ZipError::Inflate)),
            "a match reaching before the stream's first byte must fail it, got {result:?}"
        );
    }

    /// A raw-deflate stream whose whole content is `repeats` back-references
    /// at distance 32768 — the format's maximum — each 258 bytes long. From
    /// the first byte of a stream that reaches back before anything it has
    /// produced, so a decoder that honours the distance is reading somebody
    /// else's memory. Hand-built because no compressor will emit one.
    fn deflate_of_max_distance_matches(repeats: usize) -> StdVec<u8> {
        let mut bits = BitWriter::default();
        bits.push_bits(1, 1); // BFINAL
        bits.push_bits(1, 2); // BTYPE = 01, fixed Huffman
        for _ in 0..repeats {
            // Fixed literal/length code 285 — length 258, no extra bits — is
            // the 8-bit pattern 1100_0101.
            bits.push_code(0b1100_0101, 8);
            // Distance codes are plain 5-bit values; 29 covers 24577..=32768
            // with 13 extra bits carrying the offset into that range.
            bits.push_code(29, 5);
            bits.push_bits(32_768 - 24_577, 13);
        }
        bits.push_code(0, 7); // end of block, code 256
        bits.finish()
    }

    /// Just enough of a deflate bit packer for the fixture above.
    #[derive(Default)]
    struct BitWriter {
        bytes: StdVec<u8>,
        partial: u8,
        filled: u32,
    }

    impl BitWriter {
        /// Deflate packs plain integer fields low bit first.
        fn push_bits(&mut self, value: u32, count: u32) {
            for index in 0..count {
                self.push_bit((value >> index) & 1);
            }
        }

        /// Huffman codes go the other way: high bit of the code first.
        fn push_code(&mut self, code: u32, count: u32) {
            for index in (0..count).rev() {
                self.push_bit((code >> index) & 1);
            }
        }

        fn push_bit(&mut self, bit: u32) {
            self.partial |= (bit as u8) << self.filled;
            self.filled += 1;
            if self.filled == 8 {
                self.bytes.push(self.partial);
                self.partial = 0;
                self.filled = 0;
            }
        }

        fn finish(mut self) -> StdVec<u8> {
            if self.filled > 0 {
                self.bytes.push(self.partial);
            }
            self.bytes
        }
    }

    #[test]
    fn zip_stream_resolves_matches_that_reach_back_across_the_ring_wrap() {
        // The decoder's output buffer *is* its LZ77 window, so a match whose
        // distance reaches back past the ring's physical end has to read the
        // bytes that wrapped to the front. Near-random data (the test above)
        // barely exercises that: it compresses into short, close matches.
        //
        // A 30 KB random block repeated eight times does. Every repeat is one
        // match at distance 30,720 — just under the 32 KiB ceiling — so the
        // matches are long, near-maximal, and land at every offset relative
        // to the wrap. A ring the engine indexed wrongly returns garbage
        // here while still returning `Done`.
        let block: StdVec<u8> = (0u32..30_720)
            .map(|value| (value.wrapping_mul(2_246_822_519) >> 11) as u8)
            .collect();
        let mut plain: StdVec<u8> = StdVec::new();
        for _ in 0..8 {
            plain.extend_from_slice(&block);
        }
        let deflated = miniz_oxide::deflate::compress_to_vec(&plain, 6);
        assert!(
            deflated.len() < plain.len() / 4,
            "fixture must actually produce long matches, got {} from {}",
            deflated.len(),
            plain.len()
        );
        let zip_bytes = zip_with_methods(&[("OPS/repeat.xhtml", 8, &deflated, &plain)]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/repeat.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        // A compressed chunk far smaller than the ring, so decode resumes
        // mid-match at offsets unrelated to the wrap.
        let mut compressed = [0u8; 512];
        let mut window = [0u8; 64];
        let mut inflate = inflate_scratch();
        let mut collected: StdVec<u8> = StdVec::new();

        stream
            .read_entry_to_sink(entry, &mut compressed, &mut window, &mut inflate, |chunk| {
                collected.extend_from_slice(chunk);
                Ok(())
            })
            .expect("entry streams");

        assert_eq!(collected.len(), plain.len());
        assert!(collected == plain, "decoded bytes diverged from the source");
    }

    #[test]
    fn zip_stream_prefix_keeps_window_history_beyond_its_output_buffer() {
        // The prefix path caps how far the ring may run by the space left in
        // the caller's buffer, which is smaller than the window. History has
        // to survive that cap: a match reaching back further than `output`
        // is long must still resolve, because the ring holds it even though
        // the output buffer does not.
        let block: StdVec<u8> = (0u32..20_000)
            .map(|value| (value.wrapping_mul(2_654_435_761) >> 15) as u8)
            .collect();
        let mut plain: StdVec<u8> = StdVec::new();
        for _ in 0..3 {
            plain.extend_from_slice(&block);
        }
        let deflated = miniz_oxide::deflate::compress_to_vec(&plain, 6);
        let zip_bytes = zip_with_methods(&[("OPS/prefix.xhtml", 8, &deflated, &plain)]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/prefix.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 512];
        // Smaller than one block, so the second repeat's matches reach back
        // past everything this buffer can hold.
        let mut output = [0u8; 25_000];
        let mut inflate = inflate_scratch();

        let (len, complete) = stream
            .read_entry_prefix_streamed(entry, &mut compressed, &mut output, &mut inflate)
            .expect("prefix reads");

        assert_eq!(len, output.len(), "the prefix should fill its buffer");
        assert!(!complete, "the entry is larger than the buffer");
        assert!(
            output[..len] == plain[..len],
            "the prefix diverged from the source"
        );
    }

    #[test]
    fn zip_local_stream_prefix_stop_leaves_cursor_on_next_entry() {
        // A bounded prefix read stops mid-entry; the unread payload must be
        // skipped so the forward-only cursor lands on the next local header.
        let plain: StdVec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
            .iter()
            .copied()
            .cycle()
            .take(1800)
            .collect();
        let deflated: &[u8] = &[
            75, 76, 74, 78, 73, 77, 75, 207, 200, 204, 202, 206, 201, 205, 203, 47, 40, 44, 42, 46,
            41, 45, 43, 175, 168, 172, 50, 48, 52, 50, 54, 49, 53, 51, 183, 176, 76, 28, 85, 51,
            170, 102, 84, 205, 168, 154, 81, 53, 163, 106, 134, 169, 26, 0,
        ];
        let zip_bytes = zip_with_methods(&[
            ("OPS/big.xhtml", 8, deflated, &plain),
            ("OPS/next.xhtml", 0, b"tail entry", b"tail entry"),
        ]);
        let mut stream = ZipLocalStream::new(SliceStream {
            bytes: &zip_bytes,
            cursor: 0,
        });
        let entry = stream
            .find_entry("OPS/big.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("big entry exists");
        let mut compressed = [0u8; 16];
        let mut prefix = [0u8; 64];
        let mut inflate = inflate_scratch();

        let (len, complete) = stream
            .read_entry_prefix_streamed(entry, &mut compressed, &mut prefix, &mut inflate)
            .expect("prefix reads");
        assert_eq!(len, prefix.len());
        assert!(!complete);
        assert_eq!(&prefix[..len], &plain[..len]);

        let next = stream
            .find_entry("OPS/next.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("next entry found after prefix stop");
        let mut out = [0u8; 32];
        let read = stream
            .read_entry_streamed(next, &mut compressed, &mut out, &mut inflate)
            .expect("stored entry reads");
        assert_eq!(&out[..read], b"tail entry");
    }

    #[test]
    fn zip_stream_inflates_deflated_entry_in_small_chunks() {
        let plain = b"Large compressed member ".repeat(20);
        let deflated: &[u8] = &[
            243, 73, 44, 74, 79, 85, 72, 206, 207, 45, 40, 74, 45, 46, 78, 77, 81, 200, 77, 205,
            77, 74, 45, 82, 240, 25, 21, 31, 22, 226, 0,
        ];
        let zip_bytes = zip_with_methods(&[("OPS/chapter.xhtml", 8, deflated, plain.as_slice())]);
        let mut tail = [0u8; 512];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/chapter.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 8];
        let mut output = [0u8; 512];
        let mut inflate = inflate_scratch();

        let len = stream
            .read_entry_streamed(entry, &mut compressed, &mut output, &mut inflate)
            .expect("entry read");

        assert_eq!(&output[..len], plain.as_slice());
    }

    #[test]
    fn zip_stream_returns_partial_prefix_when_output_fills() {
        let zip_bytes = stored_zip(&[("OPS/chapter.xhtml", b"0123456789abcdef".as_slice())]);
        let mut tail = [0u8; 512];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/chapter.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 4];
        let mut output = [0u8; 8];
        let mut inflate = inflate_scratch();

        let (len, complete) = stream
            .read_entry_prefix_streamed(entry, &mut compressed, &mut output, &mut inflate)
            .expect("entry read");

        assert_eq!(len, 8);
        assert!(!complete);
        assert_eq!(&output, b"01234567");
    }

    #[test]
    fn streamed_xhtml_blocks_match_whole_document_parse_for_any_chunking() {
        // Rich enough to exercise every state-machine path: headings,
        // nested styles, lists, a table, br, img alt, skipped head/style/
        // script/nav/hidden content, entities, multi-byte UTF-8, and a
        // paragraph long enough to force a mid-block word-boundary flush.
        let mut long_para = std::string::String::new();
        for index in 0..80 {
            long_para.push_str(&std::format!("w{index:03}rd léng—thy "));
        }
        let xhtml = std::format!(
            r#"<?xml version="1.0"?><!DOCTYPE html><!-- comment -->
            <html><head><title>skip me</title><style>p {{ color: red; }}</style>
            <script>var x = "<p>not text</p>";</script></head>
            <body>
              <nav><ol><li>toc entry</li></ol></nav>
              <h1>Chapter &amp; Verse</h1>
              <p>Hello <em>soft &#233;</em> and <strong>bold <i>both</i></strong> end.</p>
              <p style="display:none">invisible</p>
              <blockquote>Quoted &hellip; text</blockquote>
              <ul><li>alpha</li><li>beta<ol><li>nested</li></ol></li></ul>
              <table style="margin-left: auto; margin-right: auto;">
                <tr><td>I</td><td>Intro 世界</td></tr>
              </table>
              <p>{long_para}</p>
              <p class="center">Centered<br/>after break</p>
              <img src="x.png" alt="A picture"/>
              <img src="y.png" alt="Café 世界"/>
              <p title="naïve">Titled</p>
              <span epub:type="pagebreak" title="12"></span>
            </body></html>"#
        );

        struct Recorder {
            fragments: StdVec<(std::string::String, TextRole, FontStyle, TextAlign, bool)>,
        }
        impl XhtmlBlockSink for Recorder {
            fn push_block(
                &mut self,
                text: &str,
                role: TextRole,
                style: FontStyle,
                align: TextAlign,
                paragraph_end: bool,
                _logical_offset: u32,
            ) -> Result<(), XhtmlError> {
                self.fragments
                    .push((text.into(), role, style, align, paragraph_end));
                Ok(())
            }
        }

        let mut whole = Recorder {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(&xhtml, None, &mut whole).expect("whole parse");
        assert!(whole.fragments.len() > 10, "doc should produce many blocks");

        for chunk_len in [1usize, 2, 3, 5, 7, 64, 4096] {
            let mut streamed = Recorder {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .expect("chunk feeds");
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .expect("finish");

            assert_eq!(
                whole.fragments, streamed.fragments,
                "chunk size {chunk_len} must not change emitted blocks"
            );
        }
    }

    /// A tag's bytes arrive one at a time and are read back as UTF-8, so an
    /// `alt` past ASCII comes out as the author wrote it. Read one byte per
    /// char it came out Latin-1: "CafÃ©", and a TOC href spelled that way
    /// matches no spine item.
    #[test]
    fn streamed_tag_attributes_keep_their_utf8() {
        let xhtml = r#"<body><img src="y.png" alt="Café 世界"/><p>text</p></body>"#;
        for chunk_len in [1usize, 2, 3, 4096] {
            let mut sink = RecordingSink {
                fragments: StdVec::new(),
            };
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
            assert_eq!(sink.fragments[0].0, "Café 世界", "chunk size {chunk_len}");
        }

        // Bytes that are not UTF-8 inside a tag are skipped, as they are in
        // text, and the tag still reads: the page-break marker hides its
        // number, the heading keeps its role, and the image its placeholder.
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };
        let mut tokenizer = StreamingXmlTokenizer::new();
        let mut parser = XhtmlBlockStreamParser::new(false);
        let mut bytes = StdVec::from(&b"<body><p>before</p>"[..]);
        bytes.extend_from_slice(b"<span epub:type=\"pagebreak\" title=\"p\xE9\">12</span>");
        bytes.extend_from_slice(b"<h1 id=\"chap\xEEtre\">Heading</h1>");
        bytes.extend_from_slice(b"<img alt=\"Caf\xC3\x28 au lait\"/><p>after</p></body>");
        tokenizer
            .feed_xhtml_blocks(&bytes, &mut parser, None, &mut sink)
            .expect("feeds");
        tokenizer
            .finish_xhtml_blocks(&mut parser, &mut sink)
            .expect("finish");
        let texts: StdVec<&str> = sink.fragments.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(texts, ["before", "Heading", "Caf( au lait", "after"]);
        assert_eq!(sink.fragments[1].1, TextRole::Heading1);
    }

    #[test]
    fn zip_stream_indexes_oversized_central_directory() {
        // 16 entries x (46-byte header + 20-byte name) > 1 KB of central
        // directory against a 1 KB tail scratch: the whole directory no
        // longer fits, but all 16 index records (20 bytes each) do.
        let names: StdVec<std::string::String> = (0..16)
            .map(|index| std::format!("OPS/chapter-{index:02}.xhtml"))
            .collect();
        let bodies: StdVec<std::string::String> = (0..16)
            .map(|index| std::format!("body {index:02}"))
            .collect();
        let files: StdVec<(&str, &[u8])> = names
            .iter()
            .zip(bodies.iter())
            .map(|(name, body)| (name.as_str(), body.as_bytes()))
            .collect();
        let zip_bytes = stored_zip(&files);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");

        for (name, body) in files.iter() {
            let entry = stream
                .find_entry(name, &mut [0u8; 46], &mut [0u8; 64])
                .expect("indexed entry found");
            let mut compressed = [0u8; 64];
            let mut output = [0u8; 64];
            let len = stream
                .read_entry(entry, &mut compressed, &mut output)
                .expect("entry read");
            assert_eq!(&output[..len], *body);
        }
        assert_eq!(
            stream
                .find_entry("OPS/missing.xhtml", &mut [0u8; 46], &mut [0u8; 64])
                .err(),
            Some(ZipError::EntryNotFound)
        );
    }

    #[test]
    fn zip_stream_partial_index_falls_back_to_the_walk() {
        // 128-byte scratch indexes only 6 of 10 entries; lookups past the
        // indexed prefix must still resolve through the directory walk.
        let names: StdVec<std::string::String> = (0..10)
            .map(|index| std::format!("OPS/chapter-{index:02}.xhtml"))
            .collect();
        let bodies: StdVec<std::string::String> = (0..10)
            .map(|index| std::format!("body {index:02}"))
            .collect();
        let files: StdVec<(&str, &[u8])> = names
            .iter()
            .zip(bodies.iter())
            .map(|(name, body)| (name.as_str(), body.as_bytes()))
            .collect();
        let zip_bytes = stored_zip(&files);
        let mut tail = [0u8; 128];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");

        for (name, body) in files.iter() {
            let entry = stream
                .find_entry(name, &mut [0u8; 46], &mut [0u8; 64])
                .expect("entry found through index or walk");
            let mut compressed = [0u8; 64];
            let mut output = [0u8; 64];
            let len = stream
                .read_entry(entry, &mut compressed, &mut output)
                .expect("entry read");
            assert_eq!(&output[..len], *body);
        }
        assert_eq!(
            stream
                .find_entry("OPS/missing.xhtml", &mut [0u8; 46], &mut [0u8; 64])
                .err(),
            Some(ZipError::EntryNotFound)
        );
    }

    /// A scratch with a decoder already adopted. `prepare` wants a `'static`
    /// decoder because firmware's lives in a `StaticCell`; a test leaks one,
    /// which costs nothing in a process that is about to exit.
    fn inflate_scratch() -> ZipInflateScratch {
        let mut scratch = ZipInflateScratch::new();
        scratch.prepare(StdBox::leak(StdBox::new(DecompressorOxide::new())));
        scratch
    }

    fn stored_zip(files: &[(&str, &[u8])]) -> StdVec<u8> {
        let entries: StdVec<_> = files
            .iter()
            .map(|(name, data)| (*name, 0, *data, *data))
            .collect();
        zip_with_methods(&entries)
    }

    fn zip_with_methods(files: &[(&str, u16, &[u8], &[u8])]) -> StdVec<u8> {
        let mut bytes = StdVec::new();
        let mut central = StdVec::new();
        let mut offsets = StdVec::new();

        for (name, method, compressed, plain) in files {
            offsets.push(bytes.len() as u32);
            push_u32(&mut bytes, 0x0403_4b50);
            push_u16(&mut bytes, 20);
            push_u16(&mut bytes, 0);
            push_u16(&mut bytes, *method);
            push_u16(&mut bytes, 0);
            push_u16(&mut bytes, 0);
            push_u32(&mut bytes, 0);
            push_u32(&mut bytes, compressed.len() as u32);
            push_u32(&mut bytes, plain.len() as u32);
            push_u16(&mut bytes, name.len() as u16);
            push_u16(&mut bytes, 0);
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(compressed);
        }

        for ((name, method, compressed, plain), offset) in files.iter().zip(offsets.iter()) {
            push_u32(&mut central, 0x0201_4b50);
            push_u16(&mut central, 20);
            push_u16(&mut central, 20);
            push_u16(&mut central, 0);
            push_u16(&mut central, *method);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u32(&mut central, 0);
            push_u32(&mut central, compressed.len() as u32);
            push_u32(&mut central, plain.len() as u32);
            push_u16(&mut central, name.len() as u16);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u32(&mut central, 0);
            push_u32(&mut central, *offset);
            central.extend_from_slice(name.as_bytes());
        }

        let central_offset = bytes.len() as u32;
        let central_size = central.len() as u32;
        bytes.extend_from_slice(&central);
        push_u32(&mut bytes, 0x0605_4b50);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, files.len() as u16);
        push_u16(&mut bytes, files.len() as u16);
        push_u32(&mut bytes, central_size);
        push_u32(&mut bytes, central_offset);
        push_u16(&mut bytes, 0);
        bytes
    }

    fn push_u16(bytes: &mut StdVec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(bytes: &mut StdVec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn test_load_container_xml_and_find_opf_path_requires_4096_capacity() {
        let mut container_content = StdVec::new();
        container_content.extend_from_slice(b"<?xml version=\"1.0\"?>\n<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n  <rootfiles>\n    <rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\"/>\n  </rootfiles>\n  <!-- ");
        while container_content.len() < 4092 {
            container_content.push(b'a');
        }
        container_content.extend_from_slice(b" -->");
        assert_eq!(container_content.len(), 4096);

        let zip_bytes = stored_zip(&[("META-INF/container.xml", container_content.as_slice())]);

        let mut header_scratch = [0u8; 46];
        let mut name_scratch = [0u8; 256];
        let mut compressed_scratch = [0u8; 1024];
        let mut zip_inflate = inflate_scratch();
        let mut opf_path_buf = heapless::String::<256>::new();

        // 1. With 3840 bytes (the regression limit), it must fail with OutputTooSmall
        let mut zip_small = ZipLocalStream::new(SliceStream {
            bytes: &zip_bytes,
            cursor: 0,
        });
        let mut small_container_scratch = [0u8; 3840];
        let res_small = load_container_xml_and_find_opf_path(
            &mut zip_small,
            &mut header_scratch,
            &mut name_scratch,
            &mut compressed_scratch,
            &mut small_container_scratch,
            &mut zip_inflate,
            &mut opf_path_buf,
        );
        assert_eq!(res_small, Err(EpubError::Zip(ZipError::OutputTooSmall)));

        // 2. With 4096 bytes (the fixed limit), it must succeed
        let mut zip_full = ZipLocalStream::new(SliceStream {
            bytes: &zip_bytes,
            cursor: 0,
        });
        let mut full_container_scratch = [0u8; 4096];
        let mut zip_inflate = inflate_scratch(); // reset inflation
        let res_full = load_container_xml_and_find_opf_path(
            &mut zip_full,
            &mut header_scratch,
            &mut name_scratch,
            &mut compressed_scratch,
            &mut full_container_scratch,
            &mut zip_inflate,
            &mut opf_path_buf,
        );
        assert!(res_full.is_ok());
        assert_eq!(opf_path_buf.as_str(), "OEBPS/content.opf");
    }

    /// A sink that records what the coordinate space looked like: every
    /// block's offset and the text that claimed it.
    #[derive(Default)]
    struct OffsetSink {
        blocks: std::vec::Vec<(u32, std::string::String)>,
    }

    impl XhtmlBlockSink for OffsetSink {
        fn push_block(
            &mut self,
            text: &str,
            _role: TextRole,
            _style: FontStyle,
            _align: TextAlign,
            _paragraph_end: bool,
            logical_offset: u32,
        ) -> Result<(), XhtmlError> {
            self.blocks.push((logical_offset, text.into()));
            Ok(())
        }
    }

    const ANCHOR_DOC: &str = concat!(
        "<html><body><p>The first paragraph, which runs on for a while so ",
        "that the wrap point matters.</p><p>Second.</p>",
        "<img src=\"plate.png\" alt=\"A plate\"/><p>Third and last.</p></body></html>"
    );

    #[test]
    fn every_block_starts_where_the_one_before_it_ended() {
        let mut sink = OffsetSink::default();
        xhtml_blocks_to_sink(ANCHOR_DOC, None, &mut sink).expect("parses");
        assert!(sink.blocks.len() >= 4, "{:?}", sink.blocks);
        assert_eq!(sink.blocks[0].0, 0, "the first block opens the stream");
        for pair in sink.blocks.windows(2) {
            let (offset, text) = &pair[0];
            let (next_offset, _) = &pair[1];
            assert_eq!(
                *next_offset,
                offset + crate::anchor::block_stream_len(text.len()),
                "offsets run contiguously: {:?}",
                sink.blocks
            );
        }
    }

    #[test]
    fn an_image_holds_a_place_of_its_own() {
        let mut sink = OffsetSink::default();
        xhtml_blocks_to_sink(ANCHOR_DOC, None, &mut sink).expect("parses");
        let plate = sink
            .blocks
            .iter()
            .position(|(_, text)| text.contains("A plate"))
            .expect("the image emits a block");
        assert!(plate > 0, "with a paragraph before it");
        let before = sink.blocks[plate - 1].0;
        let at = sink.blocks[plate].0;
        let after = sink.blocks[plate + 1].0;
        assert!(before < at && at < after, "and a place between them");
    }

    #[test]
    fn a_streamed_parse_lands_on_the_same_offsets() {
        // The one way this coordinate space could drift without anyone
        // noticing: firmware feeds the parser in bounded windows, and a
        // window boundary must not move a block's place in the stream.
        let mut whole = OffsetSink::default();
        xhtml_blocks_to_sink(ANCHOR_DOC, None, &mut whole).expect("parses");

        for window in [1usize, 3, 7, 64, 4096] {
            let mut streamed = OffsetSink::default();
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            let bytes = ANCHOR_DOC.as_bytes();
            let mut at = 0usize;
            while at < bytes.len() {
                let end = (at + window).min(bytes.len());
                tokenizer
                    .feed_xhtml_blocks(&bytes[at..end], &mut parser, None, &mut streamed)
                    .expect("feeds");
                at = end;
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .expect("finishes");
            assert_eq!(
                streamed.blocks, whole.blocks,
                "a {window}-byte window changed the stream"
            );
        }
    }

    #[test]
    fn the_cursor_reports_where_the_next_block_goes() {
        let mut sink = OffsetSink::default();
        let mut parser = XhtmlBlockStreamParser::new(true);
        assert_eq!(parser.stream_offset(), 0, "a fresh spine item starts at 0");
        parser.handle_text("Some words.", &mut sink).expect("text");
        parser.finish(&mut sink).expect("finish");
        let (offset, text) = sink.blocks.last().expect("a block").clone();
        assert_eq!(
            parser.stream_offset(),
            offset + crate::anchor::block_stream_len(text.len()),
            "and ends past the block it emitted"
        );
    }

    #[test]
    fn cdata_sections_emit_verbatim_text_in_buffered_and_streaming_parsers() {
        let xhtml = "<body><p>Before <![CDATA[Write &lt;tag&gt; &amp; \"quotes\" ] and ]]]]> after</p></body>";
        let mut buffered: Vec<TextBlock<128>, 8> = Vec::new();
        xhtml_text_blocks_with_css(xhtml, None, &mut buffered).expect("buffered parse");
        assert_eq!(buffered.len(), 1);
        assert_eq!(
            buffered[0].text.as_str(),
            "Before Write &lt;tag&gt; &amp; \"quotes\" ] and ]] after"
        );

        struct CollectSink {
            out: std::string::String,
        }
        impl XhtmlBlockSink for CollectSink {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                _paragraph_end: bool,
                _logical_offset: u32,
            ) -> Result<(), XhtmlError> {
                self.out.push_str(text);
                Ok(())
            }
        }
        let mut tok = StreamingXmlTokenizer::new();
        let mut parser = XhtmlBlockStreamParser::new(false);
        let mut sink = CollectSink {
            out: std::string::String::new(),
        };
        for byte in xhtml.as_bytes() {
            tok.feed_xhtml_blocks(core::slice::from_ref(byte), &mut parser, None, &mut sink)
                .expect("stream byte");
        }
        tok.finish_xhtml_blocks(&mut parser, &mut sink)
            .expect("finish stream");
        assert_eq!(
            sink.out,
            "Before Write &lt;tag&gt; &amp; \"quotes\" ] and ]] after"
        );
    }

    #[test]
    fn buffered_and_streaming_extraction_parity_with_encoded_entities() {
        let xhtml = "<body><p>&amp;lt;tag&amp;gt; &amp;amp; &quot;quotes&quot; &sbquo;low&lsquo; &bdquo;double&ldquo; &lsaquo;angle&rsaquo; &alpha;&Omega; &rarr; &ne;</p></body>";
        let mut buffered: Vec<TextBlock<128>, 8> = Vec::new();
        xhtml_text_blocks_with_css(xhtml, None, &mut buffered).expect("buffered parse");
        let buffered_text: std::string::String = buffered
            .iter()
            .map(|b| b.text.as_str())
            .collect::<std::vec::Vec<_>>()
            .join(" ");

        struct CollectSink {
            out: std::string::String,
        }
        impl XhtmlBlockSink for CollectSink {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                _paragraph_end: bool,
                _logical_offset: u32,
            ) -> Result<(), XhtmlError> {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if !self.out.is_empty() {
                        self.out.push(' ');
                    }
                    self.out.push_str(trimmed);
                }
                Ok(())
            }
        }

        let mut tok = StreamingXmlTokenizer::new();
        let mut parser = XhtmlBlockStreamParser::new(false);
        let mut streaming_sink = CollectSink {
            out: std::string::String::new(),
        };
        for byte in xhtml.as_bytes() {
            tok.feed_xhtml_blocks(
                core::slice::from_ref(byte),
                &mut parser,
                None,
                &mut streaming_sink,
            )
            .expect("stream feed");
        }
        tok.finish_xhtml_blocks(&mut parser, &mut streaming_sink)
            .expect("stream finish");

        assert_eq!(
            buffered_text,
            "&lt;tag&gt; &amp; \"quotes\" ‚low‘ „double“ ‹angle› αΩ → ≠"
        );
        assert_eq!(buffered_text, streaming_sink.out);
    }

    #[test]
    fn html4_named_entities_decode_to_expected_unicode_scalars() {
        let cases: &[(&str, char)] = &[
            // Quotation / Punctuation distinctions
            ("&sbquo;", '‚'),      // U+201A
            ("&bdquo;", '„'),      // U+201E
            ("&lsquo;", '‘'),      // U+2018
            ("&rsquo;", '’'),      // U+2019
            ("&ldquo;", '“'),      // U+201C
            ("&rdquo;", '”'),      // U+201D
            ("&lsaquo;", '‹'),     // U+2039
            ("&rsaquo;", '›'),     // U+203A
            ("&mdash;", '—'),      // U+2014
            ("&ndash;", '–'),      // U+2013
            ("&hellip;", '…'),     // U+2026
            ("&prime;", '′'),      // U+2032
            ("&Prime;", '″'),      // U+2033
            ("&oline;", '‾'),      // U+203E
            ("&frasl;", '⁄'),      // U+2044
            ("&euro;", '€'),       // U+20AC
            ("&trade;", '™'),      // U+2122
            ("&lrm;", '\u{200E}'), // U+200E
            ("&rlm;", '\u{200F}'), // U+200F
            // Greek
            ("&Alpha;", 'Α'),    // U+0391
            ("&Omega;", 'Ω'),    // U+03A9
            ("&alpha;", 'α'),    // U+03B1
            ("&omega;", 'ω'),    // U+03C9
            ("&thetasym;", 'ϑ'), // U+03D1
            ("&upsih;", 'ϒ'),    // U+03D2
            ("&piv;", 'ϖ'),      // U+03D6
            // Arrows
            ("&larr;", '←'),  // U+2190
            ("&rarr;", '→'),  // U+2192
            ("&uarr;", '↑'),  // U+2191
            ("&darr;", '↓'),  // U+2193
            ("&harr;", '↔'),  // U+2194
            ("&crarr;", '↵'), // U+21B5
            ("&lArr;", '⇐'),  // U+21D0
            ("&rArr;", '⇒'),  // U+21D2
            // Math
            ("&forall;", '∀'),      // U+2200
            ("&part;", '∂'),        // U+2202
            ("&exist;", '∃'),       // U+2203
            ("&empty;", '∅'),       // U+2205
            ("&nabla;", '∇'),       // U+2207
            ("&isin;", '∈'),        // U+2208
            ("&notin;", '∉'),       // U+2209
            ("&ni;", '∋'),          // U+220B
            ("&prod;", '∏'),        // U+220F
            ("&sum;", '∑'),         // U+2211
            ("&minus;", '−'),       // U+2212
            ("&lowast;", '∗'),      // U+2217
            ("&radic;", '√'),       // U+221A
            ("&prop;", '∝'),        // U+221D
            ("&infin;", '∞'),       // U+221E
            ("&ang;", '∠'),         // U+2220
            ("&and;", '∧'),         // U+2227
            ("&or;", '∨'),          // U+2228
            ("&cap;", '∩'),         // U+2229
            ("&cup;", '∪'),         // U+222A
            ("&int;", '∫'),         // U+222B
            ("&there4;", '∴'),      // U+2234
            ("&sim;", '∼'),         // U+223C
            ("&cong;", '≅'),        // U+2245
            ("&asymp;", '≈'),       // U+2248
            ("&ne;", '≠'),          // U+2260
            ("&equiv;", '≡'),       // U+2261
            ("&le;", '≤'),          // U+2264
            ("&ge;", '≥'),          // U+2265
            ("&sub;", '⊂'),         // U+2282
            ("&sup;", '⊃'),         // U+2283
            ("&nsub;", '⊄'),        // U+2284
            ("&sube;", '⊆'),        // U+2286
            ("&supe;", '⊇'),        // U+2287
            ("&oplus;", '⊕'),       // U+2295
            ("&otimes;", '⊗'),      // U+2297
            ("&perp;", '⊥'),        // U+22A5
            ("&sdot;", '⋅'),        // U+22C5
            ("&lceil;", '⌈'),       // U+2308
            ("&rceil;", '⌉'),       // U+2309
            ("&lfloor;", '⌊'),      // U+230A
            ("&rfloor;", '⌋'),      // U+230B
            ("&lang;", '\u{2329}'), // U+2329
            ("&rang;", '\u{232A}'), // U+232A
            // Suits & shapes
            ("&loz;", '◊'),    // U+25CA
            ("&spades;", '♠'), // U+2660
            ("&clubs;", '♣'),  // U+2663
            ("&hearts;", '♥'), // U+2665
            ("&diams;", '♦'),  // U+2666
        ];
        for &(entity, expected) in cases {
            assert_eq!(
                decode_html_entity(entity),
                Some(expected),
                "Failed to decode entity {entity}"
            );
        }
    }

    #[test]
    fn nbsp_and_latin_entities_decode_and_preserve_non_breaking_space() {
        let xhtml =
            "<body><p>&nbsp;— Café &eacute;t&eacute; &laquo;non&nbsp;breaking&raquo;</p></body>";
        let mut blocks: Vec<TextBlock<128>, 8> = Vec::new();
        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("parse");
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].text.as_str(),
            "\u{00A0}— Café été «non\u{00A0}breaking»"
        );
    }

    #[test]
    fn hr_and_paragraph_wrapped_list_items_render_cleanly() {
        let xhtml =
            "<body><p>Above</p><hr/><ul><li><p>First</p><p>Follow-up</p></li></ul><ol><li><p>Numbered</p></li></ol></body>";
        let mut blocks: Vec<TextBlock<128>, 8> = Vec::new();
        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("parse");
        let texts: std::vec::Vec<(&str, TextAlign)> =
            blocks.iter().map(|b| (b.text.as_str(), b.align)).collect();
        assert_eq!(
            texts,
            std::vec![
                ("Above", TextAlign::Justify),
                ("* * *", TextAlign::Center),
                ("- First", TextAlign::Justify),
                ("Follow-up", TextAlign::Justify),
                ("1. Numbered", TextAlign::Justify),
            ]
        );
    }
}

/// The inline-style reader checked in two layers. [`style_declarations`] is
/// checked against Servo's `cssparser`, which tokenizes and recovers from
/// errors as CSS Syntax specifies: each case builds a style from pieces of CSS
/// syntax, writes it into a `style` attribute the way XHTML must (entities and
/// all), and both readers list its `display`, `visibility` and `all`
/// declarations.
/// A disagreement shrinks to the fewest pieces that still disagree.
/// `cssparser` knows no property grammars, so the cascade on top, which values
/// are valid and which declaration wins, has tests of its own.
#[cfg(test)]
mod style_reference {
    use super::{
        attr_value, css_is_substitution, std, style_declarations, style_hides, StyleCascade,
        StyleDeclaration, StyleProperty, StyleValue, STYLE_VALUE_WORDS, STYLE_WORD_BYTES,
    };
    use cssparser::{
        AtRuleParser, CowRcStr, DeclarationParser, ParseError, Parser, ParserState,
        QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, Token,
    };
    use std::string::{String, ToString};
    use std::vec::Vec;

    enum ValueToken {
        Ident(String),
        Bang,
        Other,
    }

    /// A value's top-level tokens into `top`, when given, and whether any
    /// block in it holds a substitution function.
    fn scan_value(input: &mut Parser<'_>, mut top: Option<&mut Vec<ValueToken>>) -> bool {
        let mut substituted = false;
        // `next` passes over whitespace and comments.
        while let Ok(token) = input.next() {
            let token = token.clone();
            let nested = match &token {
                Token::Function(name) => {
                    substituted |= css_is_substitution(name);
                    true
                }
                Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock => {
                    true
                }
                _ => false,
            };
            if let Some(top) = top.as_deref_mut() {
                top.push(match &token {
                    Token::Ident(word) => ValueToken::Ident(word.to_string()),
                    Token::Delim('!') => ValueToken::Bang,
                    _ => ValueToken::Other,
                });
            }
            if nested {
                let _ = input.parse_nested_block(|block| {
                    substituted |= scan_value(block, None);
                    Ok::<(), ParseError<()>>(())
                });
            }
        }
        substituted
    }

    /// The `display`, `visibility` and `all` declarations `cssparser` finds.
    fn reference_declarations(css: &str) -> Vec<StyleDeclaration> {
        struct Declarations;
        impl<'i> DeclarationParser<'i> for Declarations {
            type Declaration = Option<StyleDeclaration>;
            type Error = ();

            fn parse_value(
                &mut self,
                name: CowRcStr<'i>,
                input: &mut Parser<'i>,
                _declaration_start: &ParserState,
            ) -> Result<Option<StyleDeclaration>, ParseError<()>> {
                let property = if name.eq_ignore_ascii_case("display") {
                    StyleProperty::Display
                } else if name.eq_ignore_ascii_case("visibility") {
                    StyleProperty::Visibility
                } else if name.eq_ignore_ascii_case("all") {
                    StyleProperty::All
                } else {
                    while input.next().is_ok() {}
                    return Ok(None);
                };
                let mut tokens = Vec::new();
                let substituted = scan_value(input, Some(&mut tokens));
                // CSS Syntax, consume a declaration: a value ending in `!`
                // and `important` loses them and is important.
                let mut important = false;
                if let [.., ValueToken::Bang, ValueToken::Ident(word)] = tokens.as_slice() {
                    if word.eq_ignore_ascii_case("important") {
                        important = true;
                        tokens.truncate(tokens.len() - 2);
                    }
                }
                let keywords = (1..=STYLE_VALUE_WORDS).contains(&tokens.len())
                    && tokens.iter().all(|token| {
                        matches!(token, ValueToken::Ident(word) if word.len() <= STYLE_WORD_BYTES)
                    });
                let value = if substituted {
                    StyleValue::Substituted
                } else if keywords {
                    let mut words = heapless::Vec::new();
                    for token in &tokens {
                        if let ValueToken::Ident(word) = token {
                            let mut lower = heapless::String::new();
                            lower.push_str(&word.to_ascii_lowercase()).unwrap();
                            words.push(lower).unwrap();
                        }
                    }
                    StyleValue::Keywords(words)
                } else {
                    StyleValue::Other
                };
                Ok(Some(StyleDeclaration {
                    property,
                    value,
                    important,
                }))
            }
        }
        impl<'i> AtRuleParser<'i> for Declarations {
            type Prelude = ();
            type AtRule = Option<StyleDeclaration>;
            type Error = ();
        }
        impl<'i> QualifiedRuleParser<'i> for Declarations {
            type Prelude = ();
            type QualifiedRule = Option<StyleDeclaration>;
            type Error = ();
        }
        // A style attribute holds declarations only; nested rules are not
        // allowed there.
        impl<'i> RuleBodyItemParser<'i, Option<StyleDeclaration>, ()> for Declarations {
            fn parse_declarations(&self) -> bool {
                true
            }
            fn parse_qualified(&self) -> bool {
                false
            }
        }

        let mut parser = Parser::new(css);
        RuleBodyParser::new(&mut parser, &mut Declarations)
            .filter_map(|item| item.ok().flatten())
            .collect()
    }

    /// The style as XHTML carries it inside `style="..."`.
    fn attribute_tag(css: &str) -> String {
        let mut tag = String::from("p style=\"");
        for ch in css.chars() {
            match ch {
                '&' => tag.push_str("&amp;"),
                '"' => tag.push_str("&quot;"),
                '<' => tag.push_str("&lt;"),
                '\n' => tag.push_str("&#10;"),
                '\r' => tag.push_str("&#13;"),
                '\t' => tag.push_str("&#9;"),
                '\u{a0}' => tag.push_str("&nbsp;"),
                _ => tag.push(ch),
            }
        }
        tag.push('"');
        tag
    }

    fn our_declarations(css: &str) -> Vec<StyleDeclaration> {
        let tag = attribute_tag(css);
        let mut found = Vec::new();
        style_declarations(attr_value(&tag, "style").unwrap(), |declaration| {
            found.push(declaration)
        });
        found
    }

    fn reference_hides(css: &str) -> bool {
        let mut cascade = StyleCascade::default();
        for declaration in reference_declarations(css) {
            cascade.apply(&declaration);
        }
        cascade.hides()
    }

    fn disagrees(pieces: &[&str]) -> bool {
        let css: String = pieces.concat();
        our_declarations(&css) != reference_declarations(&css)
    }

    /// The inputs that set the reader apart from a word search: URLs and
    /// escapes are tokens, and `!important` is the only priority.
    #[test]
    fn style_hides_reads_css_tokens() {
        for (css, hides) in [
            ("background:url(/*);display:none", true),
            ("display:n\\one", true),
            ("dis\\play:none", true),
            ("display:\\6e one", true),
            ("display:none !foo", false),
            ("display:none\u{a0}", false),
            ("display:\u{a0}none", false),
            ("display:none!important!important", false),
            ("x:(;display:none)", false),
            ("@x{;display:none}display:none", true),
        ] {
            assert_eq!(reference_hides(css), hides, "cssparser on {css:?}");
            assert_eq!(style_hides(&attribute_tag(css)), hides, "{css:?}");
        }
    }

    /// Each property's last valid declaration wins, an `!important` one over
    /// any other; a value CSS rejects is dropped and changes nothing.
    #[test]
    fn style_cascade_follows_the_last_valid_declaration() {
        for (css, hides) in [
            ("display:none; display:block", false),
            ("display:block; display:none", true),
            ("display:none; display:foo", true),
            ("display:none; display:none none", true),
            ("display:none; display:'block'", true),
            ("display:none; display:inline flow-root list-item", false),
            ("display:none; display:list-item flow", false),
            ("display:none; display:block inline", true),
            ("display:none; display:list-item table", true),
            ("display:none; display:list-item list-item", true),
            ("display:none; display:table-header-group", false),
            ("display:none; display:INHERIT", false),
            ("display:none; display:inherit block", true),
            ("display:none; display:var(--shown)", false),
            ("display:none; display:calc(var(--x))", false),
            ("display:none; display:calc(1)", true),
            ("display:none !important; display:block", true),
            ("display:none !important; display:block !important", false),
            ("display:block !important; display:none", false),
            ("visibility:hidden; visibility:visible", false),
            ("visibility:visible; visibility:collapse", true),
            ("visibility:hidden; visibility:shown", true),
            ("visibility:hidden; display:block", true),
            ("display:none; visibility:visible", true),
            ("display:none;", true),
            ("display:", false),
            ("display:none; all:initial", false),
            ("all:initial; display:none", true),
            ("visibility:hidden; all:unset", false),
            ("display:none; all:INHERIT", false),
            ("display:none; all:revert-layer", false),
            ("display:none; all:var(--reset)", false),
            ("display:none; all:foo", true),
            ("display:none; all:initial inherit", true),
            ("display:none !important; all:initial", true),
            ("display:none; all:initial !important; display:none", false),
        ] {
            assert_eq!(style_hides(&attribute_tag(css)), hides, "{css:?}");
        }
    }

    /// Declarations a case starts from, before mutation.
    const SEEDS: &[&[&str]] = &[
        &["display", ":", "none"],
        &["visibility", ":", "hidden"],
        &["display", " ", ":", " ", "none", " ", "!", "important"],
        &["DISPLAY", ":", "NONE", "!important"],
        &["display", ":", "block"],
        &["display", ":", "inline", " ", "flow-root"],
        &["display", ":", "var(", "--x", ")"],
        &["visibility", ":", "visible"],
        &["visibility", ":", "collapse", "!important"],
        &["all", ":", "initial"],
        &["color", ":", "red"],
        &["--x", ":", "'", "/*", "'"],
        &["width", ":", "calc(", "1", "%", ")"],
        &["background", ":", "url(", "a", ")"],
        &["@x", " ", "{", "}"],
    ];

    /// What a mutation inserts or swaps in: pieces of CSS syntax.
    const PIECES: &[&str] = &[
        "display",
        "visibility",
        "none",
        "hidden",
        "block",
        "visible",
        "inherit",
        "all",
        "unset",
        "list-item",
        "flow",
        "--x",
        "a",
        "e",
        "red",
        "url(",
        "url( ",
        "calc(",
        "var(",
        "(",
        ")",
        "[",
        "]",
        "{",
        "}",
        ":",
        ";",
        " ",
        "\n",
        "\r",
        "\t",
        "/*",
        "*/",
        "*",
        "/",
        "'",
        "\"",
        "\\",
        "\\6e ",
        "\\6E",
        "\\n",
        "\\\n",
        "!",
        "important",
        "!important",
        "@x",
        "&",
        "<",
        ",",
        "#",
        "-",
        "+",
        ".",
        "0",
        "1",
        "%",
        "\u{e9}",
        "\u{a0}",
        "\0",
    ];

    #[test]
    fn style_declarations_agree_with_cssparser() {
        // xorshift64, seeded, so a failure reproduces.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        const CASES: usize = 100_000;
        let (mut hidden, mut overridden) = (0, 0);
        for _ in 0..CASES {
            let mut pieces: Vec<&str> = Vec::new();
            for declaration in 0..1 + next(3) {
                if declaration > 0 {
                    pieces.push(";");
                }
                pieces.extend_from_slice(SEEDS[next(SEEDS.len())]);
            }
            for _ in 0..next(4) {
                let at = next(pieces.len() + 1);
                let piece = PIECES[next(PIECES.len())];
                match next(3) {
                    0 => pieces.insert(at, piece),
                    1 if at < pieces.len() => pieces[at] = piece,
                    _ if at < pieces.len() => {
                        pieces.remove(at);
                    }
                    _ => pieces.push(piece),
                }
            }
            let css: String = pieces.concat();
            hidden += usize::from(reference_hides(&css));
            overridden += usize::from(reference_declarations(&css).len() > 1);
            if !disagrees(&pieces) {
                continue;
            }
            // Drop pieces while the disagreement stands.
            let mut index = 0;
            while index < pieces.len() {
                let removed = pieces.remove(index);
                if !disagrees(&pieces) {
                    pieces.insert(index, removed);
                    index += 1;
                }
            }
            let css: String = pieces.concat();
            panic!(
                "style {css:?}:\n  ours      {:?}\n  cssparser {:?}",
                our_declarations(&css),
                reference_declarations(&css)
            );
        }
        // Both answers must be common, and so must styles where a later
        // declaration meets an earlier one, or agreement proves little.
        assert!(
            (CASES / 5..CASES * 4 / 5).contains(&hidden),
            "{hidden} of {CASES} cases hidden"
        );
        assert!(
            overridden > CASES / 10,
            "{overridden} of {CASES} cases have two declarations"
        );
    }
}
