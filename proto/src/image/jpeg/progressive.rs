//! Multi-scan progressive JPEG decoder (`SOF2`) reconstructing intra-block AC
//! detail without buffering full-image coefficients in RAM.
//!
//! Each luma scan (`[Y]`, or an interleaved DC scan containing `Y`) is walked in
//! parallel across a rolling per-scan read window sliced from the caller's
//! compressed input buffer. Every block `(bx, by)` initializes a 64-entry
//! `i32` block in scratch, applies all collected luma scans in file order (DC
//! initial, DC refinement, AC initial bands with `EOBRUN`, and AC successive
//! approximation refinements), runs `idct::transform`, and feeds the existing
//! box-filter row pipeline.

use super::super::png::{reciprocal, Spans};
use super::idct;
use super::{
    build_table, filter_across_region, filter_down_region, quant_byte_into, BitState, Bits,
    Columns, Frame, Head, Huffman, JpegDecoder, JpegError, JpegInfo, JpegProbe, LumaSink, Mark,
    SinkError, TableState, BLOCK_BYTES, FAST_BITS, HELD, MAX_STRAY, SLOT_BYTES, ZIGZAG,
};

/// Maximum luma scans decoded in parallel for progressive AC reconstruction.
const MAX_PROG_SCANS: usize = 10;
/// Hard cap on distinct luma Huffman tables stored simultaneously in scratch.
const MAX_PROG_TABLES: usize = 10;
/// Quantization tables (4 × 64 × `u16`).
const QUANT_BYTES: usize = 4 * 64 * 2;
/// Buffer tail per block: `block` (`256`), `tile` (`<= 8 × 8` = `64`, keep `256`),
/// `carry` (`8 × 4` = `32`, keep `64`).
const TILE_AT_OFF: usize = BLOCK_BYTES;
const TILE_LEN: usize = 256;
const CARRY_AT_OFF: usize = TILE_AT_OFF + TILE_LEN;
const CARRY_LEN: usize = 64;
const SUMS_AT_OFF: usize = CARRY_AT_OFF + CARRY_LEN;
const HDR_TMP_LEN: usize = 273;

#[derive(Clone, Copy, Default)]
struct DcCheckpoint {
    valid: bool,
    file_pos: u32,
    bits: BitState,
    preds: [i32; 3],
    restart_left: u16,
    next_restart: u8,
}

#[derive(Clone, Copy, Default)]
struct ProgScan {
    ss: u8,
    se: u8,
    ah: u8,
    al: u8,
    count: u8,
    luma_idx: u8,
    luma_x: u8,
    luma_y: u8,
    comp_blocks: [u8; 3],
    dc_tables: [u8; 3],
    ac_table: u8,
    end_off: u32,
    file_pos: u32,
    buf_at: u16,
    buf_len: u16,
    bits: BitState,
    preds: [i32; 3],
    eobrun: u16,
    restart_interval: u16,
    restart_left: u16,
    next_restart: u8,
}

pub type ReadAt<'a> = dyn FnMut(u32, &mut [u8]) -> Result<(), JpegError> + 'a;

struct SeqReader<'a, 'b> {
    buf: &'a mut [u8],
    pos: u32,
    total_len: u32,
    off: usize,
    len: usize,
    read_at: &'b mut ReadAt<'b>,
}

impl<'a, 'b> SeqReader<'a, 'b> {
    fn new(buf: &'a mut [u8], total_len: u32, read_at: &'b mut ReadAt<'b>) -> Self {
        Self {
            buf,
            pos: 0,
            total_len,
            off: 0,
            len: 0,
            read_at,
        }
    }

    fn file_offset(&self) -> u32 {
        self.pos - (self.len - self.off) as u32
    }

    fn refill(&mut self) -> Result<bool, JpegError> {
        self.off = 0;
        self.len = 0;
        let rem = self.total_len.saturating_sub(self.pos);
        if rem == 0 {
            return Ok(false);
        }
        let want = (rem as usize).min(self.buf.len());
        (self.read_at)(self.pos, &mut self.buf[..want])?;
        self.pos += want as u32;
        self.len = want;
        Ok(true)
    }

    fn read_u8(&mut self) -> Result<u8, JpegError> {
        if self.off >= self.len && !self.refill()? {
            return Err(JpegError::Truncated);
        }
        let byte = self.buf[self.off];
        self.off += 1;
        Ok(byte)
    }

    fn read_be16(&mut self) -> Result<u16, JpegError> {
        let hi = self.read_u8()?;
        let lo = self.read_u8()?;
        Ok(u16::from_be_bytes([hi, lo]))
    }

    fn read_exact(&mut self, out: &mut [u8]) -> Result<(), JpegError> {
        let mut done = 0;
        while done < out.len() {
            if self.off >= self.len && !self.refill()? {
                return Err(JpegError::Truncated);
            }
            let take = (self.len - self.off).min(out.len() - done);
            out[done..done + take].copy_from_slice(&self.buf[self.off..self.off + take]);
            self.off += take;
            done += take;
        }
        Ok(())
    }

    fn skip(&mut self, n: u32) -> Result<(), JpegError> {
        let cur = self.file_offset();
        let target = cur.checked_add(n).ok_or(JpegError::Corrupt)?;
        if target > self.total_len {
            return Err(JpegError::Truncated);
        }
        let avail = (self.len - self.off) as u32;
        if n <= avail {
            self.off += n as usize;
        } else {
            self.pos = target;
            self.off = 0;
            self.len = 0;
        }
        Ok(())
    }

    /// Scans entropy-coded data forward until a non-fill, non-stuffed,
    /// non-RST marker (`0xFF <code>`) is seen, returning `(end_off, Some(code))`
    /// where `end_off` includes the two marker bytes so [`Bits`] reaches the
    /// marker cleanly.
    fn scan_to_next_marker(
        &mut self,
        gives_way: &dyn Fn() -> bool,
    ) -> Result<(u32, u8), JpegError> {
        let mut ff = false;
        loop {
            if gives_way() {
                return Err(JpegError::Sink(SinkError::Cancelled));
            }
            if self.off >= self.len && !self.refill()? {
                return Err(JpegError::Truncated);
            }
            let slice = &self.buf[self.off..self.len];
            for (idx, &byte) in slice.iter().enumerate() {
                if ff {
                    if byte == 0xFF {
                        continue;
                    }
                    ff = false;
                    if byte != 0 && byte & 0xF8 != 0xD0 {
                        self.off += idx + 1;
                        return Ok((self.file_offset(), byte));
                    }
                } else if byte == 0xFF {
                    ff = true;
                }
            }
            self.off = self.len;
        }
    }
}

