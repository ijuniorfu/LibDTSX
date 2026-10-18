//! The decoder behind a handle: framing, decoding, the DTS:X verdict, height removal, channel
//! roles, metadata and recovery. Everything here is safe Rust; `lib.rs` wraps it for C.
//!
//! Only the standard presentation is rendered: a 7.1 bed and four fixed height channels, each
//! height carried in the bed speaker under it at -3 dB (TFL in L, TFR in R, TBL in Lsr, TBR in
//! Rsr). That fold is the standard's own constant, so it is removed as such; the private
//! metadata is not read, and every frame of every film measured carried exactly this fold. The
//! alternate presentations, which carry objects, are refused, so a host plays them as 7.1.

use crate::framer::{AuInfo, Framer, Next};
use crate::position::speaker_position;
use crate::types::*;
use dca::{FoldPlan, HdDecoder, HdError, HdFrame, XPresentation};
use std::any::Any;
use std::ffi::CString;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Input the caller may push ahead of pulling before push() refuses more.
const MAX_PENDING_INPUT: usize = 8 << 20;
/// A stream with no DTS frame in this many bytes since create() is not DTS.
const MAX_UNFRAMED_BEFORE_VERDICT: u64 = 1 << 20;
/// Frames in a row that may fail to decode before the stream counts as one this library cannot
/// decode. Generous: the first frames after a seek can be damaged or mid-buffer.
const MAX_UNDECODABLE_BEFORE_VERDICT: u32 = 8;
/// Frames decoded after create() or reset() only to rebuild the core's filter and predictor
/// history, which the lossless residual is coded against. Without it the first frame entered
/// mid-stream reconstructs as distorted audio (over 110 seeks into four DTS:X streams: its
/// error a median 2.7 dB below the signal) and the second can still err by -16 dBFS; from the
/// third the error peaked at -33 dBFS and decayed to bit-exact within about ten frames.
const PRIMING_FRAMES: u32 = 2;
/// Gain of each standard height in the bed speaker under it: -3 dB, the six-bit gain code 55 of
/// the DTS downmix table (23170 / 32768).
const STANDARD_HEIGHT_FOLD: f32 = 23170.0 / 32768.0;
/// DCA speakers of the 7.1 bed the standard presentation is authored over: C, L, R, Ls, Rs, LFE,
/// Lsr, Rsr.
const BED_7_1: [usize; 8] = [0, 1, 2, 3, 4, 5, 7, 8];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pending,
    Accepted,
    Refused(Status),
}

/// Channel roles as blocks report them; `layout_serial` changes exactly when these do.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Roles {
    sample_rate: u32,
    presentation: u32,
    channels: u32,
    speakers: [u8; MAX_CHANNELS],
}

pub struct Engine {
    framer: Framer,
    decoder: Box<HdDecoder>,
    frame: Box<HdFrame>,
    /// The standard height fold, removed from every bed of a frame whose extension applies.
    fold: FoldPlan,
    verdict: Verdict,
    undecodable_in_a_row: u32,
    /// Frames still to decode without output (see PRIMING_FRAMES).
    priming: u32,
    info: Option<StreamInfo>,

    /// Input frames (samples per channel) before the next frame since reset.
    input_frames: u64,
    /// Samples of the stream's frames, for frames that produced none.
    nominal_frames: u32,
    sample_position: u64,
    pending_flags: u32,

    roles: Option<Roles>,
    presentation: Option<XPresentation>,
    layout_serial: u32,

    planes: Box<[[f32; MAX_BLOCK_FRAMES]]>,
    plane_ptrs: [*const f32; MAX_CHANNELS],
    speakers: [u8; MAX_CHANNELS],

    /// Element values in force (CHANGED cleared), the update's copy, and the C views of them.
    elements: [Element; MAX_CHANNELS],
    update_elements: [Element; MAX_CHANNELS],
    update: MetadataUpdate,
    metadata: Metadata,
    /// The next block restates every element as a jump.
    restate: bool,

