//! The decoder behind a handle: framing, decoding, the DTS:X verdict, fold removal, channel
//! roles, metadata and recovery. Everything here is safe Rust; `lib.rs` wraps it for C.
//!
//! The spatial reconstruction follows SpatialAudioBridge's DTS:X decoder (Apache-2.0,
//! kristofferR/SpatialAudioBridge): presentation detection and metadata come from `dca`,
//! every stated or recomputable fold is subtracted from the bed before the bed and the
//! extension feeds are rendered side by side, and anything unresolved is never rendered twice.

use crate::framer::{AuInfo, Framer, Next};
use crate::position::{room_position, speaker_position};
use crate::types::*;
use dca::{
    FoldPlan, FoldRenderer, HdDecoder, HdError, HdFrame, SourceRole, XMetadata, XPresentation,
};
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

/// One frame's spatial reconstruction, when its extension can be applied.
struct Spatial {
    metadata: XMetadata,
    plan: FoldPlan,
}

pub struct Engine {
    framer: Framer,
    decoder: Box<HdDecoder>,
    frame: Box<HdFrame>,
    folds: FoldRenderer,
    /// The presentation `folds` last rendered, whose gains it ramps from.
    fold_presentation: Option<XPresentation>,
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
            folds: FoldRenderer::new(),
            fold_presentation: None,
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
        self.folds.reset();
        self.fold_presentation = None;
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
        self.fold_presentation = None;
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
    /// input timeline and the fold renderer advances through it, so the first block's folds ramp
    /// from this frame's as they would in a continuous decode.
    fn prime(&mut self) {
        self.priming -= 1;
        let frames = self.frame.bed_sample_count();
        let _ = self.spatial(XPresentation::detect(&self.frame), frames);
        self.nominal_frames = frames as u32;
        self.input_frames += frames as u64;
        self.stats.access_units_decoded += 1;
        self.stats.access_units_skipped += 1;
    }

    /// The frame's spatial reconstruction, or why it cannot be applied.
    fn spatial(
        &mut self,
        presentation: Option<XPresentation>,
        frames: usize,
    ) -> Result<Spatial, String> {
        let frame = &self.frame;
        if let Some(error) = frame.x_decode_error {
            return Err(format!("DTS:X extension decoding: {error}"));
        }
        let presentation = presentation.ok_or("frame without a DTS:X extension")?;
        let metadata = XMetadata::parse(&frame.x_payload, presentation.feed_count())
            .map_err(|e| format!("DTS:X metadata: {e:?}"))?;
        if metadata.source_count() != presentation.feed_count() {
            return Err("DTS:X metadata does not match its feed count".into());
        }
        for feed in 0..presentation.feed_count() {
            let source = metadata
                .source(feed)
                .ok_or("DTS:X metadata misses a source")?;
            let matches = match source.role {
                SourceRole::Object { .. } => presentation.object_feeds().contains(&feed),
                SourceRole::Height(channel) => {
                    presentation.fixed_feeds().contains(&feed)
                        && presentation.fixed_channels()[feed - presentation.fixed_feeds().start]
                            == channel
                }
            };
            if !matches {
                return Err("DTS:X source role does not match its presentation".into());
            }
        }
        // The fold renderer ramps from the previous frame's gains, which only mean anything for
        // the same presentation.
        if self.fold_presentation != Some(presentation) {
            self.folds.reset();
            self.fold_presentation = Some(presentation);
        }
        let mut plan = FoldPlan::from_metadata(&metadata);
        self.folds.apply(&mut plan, &metadata, frames);
        if plan.has_unknown() {
            return Err("DTS:X bed fold not stated; the frame plays its bed as authored".into());
        }
        Ok(Spatial { metadata, plan })
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
        let detected = XPresentation::detect(&self.frame);
        // A frame without a usable extension keeps the stream's layout, with silent feeds.
        let presentation = detected
            .or(self.presentation)
            .ok_or("no DTS:X presentation")?;

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
        let objects = presentation.object_feeds().len();
        let fixed = presentation.fixed_channels();
        let channels = bed_count + objects + fixed.len();

        let spatial = if detected == Some(presentation) {
            self.spatial(detected, frames)
        } else {
            Err("frame without a DTS:X extension".into())
        };
        let spatial = match spatial {
            Ok(spatial) => Some(spatial),
            Err(message) => {
                self.set_error(message);
                None
            }
        };

        // Planes: the bed with every fold removed (or as authored), then the feeds.
        for (k, &(index, _)) in bed[..bed_count].iter().enumerate() {
            let pcm = self.frame.samples[index].as_deref().unwrap_or_default();
            let plane = &mut self.planes[k][..frames];
            match &spatial {
                Some(s) => s
                    .plan
                    .clean_channel(index, pcm, &self.frame.x_samples, plane),
                None => plane.copy_from_slice(pcm),
            }
        }
        for feed in 0..objects + fixed.len() {
            let plane = &mut self.planes[bed_count + feed][..frames];
            match (&spatial, self.frame.x_samples.get(feed)) {
                (Some(_), Some(pcm)) => plane.copy_from_slice(&pcm[..frames]),
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
        for feed in 0..objects {
            let c = bed_count + feed;
            speakers[c] = SPEAKER_OBJECT;
            elements[c] = match &spatial {
                Some(s) => object_element(s.metadata.source(feed).map(|s| s.role)),
                // Hold the last known position; the feed is silent anyway.
                None if self.roles.is_some_and(|r| r.speakers[c] == SPEAKER_OBJECT) => {
                    let mut held = self.elements[c];
                    held.flags &= !EL_CHANGED;
                    held
                }
                None => object_element(None),
            };
        }
        for (i, &channel) in fixed.iter().enumerate() {
            let c = bed_count + objects + i;
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
        if spatial.is_none() {
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
            object_count: objects as u32,
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
            experimental: presentation.is_experimental() as u32,
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
    if XPresentation::detect(frame).is_some() {
        return Judgement::Accept;
    }
    match frame.x_decode_error {
        Some(error) => Judgement::Retry(format!("DTS:X extension decoding: {error}")),
        None => Judgement::Refuse(
            ERR_UNSUPPORTED,
            format!(
                "DTS:X extension with {} feeds matches no known presentation",
                frame.x_samples.len()
            ),
        ),
    }
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

fn object_element(role: Option<SourceRole>) -> Element {
    let (azimuth, elevation, distance) = match role {
        Some(SourceRole::Object { position, .. }) => (
            position.azimuth_degrees() as f32,
            position.elevation_degrees() as f32,
            position.distance() as f32,
        ),
        // No position known yet: the front centre at the reference radius.
        _ => (0.0, 0.0, 1.0),
    };
    Element {
        kind: ELEMENT_OBJECT,
        speaker: SPEAKER_OBJECT,
        flags: EL_ACTIVE,
        position: room_position(azimuth, elevation, distance),
        gain: 1.0,
        azimuth,
        elevation,
        distance,
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
