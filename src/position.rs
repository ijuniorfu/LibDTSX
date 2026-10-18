//! Nominal room positions and names of the speaker codes (see COORDINATES in `include/dtsx.h`).

use crate::types::*;

/// Nominal room position of a speaker code, for the elements of bed and fixed channels.
pub fn speaker_position(code: u8) -> [f32; 3] {
    match code {
        SPEAKER_L => [0.0, 0.0, 0.0],
        SPEAKER_R => [1.0, 0.0, 0.0],
        SPEAKER_C | SPEAKER_LFE | SPEAKER_LFE2 => [0.5, 0.0, 0.0],
        SPEAKER_LS => [0.0, 0.5, 0.0],
        SPEAKER_RS => [1.0, 0.5, 0.0],
        SPEAKER_LB => [0.0, 1.0, 0.0],
        SPEAKER_RB => [1.0, 1.0, 0.0],
        SPEAKER_CB => [0.5, 1.0, 0.0],
        SPEAKER_LSC => [0.25, 0.0, 0.0],
        SPEAKER_RSC => [0.75, 0.0, 0.0],
        SPEAKER_LW => [0.0, 0.25, 0.0],
        SPEAKER_RW => [1.0, 0.25, 0.0],
        SPEAKER_TFL => [0.0, 0.0, 1.0],
        SPEAKER_TFR => [1.0, 0.0, 1.0],
        SPEAKER_TFC => [0.5, 0.0, 1.0],
        SPEAKER_TSL => [0.0, 0.5, 1.0],
        SPEAKER_TSR => [1.0, 0.5, 1.0],
        SPEAKER_TBL => [0.0, 1.0, 1.0],
        SPEAKER_TBR => [1.0, 1.0, 1.0],
        SPEAKER_TC => [0.5, 0.5, 1.0],
        _ => [0.5, 0.5, 0.0],
    }
}

pub fn speaker_name(code: u8) -> &'static std::ffi::CStr {
    const NAMES: [&std::ffi::CStr; 24] = [
        c"L", c"R", c"C", c"LFE", c"Ls", c"Rs", c"Tfl", c"Tfr", c"Tsl", c"Tsr", c"Tbl", c"Tbr",
        c"Lsc", c"Rsc", c"Lb", c"Rb", c"Cb", c"Tc", c"Lsd", c"Rsd", c"Lw", c"Rw", c"Tfc", c"LFE2",
    ];
    match code {
        0..=23 => NAMES[code as usize],
        SPEAKER_OBJECT => c"Obj",
        _ => c"?",
    }
}