    stats: Stats,
    last_error: CString,
    pub poisoned: bool,
}

impl Engine {
    pub fn new() -> Box<Self> {
        let mut engine = Box::new(Self {
            framer: Framer::default(),
            decoder: Box::new(HdDecoder::new()),
            frame: Box::default(),
            fold: FoldPlan::standard_heights(STANDARD_HEIGHT_FOLD),
            verdict: Verdict::Pending,
            undecodable_in_a_row: 0,
            priming: PRIMING_FRAMES,
            info: None,
            input_frames: 0,
            nominal_frames: 0,
            sample_position: 0,
            pending_flags: BLOCK_DISCONTINUITY,
            roles: None,
            presentation: None,
            layout_serial: 0,
            // Built on the heap: as a Box::new argument the 384 KiB would pass through the stack
            // of whatever thread creates the decoder (512 KiB on an iOS secondary thread).
            planes: vec![[0.0; MAX_BLOCK_FRAMES]; MAX_CHANNELS].into_boxed_slice(),
            plane_ptrs: [std::ptr::null(); MAX_CHANNELS],
            speakers: [SPEAKER_UNKNOWN; MAX_CHANNELS],
            elements: [Element::EMPTY; MAX_CHANNELS],
            update_elements: [Element::EMPTY; MAX_CHANNELS],
            update: MetadataUpdate {
                frame_offset: 0,
                ramp_frames: 0,
                elements: std::ptr::null(),
            },
            metadata: Metadata {
                element_count: 0,
                bed_count: 0,
                object_count: 0,
                fixed_count: 0,
                update_count: 0,
                reserved: 0,
                updates: std::ptr::null(),
                elements: std::ptr::null(),
            },
            restate: true,
            stats: Stats::default(),
            last_error: CString::default(),
            poisoned: false,
        });
        for (ptr, plane) in engine.plane_ptrs.iter_mut().zip(engine.planes.iter()) {
            *ptr = plane.as_ptr();
        }
        engine
    }

    /// Seek: drop input and decode state. The verdict, stream description and layout identity
    /// survive, so a seek within one stream keeps its layout and is never refused anew.
    pub fn reset(&mut self) {
        self.framer.reset();
        self.decoder.reset();
        self.undecodable_in_a_row = 0;
        self.priming = PRIMING_FRAMES;
        self.input_frames = 0;
        self.sample_position = 0;
        self.pending_flags = BLOCK_DISCONTINUITY;
        self.restate = true;
        self.stats = Stats::default();
        if !matches!(self.verdict, Verdict::Refused(_)) {
            self.last_error = CString::default();
        }
        self.poisoned = false;
    }

    fn set_error(&mut self, message: String) {
        self.last_error = CString::new(message.replace('\0', " ")).unwrap_or_default();
    }

    pub fn last_error(&self) -> &CString {
        &self.last_error
    }

    fn refuse(&mut self, status: Status, message: String) -> Status {
        self.verdict = Verdict::Refused(status);
        self.set_error(message);
        status
    }

    fn refusal(&self) -> Option<Status> {
        match self.verdict {
            Verdict::Refused(status) => Some(status),
            _ => None,
        }
    }

    pub fn push(&mut self, data: &[u8], pts: i64) -> Status {
        if let Some(status) = self.refusal() {
            return status;
        }
        if data.is_empty() {
            return OK;
        }
        if self.framer.pending() + data.len() > MAX_PENDING_INPUT {
            return ERR_BUFFER_FULL;
        }
        self.stats.bytes_pushed += data.len() as u64;
        self.framer.push(data, pts);
        OK
    }

    /// A frame that produced no output: its samples still count on the input timeline.
    fn skip_frame(&mut self, info: &AuInfo) {
        self.advance_input(info);
        self.input_frames += u64::from(self.frame_samples_guess(info));
        self.break_continuity();
    }

    /// Output no longer continues the previous block: the next block is flagged and restates its
    /// metadata, and the decoder primes again, since its history no longer matches the stream.
    fn break_continuity(&mut self) {
        self.pending_flags |= BLOCK_DISCONTINUITY;
        self.restate = true;
        self.priming = PRIMING_FRAMES;
    }