/// Decodes a JPEG in high-quality mode from random-access bytes `[0..total_len)`:
/// baseline/extended sequential streams sequentially, while multi-scan
/// progressive (`SOF2`) reconstructs DC + AC scans in raster block order without
/// holding full-image coefficient memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JpegDecodeQuality {
    /// Baseline/sequential pixels or complete progressive AC reconstruction.
    Full,
    /// Progressive DC-only fallback after exceeding scratch, scan, or table limits.
    DcOnly,
}

pub fn decode_from_reader(
    scratch: &mut [u8],
    input: &mut [u8],
    total_len: u32,
    out: (u32, u32),
    read_at: &mut ReadAt<'_>,
    gives_way: &dyn Fn() -> bool,
    sink: &mut dyn LumaSink,
) -> Result<JpegDecodeQuality, JpegError> {
    let (out_w, out_h) = out;
    if out_w == 0 || out_h == 0 {
        return Err(JpegError::BadOutputSize);
    }
    if input.len() < 512 {
        return Err(JpegError::ScratchTooSmall);
    }
    let info = probe_header(input, total_len, read_at, gives_way)?;
    info.check()?;
    if out_w > info.width || out_h > info.height {
        return Err(JpegError::BadOutputSize);
    }
    if info.coding != 0xC2 {
        if scratch.len() < info.scratch_bytes(out_w, out_h) {
            return Err(JpegError::ScratchTooSmall);
        }
        decode_sequential(
            scratch,
            input,
            total_len,
            (out_w, out_h),
            read_at,
            gives_way,
            sink,
        )?;
        return Ok(JpegDecodeQuality::Full);
    }
    match decode_progressive(
        scratch,
        input,
        total_len,
        (out_w, out_h),
        read_at,
        gives_way,
        sink,
    )? {
        ProgOutcome::Done => Ok(JpegDecodeQuality::Full),
        ProgOutcome::FallbackDc => {
            if scratch.len() < info.scratch_bytes(out_w, out_h) {
                return Err(JpegError::ScratchTooSmall);
            }
            decode_sequential(
                scratch,
                input,
                total_len,
                (out_w, out_h),
                read_at,
                gives_way,
                sink,
            )?;
            Ok(JpegDecodeQuality::DcOnly)
        }
    }
}

enum ProgOutcome {
    Done,
    FallbackDc,
}

fn probe_header(
    input: &mut [u8],
    total_len: u32,
    read_at: &mut ReadAt<'_>,
    gives_way: &dyn Fn() -> bool,
) -> Result<JpegInfo, JpegError> {
    let mut probe = JpegProbe::new();
    let mut pos = 0u32;
    let chunk_cap = input.len().min(512);
    while pos < total_len {
        if gives_way() {
            return Err(JpegError::Sink(SinkError::Cancelled));
        }
        let want = (total_len - pos).min(chunk_cap as u32) as usize;
        read_at(pos, &mut input[..want])?;
        pos += want as u32;
        if let Some(res) = probe.feed(&input[..want]) {
            return res;
        }
        let skip = probe.skippable().min(total_len.saturating_sub(pos));
        if skip > 0 {
            probe.skip(skip);
            pos += skip;
        }
    }
    Err(JpegError::Truncated)
}

fn decode_sequential(
    scratch: &mut [u8],
    input: &mut [u8],
    total_len: u32,
    out: (u32, u32),
    read_at: &mut ReadAt<'_>,
    gives_way: &dyn Fn() -> bool,
    sink: &mut dyn LumaSink,
) -> Result<(), JpegError> {
    let mut decoder = JpegDecoder::new(scratch, out.0, out.1)?;
    let mut pos = 0u32;
    while pos < total_len && !decoder.is_complete() {
        if gives_way() {
            return Err(JpegError::Sink(SinkError::Cancelled));
        }
        let want = (total_len - pos).min(input.len() as u32) as usize;
        read_at(pos, &mut input[..want])?;
        decoder.feed(&input[..want], sink)?;
        pos += want as u32;
    }
    decoder.finish()
}

struct HeaderWalk {
    frame: Frame,
    info: JpegInfo,
    quant_defined: u8,
    scans: [ProgScan; MAX_PROG_SCANS],
    scan_count: usize,
    table_defs: [(u32, u16, bool); MAX_PROG_TABLES],
    table_count: usize,
    too_many: bool,
}

