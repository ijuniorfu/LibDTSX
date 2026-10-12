/*
 * dtsx.h - C API of the LibDTSX decoder library.
 *
 * Decodes DTS:X carried on a lossless DTS-HD Master Audio stream to planar float PCM, ready to
 * render: the compatible bed with every spatial contribution the encoder folded into it removed,
 * the fixed height channels, and the objects of the alternate presentations with their
 * positions, in the room coordinates Dolby Atmos object metadata uses. Built on the Rust `dca`
 * crate from https://github.com/harletty/harletty-bridge (Apache-2.0).
 *
 * Model
 *   create -> [push bytes -> pull blocks until NEED_MORE_DATA]* -> destroy
 *   - push() accepts any chunking: whole demuxer packets (one or more DTS-HD frames each),
 *     partial frames, or arbitrary byte ranges of an elementary stream. A DTS-HD frame is a
 *     16-bit big-endian core frame followed by its extension substream; that is how Matroska,
 *     MP4, M2TS and FFmpeg's raw `dts` output carry it.
 *   - pull() decodes one DTS-HD frame per call and returns it as one block.
 *   - reset() is for seeks: it drops all buffered input and decoder state; decoding resumes
 *     at the next frame in the data pushed afterwards.
 *
 * Refusal
 *   The first frame decoded since create() decides whether the stream is DTS:X this library can
 *   render. Deciding takes one frame (512 samples at 48 kHz, a few kilobytes), so a host can try
 *   every DTS-HD MA track and fall back on the answer before playback starts. FFmpeg names only
 *   two of the extension's markers, so it labels some DTS:X tracks plain "DTS-HD MA".
 *     DTSX_ERR_NOT_DTSX       the stream carries no DTS:X extension: plain DTS-HD MA, a DTS core
 *                             or DTS-HD High Resolution Audio, or no DTS frame in the first MiB.
 *     DTSX_ERR_UNSUPPORTED    it carries one this library does not render: DTS:X on a lossy
 *                             carrier, an extension that matches no known presentation, or a
 *                             stream whose first frames all fail to decode.
 *   A refusal is returned by every later push() and pull() until destroy(); reset() keeps it,
 *   since it describes the stream, not a position in it. After a stream has been accepted,
 *   nothing about its content makes pull() fail: a frame whose extension cannot be applied
 *   (no extension, metadata that does not parse, a fold the stream does not state) is returned
 *   with DTSX_BLOCK_BED_AS_AUTHORED, see below.
 *
 * Threading
 *   A decoder handle is not thread-safe: call it from one thread at a time. Separate handles
 *   are independent. No function blocks or performs I/O.
 *
 * Memory
 *   All pointers returned inside DTSXBlock (channel planes, speakers, metadata and everything it
 *   points to) are owned by the decoder and stay valid until the next call to
 *   dtsx_decoder_pull(), _reset() or _destroy() on the same handle. push(), get_stream_info(),
 *   get_stats() and last_error() do not invalidate them. Output planes, metadata and
 *   bookkeeping are preallocated per handle, and the decoder reuses its frame buffers once a
 *   stream's layout holds.
 *
 * Errors
 *   Functions return DTSXStatus. Damaged or undecodable input in an accepted stream never makes
 *   pull() fail: the decoder skips the frame (counting its samples, see INPUT TIMELINE),
 *   counts the event in DTSXStats, records a message for dtsx_decoder_last_error(), and flags
 *   the next block with DTSX_BLOCK_DISCONTINUITY. Rust panics never cross this API; if one is
 *   caught outside the per-frame recovery, the handle returns DTSX_ERR_PANIC until
 *   dtsx_decoder_reset() or _destroy().
 */
#ifndef DTSX_H
#define DTSX_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Constants are macros with the type of the field or parameter they go with, so they stay
 * constant expressions in C and import into Swift with that type (UInt8, UInt32, Int32),
 * comparable without conversions.
 */

#define DTSX_API_VERSION 1u

/** Most channels any presentation can carry: an 8-channel bed and 9 extension feeds. */
#define DTSX_MAX_CHANNELS 24u
/** Most frames one block can hold (one DTS frame of 4096 samples). */
#define DTSX_MAX_BLOCK_FRAMES 4096u
/** "No timestamp" for push() and DTSXBlock.pts: INT64_MIN, the same value as FFmpeg's
    AV_NOPTS_VALUE, so AVPacket.pts can be passed as is. Swift does not import this macro;
    use Int64.min. */