    fn frame_samples_guess(&self, info: &AuInfo) -> u32 {
        if self.nominal_frames > 0 {
            self.nominal_frames
        } else {
            info.core_samples
        }
    }

    /// Accounts for bytes skipped before a frame: a sync error and the frames estimated in them.
    fn advance_input(&mut self, info: &AuInfo) {
        if info.skipped_before > 0 && info.ordinal > 0 {
            self.stats.sync_errors += 1;
            self.break_continuity();
            self.set_error("bitstream damage: bytes outside any DTS-HD frame skipped".into());
        }
        self.input_frames += info.estimated_before * u64::from(self.frame_samples_guess(info));
    }

    pub fn pull(&mut self, out: &mut Block) -> Status {
        loop {
            if let Some(status) = self.refusal() {
                return status;
            }
            let (info, result) = match self.framer.next() {
                Next::NeedData => {
                    if self.verdict == Verdict::Pending
                        && self.framer.unframed_since_create > MAX_UNFRAMED_BEFORE_VERDICT
                    {
                        return self.refuse(ERR_NOT_DTSX, "no DTS frame in the first MiB".into());
                    }
                    return NEED_MORE_DATA;
                }
                Next::CoreOnly(info) => (info, None),
                Next::Au(info, core, exss) => {
                    let decoder = &mut self.decoder;
                    let frame = &mut self.frame;
                    let result =
                        catch_unwind(AssertUnwindSafe(|| decoder.decode_into(core, exss, frame)))
                            .unwrap_or_else(|p| {
                                Err(HdError::Xll(format!(
                                    "decoder panicked: {}",
                                    panic_message(&p)
                                )))
                            });
                    (info, Some(result))
                }
            };
            let (pts, pts_au_index) = self.framer.attribute(info.offset);
            match result {
                None => {
                    if self.verdict == Verdict::Pending {
                        return self.refuse(
                            ERR_NOT_DTSX,
                            "DTS core without an extension substream: not DTS-HD".into(),
                        );
                    }
                    self.stats.decode_errors += 1;
                    self.set_error("DTS frame without an extension substream skipped".into());
                    self.skip_frame(&info);
                }
                Some(Err(HdError::Pending)) => {
                    // The lossless decoder buffers a frame's data across substreams (peak bit
                    // rate smoothing) and has no output for this one; the core's synthesis did
                    // not run for it either.
                    self.stats.access_units_skipped += 1;
                    self.skip_frame(&info);
                }
                Some(Err(error)) => {
                    // A decoder failure can leave its state anywhere; start the next frame clean.
                    self.decoder.reset();
                    self.stats.decode_errors += 1;
                    self.set_error(format!("DTS-HD decoding: {error:?}"));
                    self.skip_frame(&info);
                    if self.verdict == Verdict::Pending {
                        self.undecodable_in_a_row += 1;
                        if self.undecodable_in_a_row >= MAX_UNDECODABLE_BEFORE_VERDICT {
                            return self.refuse(
                                ERR_UNSUPPORTED,
                                format!("the first DTS-HD frames do not decode ({error:?})"),
                            );
                        }
                    }
                }
                Some(Ok(())) => {
                    if self.verdict == Verdict::Pending {
                        match judge(&self.frame) {
                            Judgement::Accept => self.verdict = Verdict::Accepted,
                            Judgement::Refuse(status, message) => {
                                return self.refuse(status, message);
                            }
                            Judgement::Retry(message) => {
                                self.stats.decode_errors += 1;
                                self.set_error(message.clone());
                                self.skip_frame(&info);
                                self.undecodable_in_a_row += 1;
                                if self.undecodable_in_a_row >= MAX_UNDECODABLE_BEFORE_VERDICT {
                                    return self.refuse(ERR_UNSUPPORTED, message);
                                }
                                continue;
                            }
                        }
                    }
                    self.undecodable_in_a_row = 0;
                    self.advance_input(&info);
                    if self.priming > 0 {
                        self.prime();
                        continue;
                    }
                    match self.emit(&info, pts, pts_au_index, out) {
                        Ok(()) => return OK,
                        Err(message) => {
                            self.stats.decode_errors += 1;
                            self.set_error(message);
                            self.input_frames += u64::from(self.frame_samples_guess(&info));
                            self.break_continuity();
                        }
                    }
                }
            }
        }
    }

