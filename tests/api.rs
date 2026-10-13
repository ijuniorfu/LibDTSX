//! Behaviour of the C API that the sample harness does not reach: argument handling, the refusal
//! contract, damage between frames, and a frame whose DTS:X extension is damaged. Runs in-process
//! through the same `extern "C"` functions the xcframework exports.
//!
//! The sample tests read the standard DTS:X stream from `$DTSX_SAMPLE`, default
//! `target/work/samples/dtsx-object-emulator.dts` (see harness/verify.sh), and are skipped
//! without it.

use dtsx::types::*;
use dtsx::*;
use std::ptr;

fn sample() -> Option<Vec<u8>> {
    let path = std::env::var_os("DTSX_SAMPLE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/work/samples/dtsx-object-emulator.dts")
        });
    let data = std::fs::read(&path).ok();
    if data.is_none() {
        eprintln!("skipped: no DTS:X sample at {}", path.display());
    }
    data
}

fn empty_block() -> Block {
    Block {
        sample_rate: 0,
        frame_count: 0,
        channel_count: 0,
        presentation: 0,
        flags: 0,
        access_unit_frames: 0,
        pts: 0,
        pts_au_index: 0,
        layout_serial: 0,
        sample_position: 0,
        input_access_unit: 0,
        input_frame_offset: 0,
        channels: ptr::null(),
        speakers: ptr::null(),
        metadata: ptr::null(),
    }
}

struct Decoder(*mut DTSXDecoder);

impl Decoder {
    fn new() -> Self {
        let d = dtsx_decoder_create();
        assert!(!d.is_null());
        Self(d)
    }

    fn push(&self, data: &[u8]) -> Status {
        unsafe { dtsx_decoder_push(self.0, data.as_ptr(), data.len(), NO_PTS) }
    }

    fn pull(&self) -> (Status, Block) {
        let mut block = empty_block();
        let status = unsafe { dtsx_decoder_pull(self.0, &mut block) };
        (status, block)
    }

    /// Pushes `data` in `chunk`-byte pieces and returns (flags, input_frame_offset, frame_count)
    /// of every block, stopping at the first status other than OK or NEED_MORE_DATA.
    fn decode(&self, data: &[u8], chunk: usize) -> (Status, Vec<(u32, u64, u32)>) {
        let mut blocks = Vec::new();
        for piece in data.chunks(chunk) {
            let status = self.push(piece);
            if status != OK {
                return (status, blocks);
            }
            loop {
                match self.pull() {
                    (OK, b) => blocks.push((b.flags, b.input_frame_offset, b.frame_count)),
                    (NEED_MORE_DATA, _) => break,
                    (status, _) => return (status, blocks),
                }
            }
        }
        (OK, blocks)
    }

    fn stats(&self) -> Stats {
        let mut stats = Stats::default();
        assert_eq!(unsafe { dtsx_decoder_get_stats(self.0, &mut stats) }, OK);
        stats
    }