fn parse_progressive_headers(
    scratch: &mut [u8],
    base: usize,
    input: &mut [u8],
    total_len: u32,
    out: (u32, u32),
    read_at: &mut ReadAt<'_>,
    gives_way: &dyn Fn() -> bool,
) -> Result<HeaderWalk, JpegError> {
    let input_len = input.len();
    let mut reader = SeqReader::new(input, total_len, read_at);
    if reader.read_u8()? != 0xFF || reader.read_u8()? != super::SOI {
        return Err(JpegError::NotJpeg);
    }

    let mut stray: u16 = 0;
    let mut restart: u16 = 0;
    let mut jfif = false;
    let mut adobe = 0xFFu8;
    let mut frame: Option<Frame> = None;
    let mut quant_defined = 0u8;
    let mut luma_quant = [0u8; 128];
    let mut luma_quant_saved = false;
    // Last transmitted point-transform (`al`) per luma zigzag coefficient (`0xFF` = none yet).
    let mut luma_approx = [0xFFu8; 64];
    // Last seen DHT `(file_offset, byte_len)` for class 0..=1, id 0..=3.
    let mut last_dht: [Option<(u32, u16)>; 8] = [None; 8];
    let mut table_defs = [(0u32, 0u16, false); MAX_PROG_TABLES];
    let mut table_count = 0usize;
    let mut scans = [ProgScan::default(); MAX_PROG_SCANS];
    let mut scan_count = 0usize;
    let mut too_many = false;
    let mut pending_code: Option<u8> = None;

    loop {
        if gives_way() {
            return Err(JpegError::Sink(SinkError::Cancelled));
        }
        let code = match pending_code.take() {
            Some(c) => c,
            None => loop {
                let b = reader.read_u8()?;
                if b == 0xFF {
                    let mut c = reader.read_u8()?;
                    while c == 0xFF {
                        c = reader.read_u8()?;
                    }
                    break c;
                }
                stray = stray.saturating_add(1);
                if stray > MAX_STRAY {
                    return Err(JpegError::Corrupt);
                }
            },
        };

        if code == super::EOI {
            break;
        }
        if code == super::SOI {
            return Err(JpegError::Corrupt);
        }
        if code == 0x00 || code == 0x01 || (code & 0xF8 == 0xD0) {
            stray = stray.saturating_add(2);
            if stray > MAX_STRAY {
                return Err(JpegError::Corrupt);
            }
            continue;
        }

        let seg_len = reader.read_be16()?;
        if seg_len < 2 {
            return Err(JpegError::Corrupt);
        }
        let body_len = u32::from(seg_len - 2);
        let body_start = reader.file_offset();
        let body_end = body_start
            .checked_add(body_len)
            .filter(|&end| end <= total_len)
            .ok_or(JpegError::Truncated)?;

        let sof = code & 0xF0 == 0xC0 && code != super::DHT && code != 0xC8 && code != 0xCC;
        if sof {
            if frame.is_some() {
                return Err(JpegError::Corrupt);
            }
            let take = body_len.min(HELD as u32) as usize;
            let mut held = [0u8; HELD];
            reader.read_exact(&mut held[..take])?;
            if body_len > HELD as u32 {
                reader.skip(body_len - HELD as u32)?;
            }
            let head = Head {
                mark: Mark::Held,
                code,
                len: seg_len - 2,
                left: 0,
                held,
                held_len: take as u8,
                restart,
                jfif,
                adobe,
                frame: None,
                probing: false,
                stray,
            };
            frame = Some(head.read_frame()?);
        } else if code == super::DQT {
            let mut tbl = TableState::default();
            let q_slice = scratch
                .get_mut(base..base + QUANT_BYTES)
                .ok_or(JpegError::ScratchTooSmall)?;
            while reader.file_offset() < body_end {
                let b = reader.read_u8()?;
                quant_byte_into(q_slice, &mut quant_defined, &mut tbl, b)?;
            }
            if tbl.at != 0 {
                return Err(JpegError::Corrupt);
            }
        } else if code == super::DHT {
            while reader.file_offset() < body_end {
                let tbl_off = reader.file_offset();
                let header = reader.read_u8()?;
                let (class, id) = (header >> 4, header & 15);
                if class > 1 || id > 3 {
                    return Err(JpegError::Corrupt);
                }
                let mut counts = [0u8; 16];
                reader.read_exact(&mut counts)?;
                let vals_len: u16 = counts.iter().map(|&c| u16::from(c)).sum();
                if vals_len > 256 || reader.file_offset() + u32::from(vals_len) > body_end {
                    return Err(JpegError::Corrupt);
                }
                reader.skip(u32::from(vals_len))?;
                last_dht[usize::from(class * 4 + id)] = Some((tbl_off, 17 + vals_len));
            }
        } else if code == super::DRI {
            if body_len != 2 {
                return Err(JpegError::Corrupt);
            }
            restart = reader.read_be16()?;
        } else if code == super::APP0 {
            let take = body_len.min(5) as usize;
            let mut app = [0u8; 5];
            reader.read_exact(&mut app[..take])?;
            if body_len >= 5 && app == *b"JFIF\0" {
                jfif = true;
            }
            reader.skip(body_len - take as u32)?;
        } else if code == super::APP14 {
            let take = body_len.min(12) as usize;
            let mut app = [0u8; 12];
            reader.read_exact(&mut app[..take])?;
            if body_len >= 12 && app[..5] == *b"Adobe" {
                adobe = app[11];
            }
            reader.skip(body_len - take as u32)?;
        } else if code == super::DHP || code == super::EXP {
            return Err(JpegError::Unsupported);
        } else if code == super::SOS {
            let Some(frame) = frame else {
                return Err(JpegError::Corrupt);
            };
            let info = resolved_info(frame, jfif, adobe);
            info.check()?;
            if body_len > HELD as u32 {
                return Err(JpegError::Corrupt);
            }
            let mut held = [0u8; HELD];
            reader.read_exact(&mut held[..body_len as usize])?;
            let count = usize::from(held[0]);
            let components = usize::from(info.components);
            if count == 0 || count > components || body_len as usize != 4 + 2 * count {
                return Err(JpegError::Corrupt);
            }
            let ss = held[1 + 2 * count];
            let se = held[2 + 2 * count];
            let approx = held[3 + 2 * count];
            let (ah, al) = (approx >> 4, approx & 15);
            if ss > se
                || se > 63
                || (ss == 0) != (se == 0)
                || (ss != 0 && count != 1)
                || ah > 13
                || al > 13
                || (ah != 0 && ah != al + 1)
            {
                return Err(JpegError::Corrupt);
            }

            let mut members = [0usize; 3];
            let mut dc_ids = [0u8; 3];
            let mut ac_id = 0u8;
            let mut luma_idx = u8::MAX;
            for i in 0..count {
                let id = held[1 + 2 * i];
                let tables = held[2 + 2 * i];
                let comp = frame.ids[..components]
                    .iter()
                    .position(|&known| known == id)
                    .ok_or(JpegError::Corrupt)?;
                if members[..i].contains(&comp) {
                    return Err(JpegError::Corrupt);
                }
                members[i] = comp;
                dc_ids[i] = tables >> 4;
                if comp == 0 {
                    luma_idx = i as u8;
                    ac_id = tables & 15;
                }
            }

            let scan_start = reader.file_offset();

            if luma_idx != u8::MAX {
                if quant_defined & (1 << frame.luma_table) == 0 {
                    return Err(JpegError::Corrupt);
                }
                if ss >= 1 && luma_approx[0] == 0xFF {
                    return Err(JpegError::Corrupt);
                }
                for approx in &mut luma_approx[usize::from(ss)..=usize::from(se)] {
                    if ah == 0 {
                        if *approx != 0xFF {
                            return Err(JpegError::Corrupt);
                        }
                    } else if *approx != ah {
                        return Err(JpegError::Corrupt);
                    }
                    *approx = al;
                }
                if !luma_quant_saved {
                    let q_off = base + 128 * usize::from(frame.luma_table % 4);
                    luma_quant.copy_from_slice(&scratch[q_off..q_off + 128]);
                    luma_quant_saved = true;
                }
                if scan_count >= MAX_PROG_SCANS || input_len / (scan_count + 1) < 64 {
                    too_many = true;
                    break;
                }
                let mut scan = ProgScan {
                    ss,
                    se,
                    ah,
                    al,
                    count: count as u8,
                    luma_idx,
                    luma_x: frame.factors[0] >> 4,
                    luma_y: frame.factors[0] & 15,
                    comp_blocks: [0; 3],
                    dc_tables: [0; 3],
                    ac_table: 0,
                    end_off: 0,
                    file_pos: scan_start,
                    buf_at: 0,
                    buf_len: 0,
                    bits: BitState::default(),
                    preds: [0; 3],
                    eobrun: 0,
                    restart_interval: restart,
                    restart_left: restart,
                    next_restart: 0,
                };
                for i in 0..count {
                    let f = frame.factors[members[i]];
                    scan.comp_blocks[i] = (f >> 4) * (f & 15);
                }
                if ss == 0 && ah == 0 {
                    for i in 0..count {
                        let key = usize::from(dc_ids[i]);
                        if dc_ids[i] > 3 {
                            return Err(JpegError::Corrupt);
                        }
                        let Some((off, tlen)) = last_dht[key] else {
                            return Err(JpegError::Corrupt);
                        };
                        match register_table(&mut table_defs, &mut table_count, off, tlen, true) {
                            Some(slot) => scan.dc_tables[i] = slot,
                            None => too_many = true,
                        }
                    }
                } else if ss >= 1 {
                    if ac_id > 3 {
                        return Err(JpegError::Corrupt);
                    }
                    let key = usize::from(4 + ac_id);
                    let Some((off, tlen)) = last_dht[key] else {
                        return Err(JpegError::Corrupt);
                    };
                    match register_table(&mut table_defs, &mut table_count, off, tlen, false) {
                        Some(slot) => scan.ac_table = slot,
                        None => too_many = true,
                    }
                }
                let side = info.block_side(out.0, out.1);
                let tables_bytes = QUANT_BYTES + table_count * SLOT_BYTES;
                let row_bytes = (4 + side) as usize * out.0 as usize;
                let total_needed = base + tables_bytes + SUMS_AT_OFF + row_bytes;
                if too_many || total_needed > scratch.len() {
                    too_many = true;
                    break;
                }
                let (scan_end, next_code) = reader.scan_to_next_marker(gives_way)?;
                pending_code = Some(next_code);
                scan.end_off = scan_end;
                scans[scan_count] = scan;
                scan_count += 1;
            } else {
                let (_, next_code) = reader.scan_to_next_marker(gives_way)?;
                pending_code = Some(next_code);
            }
        } else {
            reader.skip(body_len)?;
        }
    }

    let Some(frame) = frame else {
        return Err(JpegError::Truncated);
    };
    if luma_quant_saved {
        let q_off = base + 128 * usize::from(frame.luma_table % 4);
        scratch[q_off..q_off + 128].copy_from_slice(&luma_quant);
    }
    let info = resolved_info(frame, jfif, adobe);
    info.check()?;
    if !too_many && scan_count == 0 {
        return Err(JpegError::Corrupt);
    }
    Ok(HeaderWalk {
        frame,
        info,
        quant_defined,
        scans,
        scan_count,
        table_defs,
        table_count,
        too_many,
    })
}

