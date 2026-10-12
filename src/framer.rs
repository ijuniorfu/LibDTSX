//! DTS-HD framing: a 16-bit big-endian core frame followed by its extension substream (EXSS),
//! found in input of any chunking. Bytes outside any frame are skipped up to the next frame
//! and reported, so the engine can flag the gap and keep the input timeline.

use crate::types::NO_PTS;
use std::collections::VecDeque;

const CORE_SYNC: [u8; 4] = [0x7F, 0xFE, 0x80, 0x01];
const EXSS_SYNC: [u8; 4] = [0x64, 0x58, 0x20, 0x25];
/// An EXSS whose size has not parsed from this many bytes is damage, not a short read.
const MAX_EXSS_HEADER_PROBE: usize = 64 * 1024;
/// Pushes whose bytes are still unconsumed, tracked for pts attribution.
const MAX_PENDING_CHUNKS: usize = 1 << 16;

/// Where a frame sits in the input since the last reset.
#[derive(Clone, Copy, Debug)]
pub struct AuInfo {
    /// Frames before this one since reset, decodable or not; includes the estimate for skipped
    /// bytes (see `estimated`).
    pub ordinal: u64,
    /// Byte offset of the frame's first byte since reset.
    pub offset: u64,
    /// Samples per channel of the core frame, at the core's rate.
    pub core_samples: u32,
    /// Frames estimated into `ordinal` for skipped bytes since the previous frame.
    pub estimated_before: u64,
    /// Bytes skipped since the previous frame (damage or a leading fragment).
    pub skipped_before: u64,
}

pub enum Next<'a> {
    NeedData,
    /// A DTS-HD frame: its core bytes and its extension substream.
    Au(AuInfo, &'a [u8], &'a [u8]),
    /// A core frame without an extension substream: DTS, not DTS-HD.
    CoreOnly(AuInfo),
}

struct Chunk {
    start: u64,
    pts: i64,
    serial: u64,
}

#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
    start: usize,
    /// Bytes consumed (framed or skipped) since reset.
    consumed: u64,
    pushed: u64,
    frames: u64,
    framed_bytes: u64,
    skipped: u64,
    /// Bytes skipped since create() before the first frame was found (survives reset).
    pub unframed_since_create: u64,
    found_since_create: bool,
    /// Ordinals include an estimate since reset.
    pub estimated: bool,
    chunks: VecDeque<Chunk>,
    chunk_serial: u64,
    au_chunk: Option<u64>,
    au_in_chunk: u32,
}

impl Framer {
    pub fn reset(&mut self) {
        self.buf.clear();
        self.start = 0;
        self.consumed = 0;
        self.pushed = 0;
        self.frames = 0;
        self.framed_bytes = 0;
        self.skipped = 0;
        self.estimated = false;
        self.chunks.clear();
        self.au_chunk = None;
        self.au_in_chunk = 0;
    }

    pub fn pending(&self) -> usize {
        self.buf.len() - self.start
    }

