//! DTS:X physical positions in room coordinates (see COORDINATES in `include/dtsx.h`).
//!
//! DTS:X states where an object is as angles around the listener; renderers built for object
//! audio (and LibTrueHD's consumers) place sound in a room cube. The reference sphere is
//! projected onto the room's boundary so that the speakers on it keep their angles: an azimuth
//! between two speakers lands between them in the same proportion along the wall, and the
//! ceiling is reached at the 45° elevation of a height speaker. The angles are those
//! SpatialAudioBridge's DTS:X renderer pans between.

use crate::types::*;

/// Azimuth anchors along one half of the room, front centre to back centre: (degrees, x
/// distance from the centre line, y). Between anchors the position is linear in azimuth.
const ANCHORS: [(f32, f32, f32); 6] = [
    (0.0, 0.0, 0.0),   // centre
    (30.0, 0.5, 0.0),  // front left / right
    (60.0, 0.5, 0.25), // wide
    (90.0, 0.5, 0.5),  // side surround
    (145.0, 0.5, 1.0), // rear surround
    (180.0, 0.0, 1.0), // back centre
];

/// Elevation of the height ring: a position at it is on the ceiling.
const HEIGHT_RING_DEGREES: f32 = 45.0;

/// Room position `[x, y, z]` of a DTS:X polar position: `azimuth` in degrees (0 = front,
/// positive = right), `elevation` in degrees (positive = up), `distance` in reference radii.
pub fn room_position(azimuth: f32, elevation: f32, distance: f32) -> [f32; 3] {
    let azimuth = if azimuth.is_finite() { azimuth } else { 0.0 };
    // Into (-180, 180], then split into side and angle from the front.
    let wrapped = 180.0 - (180.0 - azimuth).rem_euclid(360.0);
    let angle = wrapped.abs().min(180.0);
    let right = wrapped > 0.0;

    let (mut offset, mut y) = (0.0, 1.0);
    for pair in ANCHORS.windows(2) {
        let ((a0, x0, y0), (a1, x1, y1)) = (pair[0], pair[1]);
        if angle <= a1 {
            let t = (angle - a0) / (a1 - a0);
            offset = x0 + (x1 - x0) * t;
            y = y0 + (y1 - y0) * t;
            break;
        }
    }
    let mut x = if right { 0.5 + offset } else { 0.5 - offset };

    let elevation = if elevation.is_finite() {
        elevation.clamp(0.0, 90.0)
    } else {
        0.0
    };
    let mut z = (elevation / HEIGHT_RING_DEGREES).min(1.0);
    if elevation > HEIGHT_RING_DEGREES {
        // On across the ceiling towards its centre, reached straight overhead.
        let t = (elevation - HEIGHT_RING_DEGREES) / (90.0 - HEIGHT_RING_DEGREES);
        x += (0.5 - x) * t;
        y += (0.5 - y) * t;
    }

    let distance = if distance.is_finite() {
        distance.clamp(0.0, 1.0)
    } else {
        1.0
    };
    x = 0.5 + (x - 0.5) * distance;
    y = 0.5 + (y - 0.5) * distance;
    z *= distance;
    [x, y, z]
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn speaker_angles_land_on_their_speakers() {
        for (azimuth, speaker) in [
            (0.0, SPEAKER_C),
            (-30.0, SPEAKER_L),
            (30.0, SPEAKER_R),
            (-60.0, SPEAKER_LW),
            (60.0, SPEAKER_RW),
            (-90.0, SPEAKER_LS),
            (90.0, SPEAKER_RS),
            (-145.0, SPEAKER_LB),
            (145.0, SPEAKER_RB),
            (180.0, SPEAKER_CB),
            (-180.0, SPEAKER_CB),
        ] {
            assert!(
                close(room_position(azimuth, 0.0, 1.0), speaker_position(speaker)),
                "{azimuth}: {:?}",
                room_position(azimuth, 0.0, 1.0)
            );
        }
    }

    #[test]
    fn azimuth_between_speakers_keeps_its_proportion_along_the_wall() {
        // A third of the way from front left (−30°) to side left (−90°).
        assert!(close(room_position(-50.0, 0.0, 1.0), [0.0, 0.5 / 3.0, 0.0]));
        // Halfway between centre and front right.
        assert!(close(room_position(15.0, 0.0, 1.0), [0.75, 0.0, 0.0]));
        // Across the back: 170° is 10/35 of the way from back centre to the rear right corner.
        assert!(close(
            room_position(170.0, 0.0, 1.0),
            [0.5 + 0.5 * 10.0 / 35.0, 1.0, 0.0]
        ));
        // Wrapped angles name the same place.
        assert!(close(
            room_position(-190.0, 0.0, 1.0),
            room_position(170.0, 0.0, 1.0)
        ));
        assert!(close(
            room_position(330.0, 0.0, 1.0),
            room_position(-30.0, 0.0, 1.0)
        ));
    }

    #[test]
    fn elevation_reaches_the_ceiling_at_the_height_ring_and_its_centre_overhead() {
        assert!(close(room_position(-30.0, 22.5, 1.0), [0.0, 0.0, 0.5]));
        assert!(close(room_position(-30.0, 45.0, 1.0), [0.0, 0.0, 1.0]));
        assert!(close(room_position(-30.0, 67.5, 1.0), [0.25, 0.25, 1.0]));
        for azimuth in [-180.0, -90.0, 0.0, 45.0, 135.0] {
            assert!(close(room_position(azimuth, 90.0, 1.0), [0.5, 0.5, 1.0]));
        }
        // Below the listener plane: on it.
        assert!(close(room_position(90.0, -30.0, 1.0), [1.0, 0.5, 0.0]));
    }

    #[test]
    fn distance_moves_towards_the_listener() {
        assert!(close(room_position(-30.0, 45.0, 0.5), [0.25, 0.25, 0.5]));
        assert!(close(room_position(77.0, 12.0, 0.0), [0.5, 0.5, 0.0]));
        // Beyond the reference radius: on it.
        assert!(close(room_position(-30.0, 0.0, 4.0), [0.0, 0.0, 0.0]));
    }

    #[test]
    fn the_imax_components_sit_on_centre_left_and_right() {
        assert!(close(
            room_position(0.0, 0.0, 1.0),
            speaker_position(SPEAKER_C)
        ));
        assert!(close(
            room_position(-30.0, 0.0, 1.0),
            speaker_position(SPEAKER_L)
        ));
        assert!(close(
            room_position(30.0, 0.0, 1.0),
            speaker_position(SPEAKER_R)
        ));
    }
}