fn resolved_info(frame: Frame, jfif: bool, adobe: u8) -> JpegInfo {
    let mut info = frame.info;
    info.rgb =
        info.components == 3 && (adobe == 0 || (adobe == 0xFF && !jfif && frame.ids == *b"RGB"));
    info
}

fn register_table(
    defs: &mut [(u32, u16, bool); MAX_PROG_TABLES],
    count: &mut usize,
    offset: u32,
    len: u16,
    dc: bool,
) -> Option<u8> {
    for (idx, &(o, l, d)) in defs[..*count].iter().enumerate() {
        if o == offset && l == len && d == dc {
            return Some(idx as u8);
        }
    }
    if *count >= MAX_PROG_TABLES {
        return None;
    }
    let idx = *count;
    defs[idx] = (offset, len, dc);
    *count += 1;
    Some(idx as u8)
}

fn decode_progressive(
    scratch: &mut [u8],
    input: &mut [u8],
    total_len: u32,
    out: (u32, u32),
    read_at: &mut ReadAt<'_>,
    gives_way: &dyn Fn() -> bool,
    sink: &mut dyn LumaSink,
) -> Result<ProgOutcome, JpegError> {
    let (out_w, out_h) = out;
    let base = bytemuck::pod_align_to::<u8, u32>(scratch).0.len();
    if scratch.len() < base + QUANT_BYTES {
        return Err(JpegError::ScratchTooSmall);
    }
    let mut walk = parse_progressive_headers(
        scratch,
        base,
        input,
        total_len,
        (out_w, out_h),
        read_at,
        gives_way,
    )?;
    if walk.too_many {
        return Ok(ProgOutcome::FallbackDc);
    }
    // Check total scratch needed: quant + table_count slots + block/tile/carry/sums/means.
    let side = walk.info.block_side(out_w, out_h);
    let scaled_w = super::scaled(walk.info.width, side);
    let scaled_h = super::scaled(walk.info.height, side);
    let tables_bytes = QUANT_BYTES + walk.table_count * SLOT_BYTES;
    let row_bytes = (4 + side) as usize * out_w as usize;
    let total_needed = base + tables_bytes + SUMS_AT_OFF + row_bytes;
    if total_needed > scratch.len() || input.len() / walk.scan_count < 64 {
        return Ok(ProgOutcome::FallbackDc);
    }
    let _ = walk.quant_defined;

    // Load the unique luma Huffman tables into `scratch[base + QUANT_BYTES ..]`.
    let mut hdr_buf = [0u8; HDR_TMP_LEN];
    for (i, &(off, tlen, dc)) in walk.table_defs[..walk.table_count].iter().enumerate() {
        let len = usize::from(tlen);
        if len > HDR_TMP_LEN {
            return Err(JpegError::Corrupt);
        }
        read_at(off, &mut hdr_buf[..len])?;
        let slot_at = base + QUANT_BYTES + i * SLOT_BYTES;
        let slot = scratch
            .get_mut(slot_at..slot_at + SLOT_BYTES)
            .ok_or(JpegError::Corrupt)?;
        slot.fill(0);
        slot[2 * (1 << FAST_BITS)..2 * (1 << FAST_BITS) + 16].copy_from_slice(&hdr_buf[1..17]);
        let val_len = len - 17;
        slot[2 * (1 << FAST_BITS) + 16..2 * (1 << FAST_BITS) + 16 + val_len]
            .copy_from_slice(&hdr_buf[17..len]);
        build_table(slot, dc)?;
    }

    let work_base = base + tables_bytes;
    scratch[work_base..total_needed].fill(0);

    let max_x = u32::from(walk.info.max.0);
    let max_y = u32::from(walk.info.max.1);
    let luma_x = u32::from(walk.info.luma.0);
    let luma_y = u32::from(walk.info.luma.1);
    let double_x = u8::from(max_x > luma_x);
    let double_y = u8::from(max_y > luma_y);
    let luma_w = (walk.info.width * luma_x).div_ceil(max_x);
    let luma_h = (walk.info.height * luma_y).div_ceil(max_y);
    let blocks_x = luma_w.div_ceil(8);
    let blocks_y = luma_h.div_ceil(8);

    let col_spans = Spans::new(scaled_w, out_w);
    let mut cols;
    let col_inverse = [reciprocal(col_spans.short), reciprocal(col_spans.short + 1)];
    let mut rows = Spans::new(scaled_h, out_h);
    let row_inverse = [reciprocal(rows.short), reciprocal(rows.short + 1)];
    let mut band_rows = rows.next_len();
    let mut band_left = band_rows;
    let mut next_col;
    let mut next_row = 0u32;
    let mut out_y = 0u32;

    let win_len = if input.len() / walk.scan_count >= 512 {
        512
    } else {
        input.len() / walk.scan_count
    };
    let quant_offset = base + 128 * usize::from(walk.frame.luma_table % 4);
    let mut checkpoints = [DcCheckpoint::default(); MAX_PROG_SCANS];

    for by in 0..blocks_y {
        if gives_way() {
            return Err(JpegError::Sink(SinkError::Cancelled));
        }
        // Interleaved DC scans with luma_y == 2 repeat the MCU row on odd `by`.
        for (s_idx, scan) in walk.scans[..walk.scan_count].iter_mut().enumerate() {
            if scan.count > 1 && scan.luma_y == 2 {
                if by & 1 == 0 {
                    checkpoints[s_idx] = DcCheckpoint {
                        valid: true,
                        file_pos: scan.file_pos - u32::from(scan.buf_len - scan.buf_at),
                        bits: scan.bits,
                        preds: scan.preds,
                        restart_left: scan.restart_left,
                        next_restart: scan.next_restart,
                    };
                } else if checkpoints[s_idx].valid {
                    let cp = checkpoints[s_idx];
                    scan.file_pos = cp.file_pos;
                    scan.buf_at = 0;
                    scan.buf_len = 0;
                    scan.bits = cp.bits;
                    scan.preds = cp.preds;
                    scan.restart_left = cp.restart_left;
                    scan.next_restart = cp.next_restart;
                }
            }
        }

        cols = Columns::start(col_spans);
        next_col = 0;
        if let Some(carry) = scratch.get_mut(work_base + CARRY_AT_OFF..work_base + SUMS_AT_OFF) {
            carry.fill(0);
        }

        for bx in 0..blocks_x {
            let (tables_mem, work_mem) = scratch[..total_needed].split_at_mut(work_base);
            let (block_slice, region) = work_mem
                .split_at_mut_checked(TILE_AT_OFF)
                .ok_or(JpegError::Corrupt)?;
            let block_bytes = block_slice
                .get_mut(..BLOCK_BYTES)
                .ok_or(JpegError::Corrupt)?;
            let block: &mut [i32; 64] =
                bytemuck::try_from_bytes_mut(block_bytes).map_err(|_| JpegError::Corrupt)?;
            block.fill(0);
            let mut ac_nonzero = false;

            for (s_idx, scan) in walk.scans[..walk.scan_count].iter_mut().enumerate() {
                let win = &mut input[s_idx * win_len..(s_idx + 1) * win_len];
                advance_scan_block(
                    scan,
                    bx,
                    by,
                    blocks_x,
                    tables_mem,
                    base,
                    block,
                    &mut ac_nonzero,
                    win,
                    read_at,
                )?;
            }

            // Finish trailing MCU chroma/padding if this `bx` completed an interleaved MCU row's column.
            for (s_idx, scan) in walk.scans[..walk.scan_count].iter_mut().enumerate() {
                if scan.count > 1
                    && (bx + 1 == blocks_x || (bx + 1).is_multiple_of(u32::from(scan.luma_x)))
                {
                    let win = &mut input[s_idx * win_len..(s_idx + 1) * win_len];
                    finish_interleaved_mcu(scan, bx, by, tables_mem, base, win, read_at)?;
                }
            }

            let quant: &[u16; 64] = tables_mem
                .get(quant_offset..quant_offset + 128)
                .and_then(|bytes| bytemuck::try_from_bytes(bytes).ok())
                .ok_or(JpegError::Corrupt)?;
            let (tile, rest) = region
                .split_at_mut_checked(TILE_LEN)
                .ok_or(JpegError::Corrupt)?;
            idct::transform(block, quant, ac_nonzero, side, tile, side as usize);

            let (carry, sums_and_means) = rest
                .split_at_mut_checked(CARRY_LEN)
                .ok_or(JpegError::Corrupt)?;
            let carry: &mut [u32] =
                bytemuck::try_cast_slice_mut(carry).map_err(|_| JpegError::Corrupt)?;
            let means = sums_and_means
                .get_mut(4 * out_w as usize..)
                .ok_or(JpegError::Corrupt)?;
            filter_across_region(
                tile,
                carry,
                means,
                side as usize,
                side as usize,
                double_x,
                scaled_w,
                out_w as usize,
                col_spans,
                col_inverse,
                &mut cols,
                &mut next_col,
            );
        }

        // Emit output rows covered by this `side`-tall block row.
        filter_down_region(
            &mut scratch[work_base + SUMS_AT_OFF..total_needed],
            side,
            double_y,
            scaled_h,
            out_w as usize,
            out_h,
            &mut rows,
            row_inverse,
            &mut band_rows,
            &mut band_left,
            &mut next_row,
            &mut out_y,
            sink,
        )?;
        if out_y >= out_h {
            return Ok(ProgOutcome::Done);
        }
    }

    if out_y >= out_h {
        Ok(ProgOutcome::Done)
    } else {
        Err(JpegError::Truncated)
    }
}