    /// A decoded frame that only rebuilds decoder history: no block, but its samples count on the
    /// input timeline.
    fn prime(&mut self) {
        self.priming -= 1;
        let frames = self.frame.bed_sample_count();
        self.nominal_frames = frames as u32;
        self.input_frames += frames as u64;
        self.stats.access_units_decoded += 1;
        self.stats.access_units_skipped += 1;
    }

    /// Whether the frame's heights can be removed from its bed, or why not.
    fn heights_apply(&self) -> Result<(), String> {
        let frame = &self.frame;
        if let Some(error) = frame.x_decode_error {
            return Err(format!("DTS:X extension decoding: {error}"));
        }
        if !is_7_1(frame) {
            return Err("DTS:X frame over a bed other than 7.1".into());
        }
        match XPresentation::detect(frame) {
            Some(XPresentation::Height) => Ok(()),
            Some(other) => Err(format!(
                "DTS:X frame in the {} presentation, which is not rendered",
                other.layout_label()
            )),
            None => Err("frame without a DTS:X extension".into()),
        }
    }

    fn emit(
        &mut self,
        info: &AuInfo,
        pts: i64,
        pts_au_index: u32,
        out: &mut Block,
    ) -> Result<(), String> {
        let frames = self.frame.bed_sample_count();
        if frames == 0 || frames > MAX_BLOCK_FRAMES {
            return Err(format!("DTS-HD frame of {frames} samples"));
        }
        // The verdict accepted the standard presentation only; a frame without a usable
        // extension keeps its layout, with silent heights.
        let presentation = XPresentation::Height;

        let mut bed: [(usize, u8); MAX_CHANNELS] = [(0, SPEAKER_UNKNOWN); MAX_CHANNELS];
        let mut bed_count = 0;
        for (index, channel) in self.frame.samples.iter().enumerate() {
            let Some(pcm) = channel else { continue };
            if pcm.len() != frames {
                return Err("DTS-HD bed channels differ in length".into());
            }
            if bed_count + presentation.feed_count() >= MAX_CHANNELS {
                return Err("DTS-HD frame with more channels than this API carries".into());
            }
            bed[bed_count] = (index, bed_speaker(index));
            bed_count += 1;
        }
        let fixed = presentation.fixed_channels();
        let channels = bed_count + fixed.len();

        let applies = match self.heights_apply() {
            Ok(()) => true,
            Err(message) => {
                self.set_error(message);
                false
            }
        };

        // Planes: the bed with the heights removed (or as authored), then the heights.
        for (k, &(index, _)) in bed[..bed_count].iter().enumerate() {
            let pcm = self.frame.samples[index].as_deref().unwrap_or_default();
            let plane = &mut self.planes[k][..frames];
            if applies {
                self.fold
                    .clean_channel(index, pcm, &self.frame.x_samples, plane);
            } else {
                plane.copy_from_slice(pcm);
            }
        }
        for feed in 0..fixed.len() {
            let plane = &mut self.planes[bed_count + feed][..frames];
            match self.frame.x_samples.get(feed) {
                Some(pcm) if applies => plane.copy_from_slice(&pcm[..frames]),
                _ => plane.fill(0.0),
            }
        }

        // Roles and elements.
        let mut speakers = [SPEAKER_UNKNOWN; MAX_CHANNELS];
        let mut elements = [Element::EMPTY; MAX_CHANNELS];
        for (k, &(_, code)) in bed[..bed_count].iter().enumerate() {
            speakers[k] = code;
            elements[k] = fixed_element(code);
        }
        for (i, &channel) in fixed.iter().enumerate() {
            let c = bed_count + i;
            let code = fixed_speaker(channel);
            speakers[c] = code;
            elements[c] = fixed_element(code);
        }
        self.speakers = speakers;

        let roles = Roles {
            sample_rate: self.frame.sample_rate,
            presentation: presentation_code(presentation),
            channels: channels as u32,
            speakers,
        };
        let mut flags = std::mem::take(&mut self.pending_flags) | BLOCK_HAS_METADATA;
        if self.roles != Some(roles) {
            flags |= BLOCK_LAYOUT_CHANGED;
            self.layout_serial = self.layout_serial.wrapping_add(1);
            self.roles = Some(roles);
            self.restate = true;
        }
        self.presentation = Some(presentation);
        if !applies {
            flags |= BLOCK_BED_AS_AUTHORED;
            self.stats.bed_as_authored += 1;
        }

        // One update when anything changed: a jump restating everything after a discontinuity
        // or layout change, otherwise a ramp over the frame for what moved.
        let restate = std::mem::take(&mut self.restate);
        let mut changed = false;
        for ((new, old), update) in elements
            .iter()
            .zip(&self.elements)
            .zip(&mut self.update_elements)
            .take(channels)
        {
            *update = *new;
            if restate || !new.same_values(old) {
                update.flags |= EL_CHANGED;
                changed = true;
            }
        }
        self.elements = elements;
        self.update = MetadataUpdate {
            frame_offset: 0,
            ramp_frames: if restate { 0 } else { frames as u32 },
            elements: self.update_elements.as_ptr(),
        };
        self.metadata = Metadata {
            element_count: channels as u32,
            bed_count: bed_count as u32,
            object_count: 0,
            fixed_count: fixed.len() as u32,
            update_count: changed as u32,
            reserved: 0,
            updates: &self.update,
            elements: self.elements.as_ptr(),
        };
        if changed {
            flags |= BLOCK_METADATA_UPDATED;
            self.stats.metadata_updates += 1;
        }
        if self.framer.estimated {
            flags |= BLOCK_INPUT_OFFSET_ESTIMATED;
        }

        self.nominal_frames = frames as u32;
        self.info = Some(StreamInfo {
            sample_rate: self.frame.sample_rate,
            access_unit_frames: self.nominal_frames,
            presentation: presentation_code(presentation),
            experimental: 0,
            alternate_marker: self.frame.x_imax as u32,
            bed_mask: self.frame.output_mask,
            channel_count: channels as u32,
            layout_serial: self.layout_serial,
        });
        *out = Block {
            sample_rate: self.frame.sample_rate,
            frame_count: frames as u32,
            channel_count: channels as u32,
            presentation: presentation_code(presentation),
            flags,
            access_unit_frames: self.nominal_frames,
            pts,
            pts_au_index,
            layout_serial: self.layout_serial,
            sample_position: self.sample_position,
            input_access_unit: info.ordinal,
            input_frame_offset: self.input_frames,
            channels: self.plane_ptrs.as_ptr(),
            speakers: self.speakers.as_ptr(),
            metadata: &self.metadata,
        };
        self.input_frames += frames as u64;
        self.sample_position += frames as u64;
        self.stats.access_units_decoded += 1;
        self.stats.blocks_output += 1;
        self.stats.frames_output += frames as u64;
        Ok(())
    }

