//! `#[repr(C)]` mirrors of the types in `include/dtsx.h`. The header is the contract; the size
//! and offset assertions at the bottom keep the two from drifting apart.

use std::mem::{offset_of, size_of};

pub const API_VERSION: u32 = 1;
pub const MAX_CHANNELS: usize = 24;
pub const MAX_BLOCK_FRAMES: usize = 4096;
pub const NO_PTS: i64 = i64::MIN;

pub type Status = i32;
pub const OK: Status = 0;
pub const NEED_MORE_DATA: Status = 1;
pub const ERR_NULL: Status = -1;
pub const ERR_INVALID_ARGUMENT: Status = -2;
pub const ERR_NOT_READY: Status = -3;
pub const ERR_BUFFER_FULL: Status = -4;
pub const ERR_PANIC: Status = -5;
pub const ERR_NOT_DTSX: Status = -6;
pub const ERR_UNSUPPORTED: Status = -7;

pub const PRESENTATION_HEIGHT: u32 = 0;
pub const PRESENTATION_OBJECT_D0: u32 = 1;
pub const PRESENTATION_OBJECTS_D1: u32 = 2;
pub const PRESENTATION_OBJECTS_D0: u32 = 3;
pub const PRESENTATION_OBJECTS_D3: u32 = 4;
pub const PRESENTATION_OBJECTS_D4: u32 = 5;
pub const PRESENTATION_OBJECT_ONLY: u32 = 6;

pub const SPEAKER_L: u8 = 0;
pub const SPEAKER_R: u8 = 1;
pub const SPEAKER_C: u8 = 2;
pub const SPEAKER_LFE: u8 = 3;
pub const SPEAKER_LS: u8 = 4;
pub const SPEAKER_RS: u8 = 5;
pub const SPEAKER_TFL: u8 = 6;
pub const SPEAKER_TFR: u8 = 7;
pub const SPEAKER_TSL: u8 = 8;
pub const SPEAKER_TSR: u8 = 9;
pub const SPEAKER_TBL: u8 = 10;
pub const SPEAKER_TBR: u8 = 11;
pub const SPEAKER_LSC: u8 = 12;
pub const SPEAKER_RSC: u8 = 13;
pub const SPEAKER_LB: u8 = 14;
pub const SPEAKER_RB: u8 = 15;
pub const SPEAKER_CB: u8 = 16;
pub const SPEAKER_TC: u8 = 17;
pub const SPEAKER_LW: u8 = 20;
pub const SPEAKER_RW: u8 = 21;
pub const SPEAKER_TFC: u8 = 22;
pub const SPEAKER_LFE2: u8 = 23;
pub const SPEAKER_OBJECT: u8 = 254;
pub const SPEAKER_UNKNOWN: u8 = 255;

pub const ELEMENT_BED: u8 = 0;
pub const ELEMENT_LFE: u8 = 1;
pub const ELEMENT_OBJECT: u8 = 2;

pub const EL_ACTIVE: u32 = 1 << 0;
pub const EL_CHANGED: u32 = 1 << 1;

pub const BLOCK_DISCONTINUITY: u32 = 1 << 0;
pub const BLOCK_LAYOUT_CHANGED: u32 = 1 << 1;
pub const BLOCK_HAS_METADATA: u32 = 1 << 2;
pub const BLOCK_METADATA_UPDATED: u32 = 1 << 3;
pub const BLOCK_INPUT_OFFSET_ESTIMATED: u32 = 1 << 4;
pub const BLOCK_BED_AS_AUTHORED: u32 = 1 << 5;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Element {
    pub kind: u8,
    pub speaker: u8,
    pub reserved: [u8; 2],
    pub flags: u32,
    pub position: [f32; 3],
    pub gain: f32,
    pub azimuth: f32,
    pub elevation: f32,
    pub distance: f32,
}

impl Element {
    pub const EMPTY: Self = Self {
        kind: ELEMENT_OBJECT,
        speaker: SPEAKER_OBJECT,
        reserved: [0; 2],
        flags: 0,
        position: [0.0; 3],
        gain: 0.0,
        azimuth: 0.0,
        elevation: 0.0,
        distance: 0.0,
    };

    /// Equal in everything a renderer acts on, ignoring the CHANGED marker itself.
    pub fn same_values(&self, other: &Self) -> bool {
        let mut a = *self;
        let mut b = *other;
        a.flags &= !EL_CHANGED;
        b.flags &= !EL_CHANGED;
        a == b
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MetadataUpdate {
    pub frame_offset: u32,
    pub ramp_frames: u32,
    pub elements: *const Element,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    pub element_count: u32,
    pub bed_count: u32,
    pub object_count: u32,
    pub fixed_count: u32,
    pub update_count: u32,
    pub reserved: u32,
    pub updates: *const MetadataUpdate,
    pub elements: *const Element,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub sample_rate: u32,
    pub frame_count: u32,
    pub channel_count: u32,
    pub presentation: u32,
    pub flags: u32,
    pub access_unit_frames: u32,
    pub pts: i64,
    pub pts_au_index: u32,
    pub layout_serial: u32,
    pub sample_position: u64,
    pub input_access_unit: u64,
    pub input_frame_offset: u64,
    pub channels: *const *const f32,
    pub speakers: *const u8,
    pub metadata: *const Metadata,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub access_unit_frames: u32,
    pub presentation: u32,
    pub experimental: u32,
    pub alternate_marker: u32,
    pub bed_mask: u32,
    pub channel_count: u32,
    pub layout_serial: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub bytes_pushed: u64,
    pub bytes_pending: u64,
    pub access_units_decoded: u64,
    pub blocks_output: u64,
    pub frames_output: u64,
    pub access_units_skipped: u64,
    pub decode_errors: u64,
    pub sync_errors: u64,
    pub bed_as_authored: u64,
    pub metadata_updates: u64,
}

// Layout checks against the header (arm64 and x86_64 Apple ABIs, 8-byte pointers).
const _: () = {
    assert!(size_of::<Element>() == 36);
    assert!(offset_of!(Element, flags) == 4);
    assert!(offset_of!(Element, position) == 8);
    assert!(offset_of!(Element, gain) == 20);
    assert!(offset_of!(Element, azimuth) == 24);
    assert!(offset_of!(Element, distance) == 32);
    assert!(size_of::<MetadataUpdate>() == 16);
    assert!(size_of::<Metadata>() == 40);
    assert!(offset_of!(Metadata, updates) == 24);
    assert!(offset_of!(Metadata, elements) == 32);
    assert!(size_of::<Block>() == 88);
    assert!(offset_of!(Block, pts) == 24);
    assert!(offset_of!(Block, layout_serial) == 36);
    assert!(offset_of!(Block, input_frame_offset) == 56);
    assert!(offset_of!(Block, channels) == 64);
    assert!(offset_of!(Block, metadata) == 80);
    assert!(size_of::<StreamInfo>() == 32);
    assert!(size_of::<Stats>() == 10 * 8);
};