    fn last_error(&self) -> String {
        unsafe { std::ffi::CStr::from_ptr(dtsx_decoder_last_error(self.0)) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { dtsx_decoder_destroy(self.0) };
    }
}

/// Byte offset of every DTS-HD frame: the core frame size, then the extension substream's.
fn frame_offsets(data: &[u8]) -> Vec<usize> {
    let bits = |at: usize, bit: usize, n: usize| {
        (bit..bit + n).fold(0usize, |v, b| {
            v << 1 | (data[at + b / 8] >> (7 - b % 8) & 1) as usize
        })
    };
    let mut offsets = Vec::new();
    let mut at = 0;
    while at + 16 <= data.len() && bits(at, 0, 32) == 0x7FFE_8001 {
        offsets.push(at);
        let core = bits(at, 46, 14) + 1;
        let x = at + core;
        let wide = bits(x, 42, 1) == 1;
        at =
            x + bits(
                x,
                43 + if wide { 12 } else { 8 },
                if wide { 20 } else { 16 },
            ) + 1;
    }
    offsets
}

#[test]
fn arguments_and_empty_handles() {
    let mut block = empty_block();
    let mut info = StreamInfo::default();
    let mut stats = Stats::default();
    unsafe {
        assert_eq!(
            dtsx_decoder_push(ptr::null_mut(), [0u8].as_ptr(), 1, NO_PTS),
            ERR_NULL
        );
        assert_eq!(dtsx_decoder_pull(ptr::null_mut(), &mut block), ERR_NULL);
        assert_eq!(dtsx_decoder_reset(ptr::null_mut()), ERR_NULL);
        assert_eq!(
            dtsx_decoder_get_stream_info(ptr::null(), &mut info),
            ERR_NULL
        );
        assert_eq!(dtsx_decoder_get_stats(ptr::null(), &mut stats), ERR_NULL);
        assert_eq!(*dtsx_decoder_last_error(ptr::null()), 0);
        dtsx_decoder_destroy(ptr::null_mut());
    }
    let d = Decoder::new();
    unsafe {
        assert_eq!(dtsx_decoder_push(d.0, ptr::null(), 0, NO_PTS), OK);
        assert_eq!(dtsx_decoder_push(d.0, ptr::null(), 4, NO_PTS), ERR_NULL);
        assert_eq!(dtsx_decoder_pull(d.0, ptr::null_mut()), ERR_NULL);
        assert_eq!(dtsx_decoder_get_stream_info(d.0, &mut info), ERR_NOT_READY);
    }
    assert_eq!(d.pull().0, NEED_MORE_DATA);
    assert_eq!(dtsx_api_version(), API_VERSION);
    assert_eq!(dtsx_selftest(), OK);
    let name = |p| {
        unsafe { std::ffi::CStr::from_ptr(dtsx_presentation_name(p)) }
            .to_str()
            .unwrap()
    };
    assert_eq!(name(PRESENTATION_HEIGHT), "7.1.4");
    assert_eq!(name(PRESENTATION_OBJECTS_D4), "7.1.4+5");
    assert_eq!(name(99), "?");
    let speaker = |s| {
        unsafe { std::ffi::CStr::from_ptr(dtsx_speaker_name(s)) }
            .to_str()
            .unwrap()
    };
    assert_eq!(speaker(SPEAKER_TBR), "Tbr");
    assert_eq!(speaker(SPEAKER_OBJECT), "Obj");
}

#[test]
fn input_without_dts_frames_is_refused_and_the_refusal_is_sticky() {
    let d = Decoder::new();
    let (status, blocks) = d.decode(&vec![0x55; (1 << 20) + 4096], 65_536);
    assert_eq!(status, ERR_NOT_DTSX);
    assert!(blocks.is_empty());
    assert!(
        d.last_error().contains("no DTS frame"),
        "{}",
        d.last_error()
    );
    assert_eq!(d.push(&[0]), ERR_NOT_DTSX);
    assert_eq!(d.pull().0, ERR_NOT_DTSX);
    assert_eq!(unsafe { dtsx_decoder_reset(d.0) }, OK);
    assert_eq!(d.pull().0, ERR_NOT_DTSX);
}

#[test]
fn a_dts_core_without_an_extension_is_not_dtsx() {
    let Some(data) = sample() else { return };
    // The sample's core frames alone: the stream as a DTS core decoder sees it.
    let offsets = frame_offsets(&data);
    let mut core_only = Vec::new();
    for &at in offsets.iter().take(8) {
        let core = (data[at + 5] as usize & 0x3) << 12
            | (data[at + 6] as usize) << 4
            | (data[at + 7] as usize) >> 4;
        core_only.extend_from_slice(&data[at..at + core + 1]);
    }
    let d = Decoder::new();
    let (status, blocks) = d.decode(&core_only, 4096);
    assert_eq!(status, ERR_NOT_DTSX, "{}", d.last_error());
    assert!(blocks.is_empty());
}

#[test]
fn the_first_frames_prime_and_the_timeline_counts_them() {
    let Some(data) = sample() else { return };
    let offsets = frame_offsets(&data);
    let d = Decoder::new();
    let (status, blocks) = d.decode(&data[..offsets[40]], 1000);
    assert_eq!(status, OK);
    // 40 frames, the first two priming: 38 blocks, the first at 1024 frames.
    assert_eq!(blocks.len(), 38);
    assert_eq!(
        blocks[0].0 & (BLOCK_DISCONTINUITY | BLOCK_LAYOUT_CHANGED),
        BLOCK_DISCONTINUITY | BLOCK_LAYOUT_CHANGED
    );
    assert_eq!(blocks[0].1, 1024);
    for pair in blocks.windows(2) {
        assert_eq!(pair[1].1, pair[0].1 + u64::from(pair[0].2));
        assert_eq!(pair[1].0 & BLOCK_DISCONTINUITY, 0);
    }
    assert_eq!(d.stats().access_units_skipped, 2);

    // A seek: the timeline restarts, priming again, and the layout is kept.
    assert_eq!(unsafe { dtsx_decoder_reset(d.0) }, OK);
    let (status, resumed) = d.decode(&data[offsets[100]..offsets[110]], 777);
    assert_eq!(status, OK);
    assert_eq!(resumed.len(), 8);
    assert_eq!(resumed[0].1, 1024);
    assert_eq!(
        resumed[0].0 & (BLOCK_DISCONTINUITY | BLOCK_LAYOUT_CHANGED),
        BLOCK_DISCONTINUITY
    );
}

#[test]
fn damage_between_frames_is_skipped_with_an_estimated_timeline() {
    let Some(data) = sample() else { return };
    let offsets = frame_offsets(&data);
    // Frames 0..20, then the bytes of frames 20..23 overwritten with garbage, then on.
    let mut damaged = data[..offsets[60]].to_vec();
    for byte in &mut damaged[offsets[20]..offsets[23]] {
        *byte = 0xA5;
    }
    let d = Decoder::new();
    let (status, blocks) = d.decode(&damaged, 4096);
    assert_eq!(status, OK);
    let stats = d.stats();
    assert_eq!(stats.sync_errors, 1);
    // Output resumes after the gap, priming again: two frames after the first one past it.
    let resumed = blocks
        .iter()
        .position(|b| b.0 & BLOCK_DISCONTINUITY != 0 && b.1 > 1024)
        .unwrap();
    let (flags, offset, _) = blocks[resumed];
    assert_eq!(resumed, 18);
    assert_ne!(flags & BLOCK_INPUT_OFFSET_ESTIMATED, 0);
    // The three frames' bytes are estimated from the average frame size; frame sizes vary, so
    // the estimate may be a frame off.
    assert!((offset as i64 - 25 * 512).abs() <= 512, "{offset}");
    assert!(
        blocks[resumed..]
            .iter()
            .all(|b| b.0 & BLOCK_INPUT_OFFSET_ESTIMATED != 0)
    );
    assert_eq!(blocks.len(), 60 - 2 - 3 - 2);
}

#[test]
fn repeated_gaps_do_not_add_time_to_the_timeline() {
    let Some(data) = sample() else { return };
    let offsets = frame_offsets(&data);
    // One frame repeated, so every gap holds an exact number of frames: 50 at 100..150 and at
    // 250..300. The second estimate must not count the first gap's frames as framed bytes.
    let size = offsets[1];
    let mut stream = data[..size].repeat(500);
    for (start, end) in [(100, 150), (250, 300)] {
        stream[start * size..end * size].fill(0xA5);
    }
    let d = Decoder::new();
    let (status, blocks) = d.decode(&stream, 4096);
    assert_eq!(status, OK);
    let resumed: Vec<u64> = blocks
        .iter()
        .filter(|b| b.0 & BLOCK_DISCONTINUITY != 0)
        .map(|b| b.1 / 512)
        .collect();
    assert_eq!(resumed, [2, 152, 302]);
    let (_, offset, count) = *blocks.last().unwrap();
    assert_eq!(offset + u64::from(count), 500 * 512);
}

#[test]
fn a_frame_whose_extension_is_damaged_plays_its_bed_as_authored() {
    let Some(data) = sample() else { return };
    let offsets = frame_offsets(&data);
    let mut damaged = data[..offsets[30]].to_vec();
    // The DTS:X extension payload sits at the end of the frame, from its syncword on.
    let frame = &mut damaged[offsets[20]..offsets[21]];
    let sync = frame
        .windows(4)
        .rposition(|w| w == [0x02, 0x00, 0x08, 0x50])
        .expect("DTS:X syncword");
    for byte in &mut frame[sync + 4..sync + 24] {
        *byte ^= 0xFF;
    }
    let d = Decoder::new();
    let mut flagged = Vec::new();
    for piece in damaged.chunks(4096) {
        assert_eq!(d.push(piece), OK);
        while let (OK, b) = d.pull() {
            if b.flags & BLOCK_BED_AS_AUTHORED != 0 {
                flagged.push(b.input_access_unit);
                // The extension's feeds are silent; the layout stays the stream's.
                let channels =
                    unsafe { std::slice::from_raw_parts(b.channels, b.channel_count as usize) };
                assert_eq!(b.channel_count, 12);
                for &plane in &channels[8..] {
                    let plane =
                        unsafe { std::slice::from_raw_parts(plane, b.frame_count as usize) };
                    assert!(plane.iter().all(|&x| x == 0.0));
                }
            }
            assert_eq!(
                b.flags & BLOCK_LAYOUT_CHANGED != 0,
                b.input_access_unit == 2
            );
        }
    }
    // The damaged frame alone, or, when the damage reaches the extension's audio, the frame and
    // the two that prime after its decode failed.
    assert!(flagged == vec![20], "{flagged:?}: {}", d.last_error());
    assert_eq!(d.stats().bed_as_authored, 1);
}

#[test]
fn chunking_does_not_change_the_output() {
    let Some(data) = sample() else { return };
    let offsets = frame_offsets(&data);
    let data = &data[..offsets[300]];
    let digest = |chunk: usize| {
        let d = Decoder::new();
        let mut hash = 0xcbf29ce484222325u64;
        for piece in data.chunks(chunk) {
            assert_eq!(d.push(piece), OK);
            while let (OK, b) = d.pull() {
                for c in 0..b.channel_count as usize {
                    let plane = unsafe {
                        std::slice::from_raw_parts(*b.channels.add(c), b.frame_count as usize)
                    };
                    for x in plane {
                        hash = (hash ^ u64::from(x.to_bits())).wrapping_mul(0x100000001b3);
                    }
                }
            }
        }
        hash
    };
    let whole = digest(data.len());
    assert_eq!(digest(4096), whole);
    assert_eq!(digest(317), whole);
    assert_eq!(digest(1), whole);
}

#[test]
fn a_pending_frame_on_entry_counts_on_the_timeline() {
    let Some(data) = sample() else { return };
    let offsets = frame_offsets(&data);
    // Entered at some frames the lossless layer still buffers (peak bit rate smoothing) and the
    // first frame comes back pending: no output of its own, but 512 frames of the source. Which
    // frames those are depends on the encode, so the first one is searched for.
    let pending_on_entry = |entry: usize| {
        let d = Decoder::new();
        let (status, _) = d.decode(&data[offsets[entry]..offsets[entry + 3]], 65_536);
        status == OK && d.stats().access_units_skipped == 3
    };
    let Some(entry) = (0..offsets.len() - 21).find(|&e| pending_on_entry(e)) else {
        eprintln!("skipped: no frame of this sample is pending on entry");
        return;
    };
    let end = entry + 20;
    let continuous = Decoder::new();
    let mut reference = std::collections::HashMap::new();
    for piece in data[..offsets[end]].chunks(65_536) {
        assert_eq!(continuous.push(piece), OK);
        while let (OK, b) = continuous.pull() {
            if b.input_access_unit >= entry as u64 {
                let plane =
                    unsafe { std::slice::from_raw_parts(*b.channels, b.frame_count as usize) };
                reference.insert(b.input_frame_offset - entry as u64 * 512, plane.to_vec());
            }
        }
    }
    let d = Decoder::new();
    let mut first = None;
    let mut matching = 0;
    for piece in data[offsets[entry]..offsets[end]].chunks(4096) {
        assert_eq!(d.push(piece), OK);
        while let (OK, b) = d.pull() {
            first.get_or_insert(b.input_frame_offset);
            // The offset is the source position: the same frames as the continuous decode's there.
            assert_eq!(b.input_frame_offset, b.input_access_unit * 512);
            let plane = unsafe { std::slice::from_raw_parts(*b.channels, b.frame_count as usize) };
            if reference.get(&b.input_frame_offset) == Some(&plane.to_vec()) {
                matching += 1;
            }
        }
    }
    let stats = d.stats();
    // The pending frame, then the two priming frames.
    assert_eq!(stats.access_units_skipped, 3);
    assert_eq!(first, Some(3 * 512));
    // Converged to the continuous decode, placed at the same source position.
    assert!(
        matching >= 10,
        "{matching} of 17 blocks match the continuous decode"
    );
}