    pub fn stream_info(&self) -> Option<StreamInfo> {
        self.info
    }

    pub fn stats(&self) -> Stats {
        let mut stats = self.stats;
        stats.bytes_pending = self.framer.pending() as u64;
        stats
    }
}

enum Judgement {
    Accept,
    Refuse(Status, String),
    /// The frame does not show yet; try the next one.
    Retry(String),
}

/// The verdict on the first frame decoded since create(): only stream properties decide it, so
/// it holds for every later frame.
fn judge(frame: &HdFrame) -> Judgement {
    let marked = frame.x_present || frame.x_imax;
    if !frame.lossless {
        return if marked {
            Judgement::Refuse(ERR_UNSUPPORTED, "DTS:X on a lossy DTS-HD carrier".into())
        } else {
            Judgement::Refuse(
                ERR_NOT_DTSX,
                "DTS-HD without a lossless (Master Audio) asset".into(),
            )
        };
    }
    if !marked {
        return Judgement::Refuse(
            ERR_NOT_DTSX,
            "lossless DTS-HD Master Audio without a DTS:X extension".into(),
        );
    }
    match XPresentation::detect(frame) {
        Some(XPresentation::Height) => {}
        Some(other) => {
            return Judgement::Refuse(
                ERR_UNSUPPORTED,
                format!(
                    "DTS:X in the {} presentation, with objects: only the standard 7.1.4 \
                     presentation is rendered",
                    other.layout_label()
                ),
            );
        }
        None => {
            return match frame.x_decode_error {
                Some(error) => Judgement::Retry(format!("DTS:X extension decoding: {error}")),
                None => Judgement::Refuse(
                    ERR_UNSUPPORTED,
                    format!(
                        "DTS:X extension with {} feeds matches no known presentation",
                        frame.x_samples.len()
                    ),
                ),
            };
        }
    }
    // The standard presentation is authored over the 7.1 bed, and its heights are folded into
    // L, R, Lsr and Rsr; any other bed is not one it describes.
    if !is_7_1(frame) {
        return Judgement::Refuse(
            ERR_UNSUPPORTED,
            format!(
                "DTS:X over a bed other than 7.1 (mask {:#06x})",
                frame.output_mask
            ),
        );
    }
    Judgement::Accept
}