    pub fn push(&mut self, data: &[u8], pts: i64) {
        // Past this many pushes waiting, new bytes join the previous push's record; it keeps its
        // pts only if the new push has the same one.
        if self.chunks.len() < MAX_PENDING_CHUNKS {
            self.chunks.push_back(Chunk {
                start: self.pushed,
                pts,
                serial: self.chunk_serial,
            });
            self.chunk_serial += 1;
        } else if let Some(last) = self.chunks.back_mut()
            && last.pts != pts
        {
            last.pts = NO_PTS;
        }
        self.pushed += data.len() as u64;
        if self.start > 0 && self.start * 2 >= self.buf.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// The pts of the push holding `offset`, and how many frames began earlier in it.
    pub fn attribute(&mut self, offset: u64) -> (i64, u32) {
        while self.chunks.len() >= 2 && self.chunks[1].start <= offset {
            self.chunks.pop_front();
        }
        let Some(chunk) = self.chunks.front().filter(|c| c.start <= offset) else {
            return (NO_PTS, 0);
        };
        if self.au_chunk == Some(chunk.serial) {
            self.au_in_chunk += 1;
        } else {
            self.au_chunk = Some(chunk.serial);
            self.au_in_chunk = 0;
        }
        (chunk.pts, self.au_in_chunk)
    }

    fn skip(&mut self, n: usize) {
        self.start += n;
        self.consumed += n as u64;
        self.skipped += n as u64;
        if !self.found_since_create {
            self.unframed_since_create += n as u64;
        }
    }

    pub fn next(&mut self) -> Next<'_> {
        loop {
            let avail = &self.buf[self.start..];
            if avail.len() < 4 {
                return Next::NeedData;
            }
            if avail[..4] != CORE_SYNC {
                match avail[1..].windows(4).position(|w| w == CORE_SYNC) {
                    Some(i) => self.skip(i + 1),
                    None => {
                        // Keep the last three bytes: they may start a syncword.
                        let n = avail.len() - 3;
                        self.skip(n);
                        return Next::NeedData;
                    }
                }
                continue;
            }
            let header = match dca::parse_header(avail) {
                Ok(header) => header,
                Err(dca::HeaderParseError::InsufficientData) => return Next::NeedData,
                Err(_) => {
                    self.skip(1);
                    continue;
                }
            };
            let core_len = header.frame_size;
            let core_samples = header.npcmblocks * 32;
            if avail.len() < core_len + 4 {
                return Next::NeedData;
            }
            let after = &avail[core_len..core_len + 4];
            if after == CORE_SYNC {
                let info = self.take(core_len, core_samples);
                return Next::CoreOnly(info);
            }
            if after != EXSS_SYNC {
                // A syncword inside other data, or a frame damaged past its header.
                self.skip(1);
                continue;
            }
            let exss = &avail[core_len..];
            let Some(exss_len) = dca::exss_substream_size(exss) else {
                if exss.len() > MAX_EXSS_HEADER_PROBE {
                    self.skip(1);
                    continue;
                }
                return Next::NeedData;
            };
            if exss_len < 4 {
                self.skip(1);
                continue;
            }
            let mut total = core_len + exss_len;
            if avail.len() < total + 4 {
                // Complete as it stands: at the end of a push (a demuxed packet is one whole
                // frame) there is nothing after it to check for further substreams.
                if avail.len() < total {
                    return Next::NeedData;
                }
            } else {
                // Further extension substreams of the same frame, which the decoder does not read.
                while avail.len() >= total + 4 && avail[total..total + 4] == EXSS_SYNC {
                    match dca::exss_substream_size(&avail[total..]) {
                        Some(n) if n >= 4 && avail.len() >= total + n => total += n,
                        Some(n) if n >= 4 => return Next::NeedData,
                        _ => break,
                    }
                }
            }
            let info = self.take(total, core_samples);
            let frame = &self.buf[self.start - total..self.start];
            return Next::Au(
                info,
                &frame[..core_len],
                &frame[core_len..core_len + exss_len],
            );
        }
    }

    /// Consumes a frame of `len` bytes at the read position and describes it.
    fn take(&mut self, len: usize, core_samples: u32) -> AuInfo {
        let mut estimated_before = 0;
        let skipped_before = std::mem::take(&mut self.skipped);
        if skipped_before > 0 && self.frames > 0 {
            // Damage between frames: estimate the frames it held from the average frame size.
            let average = self.framed_bytes as f64 / self.frames as f64;
            estimated_before = (skipped_before as f64 / average).round() as u64;
            self.estimated |= estimated_before > 0;
        }
        self.frames += estimated_before;
        let info = AuInfo {
            ordinal: self.frames,
            offset: self.consumed,
            core_samples,
            estimated_before,
            skipped_before,
        };
        self.frames += 1;
        self.framed_bytes += len as u64;
        self.start += len;
        self.consumed += len as u64;
        self.found_since_create = true;
        info
    }
}