struct ScanCursor<'a, 'b> {
    scan: &'a mut ProgScan,
    win: &'a mut [u8],
    read_at: &'b mut ReadAt<'b>,
}

impl<'a, 'b> ScanCursor<'a, 'b> {
    fn refill_bits(&mut self) -> Result<(), JpegError> {
        let s = &mut self.scan.bits;
        while s.have <= 24 {
            let byte = if s.marker.is_some() {
                s.padded += 8;
                0
            } else {
                if self.scan.buf_at >= self.scan.buf_len {
                    let rem = self.scan.end_off.saturating_sub(self.scan.file_pos);
                    if rem == 0 {
                        return Ok(());
                    }
                    let want = (rem as usize).min(self.win.len());
                    (self.read_at)(self.scan.file_pos, &mut self.win[..want])?;
                    self.scan.file_pos += want as u32;
                    self.scan.buf_at = 0;
                    self.scan.buf_len = want as u16;
                }
                let byte = self.win[usize::from(self.scan.buf_at)];
                self.scan.buf_at += 1;
                if s.ff {
                    if byte == 0xFF {
                        continue;
                    }
                    s.ff = false;
                    if byte != 0 {
                        s.marker = Some(byte);
                        continue;
                    }
                    0xFF
                } else if byte == 0xFF {
                    s.ff = true;
                    continue;
                } else {
                    byte
                }
            };
            s.acc |= u32::from(byte) << (24 - s.have);
            s.have += 8;
        }
        Ok(())
    }