/// Whether the frame's bed is exactly the 7.1 the standard presentation is authored over.
fn is_7_1(frame: &HdFrame) -> bool {
    frame
        .samples
        .iter()
        .enumerate()
        .filter(|(_, channel)| channel.is_some())
        .map(|(index, _)| index)
        .eq(BED_7_1)
}

fn presentation_code(presentation: XPresentation) -> u32 {
    match presentation {
        XPresentation::Height => PRESENTATION_HEIGHT,
        XPresentation::ObjectD0 => PRESENTATION_OBJECT_D0,
        XPresentation::ObjectsD1 => PRESENTATION_OBJECTS_D1,
        XPresentation::ObjectsD0 => PRESENTATION_OBJECTS_D0,
        XPresentation::ObjectsD3 => PRESENTATION_OBJECTS_D3,
        XPresentation::ObjectsD4 => PRESENTATION_OBJECTS_D4,
        XPresentation::ObjectOnly => PRESENTATION_OBJECT_ONLY,
    }
}

pub fn presentation_name(code: u32) -> &'static std::ffi::CStr {
    match code {
        PRESENTATION_HEIGHT => c"7.1.4",
        PRESENTATION_OBJECT_D0 => c"7.1.4+1",
        PRESENTATION_OBJECTS_D1 => c"7.1.4+2",
        PRESENTATION_OBJECTS_D0 => c"7.1.4+3",
        PRESENTATION_OBJECTS_D3 => c"7.1.4+4",
        PRESENTATION_OBJECTS_D4 => c"7.1.4+5",
        PRESENTATION_OBJECT_ONLY => c"5.1+1",
        _ => c"?",
    }
}

/// Speaker code of a DCA bed speaker index (ETSI TS 102 114 speaker order).
fn bed_speaker(index: usize) -> u8 {
    match index {
        0 => SPEAKER_C,
        1 => SPEAKER_L,
        2 => SPEAKER_R,
        3 | 9 => SPEAKER_LS,
        4 | 10 => SPEAKER_RS,
        5 => SPEAKER_LFE,
        6 => SPEAKER_CB,
        7 => SPEAKER_LB,
        8 => SPEAKER_RB,
        11 => SPEAKER_LSC,
        12 => SPEAKER_RSC,
        13 => SPEAKER_TFL,
        14 => SPEAKER_TFC,
        15 => SPEAKER_TFR,
        16 => SPEAKER_LFE2,
        17 => SPEAKER_LW,
        18 => SPEAKER_RW,
        19 => SPEAKER_TC,
        20 => SPEAKER_TSL,
        21 => SPEAKER_TSR,
        23 => SPEAKER_TBL,
        24 => SPEAKER_TBR,
        _ => SPEAKER_UNKNOWN,
    }
}