#define DTSX_NO_PTS INT64_MIN

/* ------------------------------------------------------------------------------------------ */
/* Status codes                                                                               */

typedef int32_t DTSXStatus;

#define DTSX_OK ((DTSXStatus)0)
/** pull(): no complete frame is buffered; push more data (or stop at end of stream). */
#define DTSX_NEED_MORE_DATA ((DTSXStatus)1)
/** A required pointer argument was NULL. */
#define DTSX_ERR_NULL ((DTSXStatus)-1)
/** An argument was out of range. */
#define DTSX_ERR_INVALID_ARGUMENT ((DTSXStatus)-2)
/** get_stream_info(): no frame has been decoded since create(). */
#define DTSX_ERR_NOT_READY ((DTSXStatus)-3)
/** push(): more than 8 MiB of input is waiting; pull() before pushing more. Nothing was consumed. */
#define DTSX_ERR_BUFFER_FULL ((DTSXStatus)-4)
/** An internal panic was caught at the API boundary; call reset() or destroy(). */
#define DTSX_ERR_PANIC ((DTSXStatus)-5)
/** The stream is not DTS:X (see Refusal above). Sticky until destroy(). */
#define DTSX_ERR_NOT_DTSX ((DTSXStatus)-6)
/** The stream is DTS:X this library does not render (see Refusal above). Sticky until destroy(). */
#define DTSX_ERR_UNSUPPORTED ((DTSXStatus)-7)

/* ------------------------------------------------------------------------------------------ */
/* Presentations                                                                              */
/*
 * What the DTS:X extension of a frame carries next to the compatible bed. The standard
 * presentation is the only one established by DTS:X content at large; the alternates are
 * classified by the `dca` crate from a corpus and are experimental there and here
 * (DTSXStreamInfo.experimental). All eight DTS:X films in a library census used the standard
 * one in every frame.
 *
 * Every presentation's channels come in this order: the bed channels in DCA speaker order,
 * then the objects, then the fixed channels.
 */
#define DTSX_PRESENTATION_HEIGHT 0u       /* 7.1.4: four fixed heights (Tfl, Tfr, Tbl, Tbr) */
#define DTSX_PRESENTATION_OBJECT_D0 1u    /* 7.1.4+1: one object, then the four heights */
#define DTSX_PRESENTATION_OBJECTS_D1 2u   /* 7.1.4+2: two objects, then the four heights */
#define DTSX_PRESENTATION_OBJECTS_D0 3u   /* 7.1.4+3: one object as three components at the bed's
                                             C, L and R, then the four heights */
#define DTSX_PRESENTATION_OBJECTS_D3 4u   /* 7.1.4+4: four objects, then the four heights */
#define DTSX_PRESENTATION_OBJECTS_D4 5u   /* 7.1.4+5: five objects, then the four heights */
#define DTSX_PRESENTATION_OBJECT_ONLY 6u  /* 5.1+1: one object over a 5.1 bed, no heights */

/* ------------------------------------------------------------------------------------------ */
/* Speakers                                                                                   */
/*
 * Channel/speaker codes for bed and fixed channels. The values are LibTrueHD's
 * (TRUEHD_ATMOS_SPEAKER_*), so a host can share one mapping between the two libraries. DTS
 * bed speakers map onto them as noted; a 5.1 bed's surround pair (DTS Ls/Rs) and a 7.1 bed's
 * side pair (DTS Lss/Rss) are both DTSX_SPEAKER_LS/RS, and its rear pair (DTS Lsr/Rsr) is
 * DTSX_SPEAKER_LB/RB.
 */