    fn ensure_bits(&mut self, n: u32) -> Result<(), JpegError> {
        if self.scan.bits.have.saturating_sub(self.scan.bits.padded) < n {
            self.refill_bits()?;
            if self.scan.bits.have.saturating_sub(self.scan.bits.padded) < n {
                return Err(JpegError::Truncated);
            }
        }
        Ok(())
    }

    fn read_bit(&mut self) -> Result<u8, JpegError> {
        self.ensure_bits(1)?;
        let bit = (self.scan.bits.acc >> 31) as u8;
        let s = &mut self.scan.bits;
        s.acc <<= 1;
        s.have -= 1;
        s.padded = s.padded.min(s.have);
        Ok(bit)
    }

    fn read_bits(&mut self, n: u32) -> Result<u32, JpegError> {
        if n == 0 {
            return Ok(0);
        }
        self.ensure_bits(n)?;
        let val = self.scan.bits.acc >> (32 - n);
        let s = &mut self.scan.bits;
        s.acc <<= n;
        s.have -= n;
        s.padded = s.padded.min(s.have);
        Ok(val)
    }

    fn receive(&mut self, size: u32) -> Result<i32, JpegError> {
        let raw = self.read_bits(size)? as i32;
        Ok(if raw < (1 << (size - 1)) {
            raw - (1 << size) + 1
        } else {
            raw
        })
    }

    fn decode_huff(&mut self, table: &Huffman<'_>) -> Result<u8, JpegError> {
        if self.scan.bits.have < 16 {
            self.refill_bits()?;
        }
        let have_before = self.scan.bits.have;
        let real_before = have_before.saturating_sub(self.scan.bits.padded);
        let mut bits = Bits {
            state: self.scan.bits,
            input: &[],
            pos: 0,
        };
        let sym = bits.decode(table)?.ok_or(JpegError::Truncated)?;
        let consumed = have_before.saturating_sub(bits.state.have);
        if consumed > real_before {
            return Err(JpegError::Truncated);
        }
        bits.state.padded = bits.state.padded.min(bits.state.have);
        self.scan.bits = bits.state;
        Ok(sym)
    }

    fn before_mcu_restart(&mut self) -> Result<(), JpegError> {
        let r = self.scan.restart_interval;
        if r == 0 || self.scan.restart_left > 0 {
            return Ok(());
        }
        if self.scan.bits.have.saturating_sub(self.scan.bits.padded) >= 8 {
            return Err(JpegError::Corrupt);
        }
        self.scan.bits.acc = 0;
        self.scan.bits.have = 0;
        self.scan.bits.padded = 0;
        let expected = self.scan.next_restart;
        loop {
            if let Some(code) = self.scan.bits.marker {
                if code != 0xD0 | (expected & 7) {
                    return Err(JpegError::Corrupt);
                }
                self.scan.bits.marker = None;
                break;
            }
            if self.scan.buf_at >= self.scan.buf_len {
                let rem = self.scan.end_off.saturating_sub(self.scan.file_pos);
                if rem == 0 {
                    return Err(JpegError::Truncated);
                }
                let want = (rem as usize).min(self.win.len());
                (self.read_at)(self.scan.file_pos, &mut self.win[..want])?;
                self.scan.file_pos += want as u32;
                self.scan.buf_at = 0;
                self.scan.buf_len = want as u16;
            }
            let byte = self.win[usize::from(self.scan.buf_at)];
            self.scan.buf_at += 1;
            if byte == 0xFF {
                self.scan.bits.ff = true;
            } else if self.scan.bits.ff && byte != 0 {
                self.scan.bits.ff = false;
                self.scan.bits.marker = Some(byte);
            } else {
                return Err(JpegError::Corrupt);
            }
        }
        self.scan.preds = [0; 3];
        self.scan.eobrun = 0;
        self.scan.restart_left = r;
        self.scan.next_restart = (self.scan.next_restart + 1) & 7;
        Ok(())
    }