fn fixed_speaker(channel: dca::SpatialChannel) -> u8 {
    use dca::SpatialChannel::*;
    match channel {
        TopFrontLeft => SPEAKER_TFL,
        TopFrontRight => SPEAKER_TFR,
        TopFrontCenter => SPEAKER_TFC,
        TopSideLeft => SPEAKER_TSL,
        TopSideRight => SPEAKER_TSR,
        TopBackLeft => SPEAKER_TBL,
        TopBackRight => SPEAKER_TBR,
        WideLeft => SPEAKER_LW,
        WideRight => SPEAKER_RW,
    }
}

fn fixed_element(code: u8) -> Element {
    Element {
        kind: if code == SPEAKER_LFE || code == SPEAKER_LFE2 {
            ELEMENT_LFE
        } else {
            ELEMENT_BED
        },
        speaker: code,
        flags: EL_ACTIVE,
        position: speaker_position(code),
        gain: 1.0,
        ..Element::EMPTY
    }
}

pub fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lossless DTS:X frame over the 7.1 bed `indices`, with `feeds` extension waveforms.
    fn frame(indices: &[usize], feeds: usize, imax: bool) -> HdFrame {
        let mut samples = vec![None; 9];
        for &index in indices {
            samples[index] = Some(vec![0.0; 512]);
        }
        HdFrame {
            samples,
            lossless: true,
            x_present: !imax,
            x_imax: imax,
            x_samples: vec![vec![0.0; 512]; feeds],
            ..HdFrame::default()
        }
    }

    #[test]
    fn the_standard_presentation_is_accepted() {
        assert!(matches!(
            judge(&frame(&BED_7_1, 4, false)),
            Judgement::Accept
        ));
    }

    #[test]
    fn every_presentation_with_objects_is_refused() {
        for (feeds, bed) in [
            (5, &BED_7_1[..]),
            (6, &BED_7_1),
            (7, &BED_7_1),
            (8, &BED_7_1),
            (9, &BED_7_1),
            (1, &[0, 1, 2, 3, 4, 5][..]),
        ] {
            let refused = matches!(
                judge(&frame(bed, feeds, true)),
                Judgement::Refuse(ERR_UNSUPPORTED, _)
            );
            assert!(refused, "{feeds} feeds");
        }
    }

    #[test]
    fn heights_over_a_bed_other_than_7_1_are_refused() {
        let mut with_back_centre = BED_7_1.to_vec();
        with_back_centre.push(6);
        assert!(matches!(
            judge(&frame(&with_back_centre, 4, false)),
            Judgement::Refuse(ERR_UNSUPPORTED, _)
        ));
        assert!(matches!(
            judge(&frame(&[0, 1, 2, 3, 4, 5], 4, false)),
            Judgement::Refuse(ERR_UNSUPPORTED, _)
        ));
    }

    /// DCA bed speakers the four standard heights are folded into, in feed order (L, R, Lsr, Rsr).
    const STANDARD_FOLD_SPEAKERS: [usize; 4] = [1, 2, 7, 8];

    #[test]
    fn the_standard_fold_is_minus_3_db_into_the_speaker_under_each_height() {
        let plan = FoldPlan::standard_heights(STANDARD_HEIGHT_FOLD);
        for speaker in 0..9 {
            for (feed, &under) in STANDARD_FOLD_SPEAKERS.iter().enumerate() {
                let expected = if under == speaker { 0.707_092_3 } else { 0.0 };
                assert!(
                    (plan.gain(speaker, feed) - expected).abs() < 1e-6,
                    "{speaker} {feed}"
                );
            }
        }
    }
}
