//! Decodes a DTS-HD elementary stream through the C API and reports what a renderer gets: the
//! verdict, the presentation, every channel's role and level, the metadata cadence and object
//! positions, flagged blocks and the decode speed. Optionally writes the decoded planes,
//! interleaved in the block's channel order, as Float32 for comparisons.
//!
//! cargo run --release --example dtsx_info -- <stream.dts | -> [--raw out.f32] [--chunk bytes]
//!
//! An FFmpeg pipe works as input: ffmpeg -i film.mkv -map 0:a:0 -c copy -f dts - | ... -- -

use dtsx::types::*;
use dtsx::*;
use std::ffi::CStr;
use std::io::{BufWriter, Read, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let Some(input) = args.get(1) else {
        return Err("usage: dtsx_info <stream.dts | -> [--raw out.f32] [--chunk bytes]".into());
    };
    let mut raw = None;
    let mut chunk = 65_536usize;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--raw" => raw = Some(BufWriter::new(std::fs::File::create(&args[i + 1])?)),
            "--chunk" => chunk = args[i + 1].parse()?,
            other => return Err(format!("unknown argument {other}").into()),
        }
        i += 2;
    }
    let mut source: Box<dyn Read> = if input == "-" {
        Box::new(std::io::stdin().lock())
    } else {
        Box::new(std::fs::File::open(input)?)
    };

    let decoder = dtsx_decoder_create();
    let started = std::time::Instant::now();
    let mut buffer = vec![0u8; chunk.max(1)];
    let mut report = Report::default();
    let mut status = OK;
    'read: loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        status = unsafe { dtsx_decoder_push(decoder, buffer.as_ptr(), n, NO_PTS) };
        if status != OK {
            break;
        }
        loop {
            let mut block = std::mem::MaybeUninit::<Block>::zeroed();
            status = unsafe { dtsx_decoder_pull(decoder, block.as_mut_ptr()) };
            match status {
                OK => {
                    let block = unsafe { block.assume_init() };
                    report.block(&block, raw.as_mut())?
                }
                NEED_MORE_DATA => break,
                _ => break 'read,
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let error = unsafe { CStr::from_ptr(dtsx_decoder_last_error(decoder)) }.to_string_lossy();
    if status < 0 {
        println!("verdict: refused, status {status}: {error}");
    }
    let mut info = StreamInfo::default();
    if unsafe { dtsx_decoder_get_stream_info(decoder, &mut info) } == OK {
        let name =
            unsafe { CStr::from_ptr(dtsx_presentation_name(info.presentation)) }.to_string_lossy();
        println!(
            "stream: {} Hz, {} frames per frame, presentation {name}{}, alternate marker {}, bed mask {:#06x}",
            info.sample_rate,
            info.access_unit_frames,
            if info.experimental != 0 {
                " (experimental)"
            } else {
                ""
            },
            info.alternate_marker,
            info.bed_mask
        );
    }
    let mut stats = Stats::default();
    unsafe { dtsx_decoder_get_stats(decoder, &mut stats) };
    report.print(elapsed, &stats);
    if !error.is_empty() && status >= 0 {
        println!("last recovered error: {error}");
    }
    if let Some(mut raw) = raw {
        raw.flush()?;
    }
    unsafe { dtsx_decoder_destroy(decoder) };
    Ok(())
}

#[derive(Default)]
struct Report {
    speakers: Vec<u8>,
    rate: u32,
    frames: u64,
    blocks: u64,
    flagged: [u64; 6],
    updates: u64,
    positions: std::collections::BTreeSet<(i32, i32, i32)>,
    energy: Vec<f64>,
    peak: Vec<f32>,
    gaps: u64,
}

impl Report {
    fn block(
        &mut self,
        b: &Block,
        raw: Option<&mut BufWriter<std::fs::File>>,
    ) -> std::io::Result<()> {
        let channels = b.channel_count as usize;
        let frames = b.frame_count as usize;
        let speakers = unsafe { std::slice::from_raw_parts(b.speakers, channels) };
        if self.speakers != speakers {
            self.speakers = speakers.to_vec();
            self.energy = vec![0.0; channels];
            self.peak = vec![0.0; channels];
            let names: Vec<String> = speakers
                .iter()
                .map(|&s| {
                    unsafe { CStr::from_ptr(dtsx_speaker_name(s)) }
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            println!(
                "layout {} at {} frames: {}",
                b.layout_serial,
                self.frames,
                names.join(" ")
            );
        }
        if b.input_frame_offset != self.frames + self.gaps {
            self.gaps = b.input_frame_offset - self.frames;
        }
        self.rate = b.sample_rate;
        self.blocks += 1;
        for (bit, count) in self.flagged.iter_mut().enumerate() {
            if b.flags & (1 << bit) != 0 {
                *count += 1;
            }
        }
        let planes: Vec<&[f32]> = (0..channels)
            .map(|c| unsafe { std::slice::from_raw_parts(*b.channels.add(c), frames) })
            .collect();
        for (c, plane) in planes.iter().enumerate() {
            for &x in plane.iter() {
                self.energy[c] += f64::from(x) * f64::from(x);
                self.peak[c] = self.peak[c].max(x.abs());
            }
        }
        if !b.metadata.is_null() {
            let md = unsafe { &*b.metadata };
            for u in 0..md.update_count as usize {
                self.updates += 1;
                let update = unsafe { &*md.updates.add(u) };
                for e in 0..md.element_count as usize {
                    let element = unsafe { &*update.elements.add(e) };
                    if element.kind == ELEMENT_OBJECT && element.flags & EL_CHANGED != 0 {
                        self.positions.insert((
                            (element.azimuth * 2.0) as i32,
                            (element.elevation * 2.0) as i32,
                            (element.distance * 64.0) as i32,
                        ));
                    }
                }
            }
        }
        if let Some(raw) = raw {
            let mut bytes = Vec::with_capacity(frames * channels * 4);
            for s in 0..frames {
                for plane in &planes {
                    bytes.extend_from_slice(&plane[s].to_le_bytes());
                }
            }
            raw.write_all(&bytes)?;
        }
        self.frames += frames as u64;
        Ok(())
    }

    fn print(&self, elapsed: f64, stats: &Stats) {
        let seconds = self.frames as f64 / f64::from(self.rate.max(1));
        println!(
            "blocks={} frames={} ({seconds:.3} s) decoded in {elapsed:.2} s, {:.0}x realtime",
            self.blocks,
            self.frames,
            seconds / elapsed.max(1e-9)
        );
        println!(
            "flags: discontinuity={} layout_changed={} has_metadata={} metadata_updated={} estimated={} bed_as_authored={}",
            self.flagged[0],
            self.flagged[1],
            self.flagged[2],
            self.flagged[3],
            self.flagged[4],
            self.flagged[5]
        );
        println!(
            "stats: decoded={} skipped={} decode_errors={} sync_errors={} bed_as_authored={} updates={} input_gap_frames={}",
            stats.access_units_decoded,
            stats.access_units_skipped,
            stats.decode_errors,
            stats.sync_errors,
            stats.bed_as_authored,
            self.updates,
            self.gaps
        );
        println!("distinct object positions: {}", self.positions.len());
        if self.positions.len() <= 12 {
            for (azimuth, elevation, distance) in &self.positions {
                println!(
                    "  object at azimuth {:+.1}, elevation {:+.1}, distance {:.3}",
                    *azimuth as f32 / 2.0,
                    *elevation as f32 / 2.0,
                    *distance as f32 / 64.0
                );
            }
        }
        let db = |x: f64| 20.0 * x.max(1e-12).log10();
        for (c, &speaker) in self.speakers.iter().enumerate() {
            let name = unsafe { CStr::from_ptr(dtsx_speaker_name(speaker)) }.to_string_lossy();
            println!(
                "  {c:2} {name:>4}: rms {:+6.1} dBFS, peak {:+6.1} dBFS",
                db((self.energy[c] / self.frames.max(1) as f64).sqrt()),
                db(f64::from(self.peak[c]))
            );
        }
    }
}