#define DTSX_SPEAKER_L ((uint8_t)0)
#define DTSX_SPEAKER_R ((uint8_t)1)
#define DTSX_SPEAKER_C ((uint8_t)2)
#define DTSX_SPEAKER_LFE ((uint8_t)3)
#define DTSX_SPEAKER_LS ((uint8_t)4)   /* left surround / side surround (DTS Ls, Lss) */
#define DTSX_SPEAKER_RS ((uint8_t)5)   /* right surround / side surround (DTS Rs, Rss) */
#define DTSX_SPEAKER_TFL ((uint8_t)6)  /* top front left (DTS:X fixed height, DTS Lh) */
#define DTSX_SPEAKER_TFR ((uint8_t)7)  /* top front right (DTS:X fixed height, DTS Rh) */
#define DTSX_SPEAKER_TSL ((uint8_t)8)  /* top side left (DTS Lhs) */
#define DTSX_SPEAKER_TSR ((uint8_t)9)  /* top side right (DTS Rhs) */
#define DTSX_SPEAKER_TBL ((uint8_t)10) /* top back left (DTS:X fixed height, DTS Lhr) */
#define DTSX_SPEAKER_TBR ((uint8_t)11) /* top back right (DTS:X fixed height, DTS Rhr) */
#define DTSX_SPEAKER_LSC ((uint8_t)12) /* left of centre (DTS Lc) */
#define DTSX_SPEAKER_RSC ((uint8_t)13) /* right of centre (DTS Rc) */
#define DTSX_SPEAKER_LB ((uint8_t)14)  /* left back / rear surround (DTS Lsr) */
#define DTSX_SPEAKER_RB ((uint8_t)15)  /* right back / rear surround (DTS Rsr) */
#define DTSX_SPEAKER_CB ((uint8_t)16)  /* centre back (DTS Cs) */
#define DTSX_SPEAKER_TC ((uint8_t)17)  /* top centre, overhead (DTS Oh) */
#define DTSX_SPEAKER_LSD ((uint8_t)18) /* left surround direct */
#define DTSX_SPEAKER_RSD ((uint8_t)19) /* right surround direct */
#define DTSX_SPEAKER_LW ((uint8_t)20)  /* left wide (DTS Lw) */
#define DTSX_SPEAKER_RW ((uint8_t)21)  /* right wide (DTS Rw) */
#define DTSX_SPEAKER_TFC ((uint8_t)22) /* top front centre (DTS Ch) */
#define DTSX_SPEAKER_LFE2 ((uint8_t)23)
/** The channel is an object, positioned by metadata. */
#define DTSX_SPEAKER_OBJECT ((uint8_t)254)
/** A bed speaker this API has no code for. */
#define DTSX_SPEAKER_UNKNOWN ((uint8_t)255)

/* ------------------------------------------------------------------------------------------ */
/* Metadata                                                                                   */
/*
 * COORDINATES (DTSXElement.position) are room coordinates, the convention of Dolby Atmos
 * object metadata (ETSI TS 103 420 clause 4.2.1) and of LibTrueHD:
 *   x: 0 = left wall      .. 1 = right wall
 *   y: 0 = front wall     .. 1 = back wall
 *   z: 0 = listener (ear-level) plane .. 1 = ceiling
 * DTS:X states object positions as physical angles around the listener (azimuth, elevation,
 * distance), which DTSXElement also reports as transmitted. The library places them in the
 * room by projecting the reference sphere onto the room's boundary:
 *   - Azimuth (0 = front, positive = right) runs along the walls, piecewise linear between the
 *     angles of the speakers that sit there: centre 0° at (0.5, 0), front ±30° at the front
 *     corners, wide ±60° a quarter down the side walls, side ±90° halfway down them, rear
 *     ±145° at the back corners and back centre 180° at (0.5, 1). A renderer that pans
 *     between neighbouring speakers by room position therefore places a sound between two
 *     speakers in the same proportion as one that pans by angle.
 *   - Elevation (degrees, positive = up) sets z = elevation / 45, reaching the ceiling at 45°,
 *     the elevation of a height speaker. Above 45° the position moves on across the ceiling
 *     towards its centre, which it reaches at 90°. Negative elevations are placed at ear level.
 *   - Distance (1 = the reference radius, the speakers) moves the position towards the
 *     listener at the room's centre, (0.5, 0.5, 0), in proportion: an object at distance 0.5
 *     sits halfway between there and its point on the boundary. Distances beyond the
 *     reference radius are placed on it.
 * These are approximations of DTS:X rendering, not DTS reference-renderer behaviour.
 *
 * Bed and fixed channels (kind BED/LFE) carry their speaker's nominal position in the same
 * coordinates (LFE at the front centre, z = 0).
 *
 * TIMING: DTS:X restates its metadata with every frame. An update is delivered at frame_offset
 * 0 of the block whose frame stated it, whenever an element differs from the previous frame:
 * ramp each changed element linearly from its previous values to the update's over
 * ramp_frames frames, which is the block's frame_count, so a moving object reaches its new
 * position on the frame's last sample (frame i of the ramp, from 0, plays at (i + 1) /
 * ramp_frames of the way). The first block after create/reset/DISCONTINUITY, and the first of
 * a new layout, carries an update describing every element (all CHANGED) with ramp_frames 0:
 * apply it as a jump. Elements without DTSX_ELEMENT_CHANGED are unchanged; do not restart
 * their ramps.
 */