    fn verify_data_remains(&mut self) -> Result<(), JpegError> {
        self.refill_bits()?;
        if self.scan.bits.marker.is_some() && self.scan.bits.have <= self.scan.bits.padded {
            return Err(JpegError::Truncated);
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn advance_scan_block(
    scan: &mut ProgScan,
    bx: u32,
    by: u32,
    blocks_x: u32,
    tables_mem: &[u8],
    base: usize,
    block: &mut [i32; 64],
    ac_nonzero: &mut bool,
    win: &mut [u8],
    read_at: &mut ReadAt<'_>,
) -> Result<(), JpegError> {
    let ss = scan.ss;
    let se = scan.se;
    let ah = scan.ah;
    let al = scan.al;

    if scan.count == 1 {
        let dc_slot = scan.dc_tables[0];
        let ac_slot = scan.ac_table;
        let restart_interval = scan.restart_interval;
        let mut cursor = ScanCursor { scan, win, read_at };
        cursor.before_mcu_restart()?;
        // An EOBRUN can cover blocks with no remaining entropy bits. An AC
        // refinement run still needs one correction bit for each nonzero
        // coefficient in its spectral band, but all-zero blocks need none.
        let needs_entropy = if ss > 0 && cursor.scan.eobrun > 0 {
            ah != 0
                && (usize::from(ss)..=usize::from(se)).any(|k| block[usize::from(ZIGZAG[k])] != 0)
        } else {
            true
        };
        if needs_entropy {
            cursor.verify_data_remains()?;
        }
        if ss == 0 {
            if ah == 0 {
                let huff = Huffman::of(
                    tables_mem,
                    base + QUANT_BYTES + usize::from(dc_slot) * SLOT_BYTES,
                )?;
                let sym = cursor.decode_huff(&huff)?;
                let val = if sym == 0 {
                    0
                } else {
                    cursor.receive(u32::from(sym))?
                };
                cursor.scan.preds[0] = cursor.scan.preds[0].wrapping_add(val);
                block[0] = i32::from(cursor.scan.preds[0] as i16) << al;
            } else if cursor.read_bit()? != 0 {
                block[0] |= 1 << al;
            }
        } else {
            let huff = Huffman::of(
                tables_mem,
                base + QUANT_BYTES + usize::from(ac_slot) * SLOT_BYTES,
            )?;
            decode_ac_scan_block(&mut cursor, &huff, ss, se, ah, al, block, ac_nonzero)?;
        }
        if restart_interval != 0 {
            cursor.scan.restart_left = cursor.scan.restart_left.saturating_sub(1);
        }
        let _ = (bx, by, blocks_x);
        return Ok(());
    }

    // Interleaved DC scan (`ss == 0`, `count > 1`):
    let luma_idx = usize::from(scan.luma_idx);
    let lx = usize::from(scan.luma_x);
    let ly = usize::from(scan.luma_y);
    let across = (bx as usize) % lx;
    let down = (by as usize) % ly;
    let at_mcu_start = across == 0;
    let dc_tables = scan.dc_tables;
    let count = usize::from(scan.count);
    let comp_blocks = scan.comp_blocks;
    let mut cursor = ScanCursor { scan, win, read_at };

    if at_mcu_start {
        cursor.before_mcu_restart()?;
        cursor.verify_data_remains()?;
        // Skip components before luma in MCU order:
        for c in 0..luma_idx {
            let huff = if ah == 0 {
                Some(Huffman::of(
                    tables_mem,
                    base + QUANT_BYTES + usize::from(dc_tables[c]) * SLOT_BYTES,
                )?)
            } else {
                None
            };
            for _ in 0..comp_blocks[c] {
                consume_dc_unit(&mut cursor, huff.as_ref(), c, ah)?;
            }
        }
        // If `luma_y == 2` and we're on row 1 of the MCU, skip row 0's `luma_x` luma blocks first.
        if ly == 2 && down == 1 {
            let huff = if ah == 0 {
                Some(Huffman::of(
                    tables_mem,
                    base + QUANT_BYTES + usize::from(dc_tables[luma_idx]) * SLOT_BYTES,
                )?)
            } else {
                None
            };
            for _ in 0..lx {
                consume_dc_unit(&mut cursor, huff.as_ref(), luma_idx, ah)?;
            }
        }
    }

    // Decode this `(across, down)` luma block:
    if ah == 0 {
        let huff = Huffman::of(
            tables_mem,
            base + QUANT_BYTES + usize::from(dc_tables[luma_idx]) * SLOT_BYTES,
        )?;
        let sym = cursor.decode_huff(&huff)?;
        let val = if sym == 0 {
            0
        } else {
            cursor.receive(u32::from(sym))?
        };
        cursor.scan.preds[luma_idx] = cursor.scan.preds[luma_idx].wrapping_add(val);
        block[0] = i32::from(cursor.scan.preds[luma_idx] as i16) << al;
    } else if cursor.read_bit()? != 0 {
        block[0] |= 1 << al;
    }
    let _ = count;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_interleaved_mcu(
    scan: &mut ProgScan,
    bx: u32,
    by: u32,
    tables_mem: &[u8],
    base: usize,
    win: &mut [u8],
    read_at: &mut ReadAt<'_>,
) -> Result<(), JpegError> {
    let luma_idx = usize::from(scan.luma_idx);
    let lx = usize::from(scan.luma_x);
    let ly = usize::from(scan.luma_y);
    let across = (bx as usize) % lx;
    let down = (by as usize) % ly;
    let ah = scan.ah;
    let count = usize::from(scan.count);
    let dc_tables = scan.dc_tables;
    let comp_blocks = scan.comp_blocks;
    let restart_interval = scan.restart_interval;
    let mut cursor = ScanCursor { scan, win, read_at };

    // Remaining luma blocks in this row of the MCU (e.g. odd image width padding)
    let remaining_in_row = (lx - 1) - across;
    let remaining_after_row = if ly == 2 && down == 0 { lx } else { 0 };
    if remaining_in_row + remaining_after_row > 0 {
        let huff = if ah == 0 {
            Some(Huffman::of(
                tables_mem,
                base + QUANT_BYTES + usize::from(dc_tables[luma_idx]) * SLOT_BYTES,
            )?)
        } else {
            None
        };
        for _ in 0..(remaining_in_row + remaining_after_row) {
            consume_dc_unit(&mut cursor, huff.as_ref(), luma_idx, ah)?;
        }
    }

    // Components after luma in this MCU:
    for c in (luma_idx + 1)..count {
        let huff = if ah == 0 {
            Some(Huffman::of(
                tables_mem,
                base + QUANT_BYTES + usize::from(dc_tables[c]) * SLOT_BYTES,
            )?)
        } else {
            None
        };
        for _ in 0..comp_blocks[c] {
            consume_dc_unit(&mut cursor, huff.as_ref(), c, ah)?;
        }
    }

    if restart_interval != 0 {
        cursor.scan.restart_left = cursor.scan.restart_left.saturating_sub(1);
    }
    Ok(())
}

fn consume_dc_unit(
    cursor: &mut ScanCursor<'_, '_>,
    huff: Option<&Huffman<'_>>,
    comp: usize,
    ah: u8,
) -> Result<(), JpegError> {
    if let Some(huff) = huff {
        let sym = cursor.decode_huff(huff)?;
        let val = if sym == 0 {
            0
        } else {
            cursor.receive(u32::from(sym))?
        };
        cursor.scan.preds[comp] = cursor.scan.preds[comp].wrapping_add(val);
    } else {
        let _ = (ah, cursor.read_bit()?);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_ac_scan_block(
    cursor: &mut ScanCursor<'_, '_>,
    huff: &Huffman<'_>,
    ss: u8,
    se: u8,
    ah: u8,
    al: u8,
    block: &mut [i32; 64],
    ac_nonzero: &mut bool,
) -> Result<(), JpegError> {
    if ah == 0 {
        if cursor.scan.eobrun > 0 {
            cursor.scan.eobrun -= 1;
            return Ok(());
        }
        let mut k = usize::from(ss);
        let se = usize::from(se);
        while k <= se {
            let rs = cursor.decode_huff(huff)?;
            let r = usize::from(rs >> 4);
            let s = u32::from(rs & 15);
            if s != 0 {
                k += r;
                if k > se {
                    return Err(JpegError::Corrupt);
                }
                let v = cursor.receive(s)? << al;
                let natural = usize::from(ZIGZAG[k]);
                block[natural] = v;
                if v != 0 {
                    *ac_nonzero = true;
                }
                k += 1;
            } else if r == 15 {
                if k + 15 > se {
                    return Err(JpegError::Corrupt);
                }
                k += 16;
            } else {
                let extra = if r > 0 {
                    cursor.read_bits(r as u32)?
                } else {
                    0
                };
                cursor.scan.eobrun = ((1u32 << r) + extra - 1) as u16;
                break;
            }
        }
        return Ok(());
    }

    // Successive-approximation AC refinement (`ah > 0`).
    let p1 = 1i32 << al;
    let m1 = (-1i32) << al;
    let mut k = usize::from(ss);
    let se = usize::from(se);
    if cursor.scan.eobrun == 0 {
        while k <= se {
            let rs = cursor.decode_huff(huff)?;
            let mut r = rs >> 4;
            let s = rs & 15;
            let new_val = if s == 1 {
                if cursor.read_bit()? != 0 {
                    p1
                } else {
                    m1
                }
            } else if s == 0 {
                if r < 15 {
                    let extra = if r > 0 {
                        cursor.read_bits(u32::from(r))?
                    } else {
                        0
                    };
                    cursor.scan.eobrun = ((1u32 << r) + extra) as u16;
                    break;
                }
                0
            } else {
                return Err(JpegError::Corrupt);
            };
            loop {
                if k > se {
                    return Err(JpegError::Corrupt);
                }
                let natural = usize::from(ZIGZAG[k]);
                if block[natural] != 0 {
                    if cursor.read_bit()? != 0 && (block[natural] & p1) == 0 {
                        block[natural] += if block[natural] >= 0 { p1 } else { m1 };
                    }
                    k += 1;
                } else if r > 0 {
                    r -= 1;
                    k += 1;
                } else {
                    block[natural] = new_val;
                    if new_val != 0 {
                        *ac_nonzero = true;
                    }
                    k += 1;
                    break;
                }
            }
        }
    }
    if cursor.scan.eobrun > 0 {
        while k <= se {
            let natural = usize::from(ZIGZAG[k]);
            if block[natural] != 0 && cursor.read_bit()? != 0 && (block[natural] & p1) == 0 {
                block[natural] += if block[natural] >= 0 { p1 } else { m1 };
            }
            k += 1;
        }
        cursor.scan.eobrun -= 1;
    }
    Ok(())
}

#[cfg(test)]
mod seq_reader_tests {
    use super::*;
    use core::cell::Cell;

    #[test]
    fn entropy_marker_walk_honors_cancellation_with_plain_read_at() {
        // A reader that only copies bytes has no cancellation responsibility.
        // The marker walker itself must stop between refills of a long scan.
        let mut bytes = vec![0x35u8; 1024];
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        let calls = Cell::new(0usize);
        let mut read_at = |at: u32, dst: &mut [u8]| {
            calls.set(calls.get() + 1);
            let start = at as usize;
            let end = start + dst.len();
            dst.copy_from_slice(bytes.get(start..end).ok_or(JpegError::Truncated)?);
            Ok(())
        };
        let mut input = [0u8; 64];
        let mut reader = SeqReader::new(&mut input, bytes.len() as u32, &mut read_at);
        let result = reader.scan_to_next_marker(&|| calls.get() == 3);
        assert_eq!(result, Err(JpegError::Sink(SinkError::Cancelled)));
        assert_eq!(calls.get(), 3, "cancellation stops subsequent SD reads");
    }
}
