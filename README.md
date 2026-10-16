# LibDTSX

A C-callable decoder for DTS:X on Apple platforms. It decodes the lossless DTS-HD Master Audio
stream DTS:X is carried on and returns everything a renderer needs to place it in a room: the
compatible bed with the encoder's spatial folds removed, the fixed height channels, and the
objects of the alternate presentations with their positions and timed updates. It ships as
`DTSX.xcframework`, a dynamic framework per platform, with a Swift package, for iOS, tvOS, macOS
and visionOS. The built library is LGPL-3.0-or-later, see [License](#license).

Its API has the shape of [LibTrueHD](https://github.com/superuser404notfound/LibTrueHD)'s, so a
host that renders TrueHD Atmos objects into a speaker bed can render DTS:X with the same code:
planar float channels, a speaker code or "object" per channel, and metadata updates with
positions in the room coordinates Dolby Atmos uses.

The decoding is the [`dca`](https://github.com/harletty/harletty-bridge/tree/a717bae441b116485f0254603dc20a6ede65d8c6/dca)
crate from harletty-bridge, a port of FFmpeg's `dca` decoder, pinned to an exact commit. This repository adds the C API,
the framing and input timeline, the DTS:X verdict, the spatial reconstruction (which follows
[SpatialAudioBridge](https://github.com/kristofferR/SpatialAudioBridge)'s DTS:X decoder), the
conversion of DTS:X object angles into room coordinates, and the Apple packaging.

## Use

```swift
.package(url: "https://github.com/superuser404notfound/LibDTSX", .upToNextMajor(from: "2.0.0")),
// target dependency:
.product(name: "DTSX", package: "LibDTSX"),
```

```swift
import DTSX

let decoder = dtsx_decoder_create()!
defer { dtsx_decoder_destroy(decoder) }

// For every demuxed DTS-HD packet:
dtsx_decoder_push(decoder, bytes, count, Int64.min)
var block = DTSXBlock()
var status = dtsx_decoder_pull(decoder, &block)
while status == DTSX_OK {
    // block.channels: planar float32, one plane per channel (bed, object or fixed height)
    // block.speakers: each channel's speaker code, DTSX_SPEAKER_OBJECT for objects
    // block.metadata: positions, and updates starting in this block
    // block.input_frame_offset: input frames before this block since the last reset
    status = dtsx_decoder_pull(decoder, &block)
}
if status == DTSX_ERR_NOT_DTSX || status == DTSX_ERR_UNSUPPORTED {
    // Not DTS:X this library renders: play the track the way you did before.
}

// On seek:
dtsx_decoder_reset(decoder)
```

[`include/dtsx.h`](include/dtsx.h) is the reference: ownership, threading, the refusal contract,
coordinates, the input timeline and every metadata field are documented there.

## What comes out

| Presentation | Channels | In content |
|---|---|---|
| 7.1.4 (standard) | 7.1 bed, then Tfl, Tfr, Tbl, Tbr | Every frame of every DTS:X film in a [library census](https://github.com/kristofferR/SpatialAudioBridge/blob/main/docs/validation.md#library-census) |
| 7.1.4+1, +2, +4, +5 | 7.1 bed, then 1, 2, 4 or 5 objects, then the four heights | +1 in classic and IMAX-labelled streams of `dca`'s corpus; +2 in *Independence Day* (FFmpeg: "DTS:X IMAX"), two static objects at ±34.5°, 12° up; +5 in Trinnov's *Experience* trailer |
| 7.1.4+3 | 7.1 bed, one object as three components at C, L and R, then the four heights | `dca`'s IMAX test vector |
| 5.1+1 | 5.1 bed, then one object | |

The alternate presentations are classified by `dca` from a corpus and are experimental there and
here (`DTSXStreamInfo.experimental`).

- **The bed is cleaned.** The encoder folds every height and object into the compatible 7.1 bed it
  also transmits. The library subtracts those contributions, at the gains the frame states or that
  `dca`'s fold renderer recomputes for objects panned into the bed from their position, so the bed,
  the heights and the objects can be rendered side by side without playing anything twice.
- **Objects are placed in the room.** DTS:X states object positions as angles around the listener.
  The library projects the reference sphere onto the room's boundary, so a renderer panning by
  room position reproduces panning by angle between neighbouring speakers: the header's
  COORDINATES section gives the mapping. The angles are reported as transmitted too.
- **Refusals are cheap.** The first frame decides whether the stream is DTS:X this library renders
  (`DTSX_ERR_NOT_DTSX`, `DTSX_ERR_UNSUPPORTED`). FFmpeg names only two of the extension's markers,
  so it labels some DTS:X tracks plain "DTS-HD MA"; a host can offer every DTS-HD MA track and fall
  back on the answer before playback starts.
- **Seeks start clean.** The lossless residual is coded against the core decoder's filter and
  predictor history, which a stream entered mid-way does not have: its first frame decodes
  distorted. The first two frames after `create()` or `reset()` only rebuild that history; they
  produce no block and count on the input timeline. Measured over 120 seeks into five DTS:X
  streams, the first block after a seek then differs from a continuous decode by at most −33 dBFS,
  and the output is bit-identical again within about ten frames.
- **Nothing fails mid-stream.** A frame whose extension cannot be applied (none, damaged
  metadata, a fold the stream does not state) is returned with `DTSX_BLOCK_BED_AS_AUTHORED`: its
  bed as transmitted and silent heights and objects. None of the films tested had one.

## Build

```sh
./build.sh
```

Requires Xcode and rustup. The toolchain and targets are pinned in `rust-toolchain.toml` (stable
Rust; every Apple target used here ships a prebuilt std). The script builds each slice with fat
LTO as a dynamic library that exports only the `dtsx_*` functions (Rust's runtime, its personality
routine included, stays inside the framework, so it sits next to other Rust libraries such as
LibTrueHD and LibDovi without colliding), wraps it in `DTSX.framework` with a dSYM for each device
slice, and assembles the xcframework. Source paths are remapped so the binaries carry no
build-machine paths. Minimum OS: iOS 18, tvOS 18, macOS 15, visionOS 1.

## Verify

```sh
harness/verify.sh
```

It downloads FFmpeg's FATE `master_audio_7.1_24bit.dts` (plain DTS-HD MA), clones SpatialAudioBridge
at a pinned commit and builds its renderer, then checks the macOS slice: a 7.1.4 render of the
decoder's output, made by SpatialAudioBridge's rule, must match SpatialAudioBridge's own after the
priming frames, in any chunking. It is bit-identical for the standard presentation; panned objects
go through sin and cos, which Apple's C and Rust toolchains round up to a unit in the last place
apart (6e-8 on the D4 sample), so they must agree within 1e-6. Seeks must restart the input
timeline and converge, and the plain stream must be refused. It also links every other slice and
imports the module from Swift. Working files go to `target/work/`.

The DTS:X samples have no stable public URL, so put them in `target/work/samples/` yourself, as
elementary streams (`ffmpeg -i <file> -map 0:a:0 -c copy -f dts <name>`):

| File | Source | SHA-256 of the stream |
|---|---|---|
| `dtsx-object-emulator.dts` | "DTS:X Object Emulator", from the [Kodi sample list](https://kodi.wiki/view/Samples) | `718cb0dc98f67dcf6f04778fb64ff8862fc625bb782353a6ff9d828fb05fbd69` |
| `dtsx-experience-d4.dts` | Trinnov's "Experience" trailer, labelled DTS-X Pro, from [demolandia](https://www.demolandia.net/cinema/dts-demo-trailers-hd/page-6.html) | `5e8fc319289ab9ce5a44c7f161feb2fe497babf442185c8c965b34e1990a675c` |

`cargo test --release` covers what the harness does not reach (arguments, the refusal contract,
damage between frames, a frame with a damaged extension) and reads the Object Emulator from
`target/work/samples/` or `$DTSX_SAMPLE`. `cargo run --release --example dtsx_info -- <stream>`
reports a stream's presentation, channels, levels, object positions and decode speed.

Beyond the samples, the harness rule matched SpatialAudioBridge bit for bit (on Linux, where C and
Rust share one libm) on three 60-second windows each of *8 Mile* and *The Bourne Identity* (7.1.4)
and *Independence Day* (7.1.4+2), and on both samples, and
the decoder refused three windows of *Arrival* (plain DTS-HD MA) on their first frame. Decoding
runs at about 200× real time for 7.1.4 and 150× for 7.1.4+5 on one core of a desktop x86-64 CPU.

## Limitations

- Only DTS:X on lossless DTS-HD Master Audio is rendered; DTS:X on a lossy carrier is refused.
- The alternate presentations rest on `dca`'s corpus evidence, and no listening sign-off exists
  for them yet.
- The object placement is an approximation of DTS:X rendering, not DTS's reference renderer.

## License

The source in this repository is Apache-2.0, see [LICENSE](LICENSE). The **built library is
LGPL-3.0-or-later**, and that is what an app embedding it receives:

- The `dca` crate is labelled Apache-2.0 upstream, but its sources say it is a port of FFmpeg's
  `dca` decoder (`libavcodec/dca*.c`, tables generated from the libavcodec sources), which is
  LGPL-2.1-or-later. It is treated as a work based on FFmpeg under that license
  ([AetherEngine#798](https://github.com/superuser404notfound/AetherEngine/issues/798)).
- The Apache-2.0 code it is combined with is compatible with version 3 of the GNU licenses, not
  with version 2, so the combination is LGPL-3.0-or-later.

For that reason every slice is a **dynamic framework** with an `@rpath` install name, never a static
library: Xcode embeds and signs it in the app bundle, and the app stays closed source if it wants,
as long as a user can swap in a modified `DTSX.framework` (LGPL section 4). An app shipping it
should name the library and its license, point to this repository and the `dca` commit as the
corresponding source, and not prevent replacing the framework. The license texts are in
[LICENSES/](LICENSES/), and [NOTICE](NOTICE) lists every component; anyone redistributing the
xcframework must keep both with it.

DTS, DTS-HD, DTS-HD Master Audio and DTS:X are trademarks of DTS, Inc. or its affiliates. This
project is not affiliated with or endorsed by DTS.