/* DTSXElement.kind */
#define DTSX_ELEMENT_BED ((uint8_t)0)     /* speaker-anchored channel; see .speaker */
#define DTSX_ELEMENT_LFE ((uint8_t)1)     /* LFE or LFE2 */
#define DTSX_ELEMENT_OBJECT ((uint8_t)2)  /* object, positioned by .position */

/* DTSXElement.flags */
#define DTSX_ELEMENT_ACTIVE 0x001u   /* always set: DTS:X states no object as inactive */
#define DTSX_ELEMENT_CHANGED 0x002u  /* in an update: differs from the previous values */

typedef struct DTSXElement {
    uint8_t kind;          /* DTSX_ELEMENT_* */
    uint8_t speaker;       /* bed/LFE: DTSX_SPEAKER_*; objects: DTSX_SPEAKER_OBJECT */
    uint8_t reserved[2];
    uint32_t flags;        /* DTSX_ELEMENT_* */
    float position[3];     /* x, y, z; see COORDINATES above */
    float gain;            /* linear amplitude gain: 1 (DTS:X objects carry their level in the PCM) */
    float azimuth;         /* objects: transmitted azimuth in degrees, 0 = front, positive = right */
    float elevation;       /* objects: transmitted elevation in degrees, positive = up */
    float distance;        /* objects: transmitted distance, 1 = reference radius; else 0 */
} DTSXElement;             /* 36 bytes */

typedef struct DTSXMetadataUpdate {
    uint32_t frame_offset;          /* first frame of this block the update applies from (0) */
    uint32_t ramp_frames;           /* interpolation length in frames, 0 = jump */
    const DTSXElement *elements;    /* element_count entries: the values reached after the ramp */
} DTSXMetadataUpdate;

typedef struct DTSXMetadata {
    uint32_t element_count;   /* == DTSXBlock.channel_count; element i is channel i */
    uint32_t bed_count;       /* elements [0, bed_count) are the bed channels (incl. LFE) */
    uint32_t object_count;    /* then object_count objects */
    uint32_t fixed_count;     /* then fixed_count fixed channels, up to element_count */
    uint32_t update_count;    /* updates starting inside this block (0 or 1) */
    uint32_t reserved;
    const DTSXMetadataUpdate *updates; /* update_count entries */
    const DTSXElement *elements;       /* element_count entries: the values in force at the end
                                          of this block (CHANGED bits cleared). Use this
                                          snapshot if you do not ramp sample-accurately. */
} DTSXMetadata;

/* ------------------------------------------------------------------------------------------ */
/* Decoded blocks                                                                             */
/*
 * SAMPLE RATE: blocks carry the stream's own rate; nothing is resampled. DTS:X is 48 kHz in
 * all content tested.
 *
 * BED: the bed channels are the stream's lossless compatible bed with every spatial
 * contribution the encoder folded into it subtracted, at the gains the frame's metadata states
 * (or, for objects it marks as panned into the bed from their position, at the gains the
 * `dca` crate's fold renderer recomputes). The bed, objects and fixed channels are therefore
 * meant to be rendered side by side: summing them back into the bed speakers reproduces the
 * compatible mix.
 *
 * BED AS AUTHORED: when a frame's extension cannot be applied (the frame has none, its
 * metadata does not parse or does not match the presentation, or a fold is not stated and
 * cannot be recomputed), the block carries DTSX_BLOCK_BED_AS_AUTHORED: its bed channels are
 * the compatible mix as transmitted, the objects and fixed channels are silent, and the layout
 * stays that of the stream. Nothing is lost or played twice; the frame's heights and objects
 * play from the bed speakers they were folded into. None of the films tested had such frames.
 *
 * INPUT TIMELINE: input_access_unit counts every DTS-HD frame in the bytes pushed since
 * create()/reset(), in stream order, whether or not it was decoded: frames the lossless
 * decoder had to skip while it buffered (peak bit rate smoothing) and frames that failed to
 * decode all count. input_frame_offset is the number of input frames (samples per channel)
 * preceding this block's first frame, summing each frame's own length. To place a block on
 * the source timeline after a seek: time = (pts of the first push after reset) +
 * input_frame_offset / sample_rate. This relies on the first push after create()/reset()
 * starting at a frame boundary, which demuxed packets always do. Bytes that are not part of a
 * frame (damage, or a push starting mid-frame) are skipped up to the next frame; when that
 * happens after the first frame since reset, the frames they held are estimated from the
 * average frame size, and DTSX_BLOCK_INPUT_OFFSET_ESTIMATED is set on this and every later
 * block until reset().
 *
 * LAYOUT: layout_serial identifies the channel roles: sample rate, presentation, channel count
 * and each channel's speaker code. It is constant for a stream and across reset() on the same
 * stream; it increments, with DTSX_BLOCK_LAYOUT_CHANGED on the first block, only when those
 * roles change (a bed or presentation change after a splice or seek).
 */

/* DTSXBlock.flags */
/** First block after create/reset, or after data was dropped (skipped frame, damage). */
#define DTSX_BLOCK_DISCONTINUITY 0x01u
/** layout_serial differs from the previous block's (also set on the first block). */
#define DTSX_BLOCK_LAYOUT_CHANGED 0x02u
/** .metadata is non-NULL (always, for a block of an accepted stream). */
#define DTSX_BLOCK_HAS_METADATA 0x04u
/** .metadata->update_count > 0. */
#define DTSX_BLOCK_METADATA_UPDATED 0x08u
/** input_access_unit includes an estimate (see INPUT TIMELINE); sticky until reset(). */
#define DTSX_BLOCK_INPUT_OFFSET_ESTIMATED 0x10u
/** The frame's extension could not be applied (see BED AS AUTHORED). */
#define DTSX_BLOCK_BED_AS_AUTHORED 0x20u

typedef struct DTSXBlock {
    uint32_t sample_rate;         /* Hz */
    uint32_t frame_count;         /* frames in each channel plane */
    uint32_t channel_count;       /* planes in .channels */
    uint32_t presentation;        /* DTSX_PRESENTATION_* */
    uint32_t flags;               /* DTSX_BLOCK_* */
    uint32_t access_unit_frames;  /* frames of the stream's frames (512 at 48 kHz in DTS-HD MA) */
    int64_t pts;                  /* pts given to the push() whose bytes contained this frame's
                                     first byte, or DTSX_NO_PTS */
    uint32_t pts_au_index;        /* frames that started earlier within that same push:
                                     block time = pts + pts_au_index * access_unit_frames / rate */
    uint32_t layout_serial;       /* see LAYOUT above */
    uint64_t sample_position;     /* frames output since create()/reset(), before this block */
    uint64_t input_access_unit;   /* see INPUT TIMELINE above */
    uint64_t input_frame_offset;  /* see INPUT TIMELINE above */
    const float *const *channels; /* channel_count pointers to frame_count floats, planar,
                                     24-bit PCM normalised so that full scale is [-1, 1) */
    const uint8_t *speakers;      /* channel_count DTSX_SPEAKER_* codes */
    const DTSXMetadata *metadata; /* see DTSX_BLOCK_HAS_METADATA */
} DTSXBlock;

/* ------------------------------------------------------------------------------------------ */
/* Stream information                                                                         */

typedef struct DTSXStreamInfo {
    uint32_t sample_rate;
    uint32_t access_unit_frames;
    uint32_t presentation;        /* DTSX_PRESENTATION_* of the most recent frame with one */
    uint32_t experimental;        /* 1: an alternate presentation (see Presentations) */
    uint32_t alternate_marker;    /* 1: the extension carries the alternate syncword, which
                                     FFmpeg reports as "DTS:X IMAX"; not proof of IMAX content */
    uint32_t bed_mask;            /* DCA speaker mask of the compatible bed */
    uint32_t channel_count;       /* channels of the most recent block */
    uint32_t layout_serial;       /* layout_serial of the most recent block */
} DTSXStreamInfo;

/* Counters since create() or the last reset(). */
typedef struct DTSXStats {
    uint64_t bytes_pushed;
    uint64_t bytes_pending;           /* pushed but not yet consumed */
    uint64_t access_units_decoded;
    uint64_t blocks_output;
    uint64_t frames_output;
    uint64_t access_units_skipped;    /* frames buffered by the lossless decoder (peak bit rate
                                         smoothing) without output of their own */
    uint64_t decode_errors;           /* frames that failed to decode */
    uint64_t sync_errors;             /* runs of bytes outside any frame, skipped */
    uint64_t bed_as_authored;         /* blocks with DTSX_BLOCK_BED_AS_AUTHORED */
    uint64_t metadata_updates;        /* updates delivered in blocks */
} DTSXStats;

/* ------------------------------------------------------------------------------------------ */
/* Functions                                                                                  */

typedef struct DTSXDecoder DTSXDecoder;

/** DTSX_API_VERSION of the compiled library. */
uint32_t dtsx_api_version(void);

/** Static, NUL-terminated library and upstream version description. */
const char *dtsx_version_string(void);

/** Static short name of a DTSX_SPEAKER_* code ("L", "Tfl", "LFE", "Obj", "?"). */
const char *dtsx_speaker_name(uint8_t speaker);

/** Static layout name of a DTSX_PRESENTATION_* value ("7.1.4", "7.1.4+5", "5.1+1", "?"). */
const char *dtsx_presentation_name(uint32_t presentation);

/**
 * Diagnostic: raises a Rust panic inside the library and catches it, as every function here
 * does with an unexpected one. Returns DTSX_OK if unwinding works in your final link (Rust's
 * panic hook prints one line to stderr). Worth calling once in a test target.
 */
DTSXStatus dtsx_selftest(void);

/** Creates a decoder. Returns NULL only if allocation fails. */
DTSXDecoder *dtsx_decoder_create(void);

/** Destroys a decoder. NULL is ignored. */
void dtsx_decoder_destroy(DTSXDecoder *decoder);

/**
 * Seek support: drops buffered input and all decoder state. Output resumes at the first frame
 * of data pushed afterwards. The next block has DTSX_BLOCK_DISCONTINUITY, and sample_position,
 * input_access_unit and the stats restart at 0. Stream info, layout_serial and a refusal stay.
 * Also clears a DTSX_ERR_PANIC state.
 */
DTSXStatus dtsx_decoder_reset(DTSXDecoder *decoder);

/**
 * Appends `size` bytes of DTS-HD bitstream. `pts` is an arbitrary caller timestamp (for
 * example AVPacket.pts) reported back on the blocks whose frames start in these bytes; pass
 * DTSX_NO_PTS if unknown. The bytes are copied; `data` may be reused on return. size == 0 is
 * a no-op (data may then be NULL). Returns the refusal once the stream has been refused.
 */
DTSXStatus dtsx_decoder_push(DTSXDecoder *decoder, const uint8_t *data, size_t size, int64_t pts);

/**
 * Decodes the next frame into *block. Returns DTSX_OK with a block, DTSX_NEED_MORE_DATA when no
 * complete frame is buffered (block untouched), a refusal (see Refusal), or an error. Call
 * repeatedly after each push() until it returns NEED_MORE_DATA; at end of stream that drains
 * everything (no separate flush is needed).
 */
DTSXStatus dtsx_decoder_pull(DTSXDecoder *decoder, DTSXBlock *block);

/**
 * Stream description from the most recent frame decoded. DTSX_ERR_NOT_READY before the first
 * one since create().
 */
DTSXStatus dtsx_decoder_get_stream_info(const DTSXDecoder *decoder, DTSXStreamInfo *info);

DTSXStatus dtsx_decoder_get_stats(const DTSXDecoder *decoder, DTSXStats *stats);

/**
 * Message describing the most recent recovered error or the refusal ("" if none). Valid until
 * the next push/pull/reset/destroy on this handle. Never NULL for a valid handle.
 */
const char *dtsx_decoder_last_error(const DTSXDecoder *decoder);

#ifdef __cplusplus
}
#endif

#endif /* DTSX_H */
