# LibDTSX

> **Superseded.** AetherEngine 7.44.0 no longer uses LibDTSX: DTS:X decodes through FFmpeg's own `dca`
> decoder, which [FFmpegBuild 3.9.0](https://github.com/superuser404notfound/FFmpegBuild/releases/tag/3.9.0)
> patches to read the standard presentation's height channels (decoder option `x_heights`). That route
> is plain FFmpeg under LGPL-2.1-or-later, while this library is LGPL-3.0-or-later (see
> [License](#license)) and cannot ship in an App Store app without an exception (#2). The repository
> stays up for reference and is no longer maintained.

A C-callable decoder for DTS:X on Apple platforms. It decodes the lossless DTS-HD Master Audio
stream DTS:X is carried on and returns its standard presentation ready to play on 7.1.4 speakers:
the compatible 7.1 bed with the four height channels removed from it, and the heights on their
own channels. The alternate presentations, which carry objects, are refused. It ships as
`DTSX.xcframework`, a dynamic framework per platform, with a Swift package, for iOS, tvOS, macOS
and visionOS. The built library is LGPL-3.0-or-later, see [License](#license).

Its API has the shape of [LibTrueHD](https://github.com/superuser404notfound/LibTrueHD)'s, so a
host that renders TrueHD Atmos objects into a speaker bed can render DTS:X with the same code:
planar float channels, a speaker code per channel, and metadata with each channel's position in
the room coordinates Dolby Atmos uses.

The decoding is the [`dca`](https://github.com/harletty/harletty-bridge/tree/a717bae441b116485f0254603dc20a6ede65d8c6/dca)
crate from harletty-bridge, a port of FFmpeg's `dca` decoder, pinned to an exact commit. This repository adds the C API,
the framing and input timeline, the DTS:X verdict, the removal of the heights from the bed, and the
Apple packaging.

## Use

```swift
.package(url: "https://github.com/superuser404notfound/LibDTSX", .upToNextMajor(from: "3.0.0")),
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
    // block.channels: planar float32, one plane per channel: the 7.1 bed, then the four heights
    // block.speakers: each channel's speaker code
    // block.metadata: each channel's nominal position
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
the input timeline and every metadata field are documented there.

## What comes out

The standard presentation: the 7.1 bed, then Tfl, Tfr, Tbl and Tbr. It is the one presentation of
DTS:X content at large: every frame of every DTS:X film in a
[library census](https://github.com/kristofferR/SpatialAudioBridge/blob/main/docs/validation.md#library-census)
carries it, and so did every frame of twelve DTS:X tracks measured here (eight *Harry Potter*
films, two *Shrek* films in two languages, *Mary and the Witch's Flower*).

- **The bed is cleaned with the standard's own fold.** The encoder mixes each height into the bed
  speaker under it at -3 dB (Tfl into L, Tfr into R, Tbl into Lb, Tbr into Rb), so that a 7.1
  decoder plays the whole soundtrack. The library subtracts exactly that, so the bed and the
  heights can be played side by side without playing anything twice. It does not read the
  stream's private metadata: across the twelve tracks above, every frame stated this same fold,
  and the result is bit-identical to subtracting what the stream states.
- **Objects are not rendered.** The alternate presentations (7.1.4 plus one to five objects in some
  IMAX-labelled streams and trailers, 5.1 plus one object) are refused with `DTSX_ERR_UNSUPPORTED`
  on their first frame, so a host plays them as the plain 7.1 their compatible bed is.
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
- **Nothing fails mid-stream.** A frame whose extension cannot be applied (none, one that does
  not decode, a frame in another presentation) is returned with `DTSX_BLOCK_BED_AS_AUTHORED`: its
  bed as transmitted and silent heights. None of the films tested had one.

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
decoder's output must be bit-identical to SpatialAudioBridge's own after the priming frames, in
any chunking. SpatialAudioBridge removes the fold each frame states, so this also shows that the
standard fold the library removes is the stated one. Seeks must restart the input timeline and
converge, and the D4 sample (objects) and the plain stream must be refused. It also links every
other slice and imports the module from Swift. Working files go to `target/work/`.
`DTSX_LOCAL_SAMPLES` may name more streams in the standard presentation, such as excerpts of films,
which are checked the same way.

The DTS:X samples have no stable public URL, so put them in `target/work/samples/` yourself, as
elementary streams (`ffmpeg -i <file> -map 0:a:0 -c copy -f dts <name>`):

| File | Source | SHA-256 of the stream |
|---|---|---|
| `dtsx-object-emulator.dts` | "DTS:X Object Emulator", from the [Kodi sample list](https://kodi.wiki/view/Samples) | `718cb0dc98f67dcf6f04778fb64ff8862fc625bb782353a6ff9d828fb05fbd69` |
| `dtsx-experience-d4.dts` | Trinnov's "Experience" trailer, labelled DTS-X Pro, from [demolandia](https://www.demolandia.net/cinema/dts-demo-trailers-hd/page-6.html) | `5e8fc319289ab9ce5a44c7f161feb2fe497babf442185c8c965b34e1990a675c` |

`cargo test --release` covers what the harness does not reach (arguments, the refusal contract,
damage between frames, a frame with a damaged extension) and reads the Object Emulator from
`target/work/samples/` or `$DTSX_SAMPLE`. `cargo run --release --example dtsx_info -- <stream>`
reports a stream's presentation, channels, levels and decode speed.

Decoding runs at about 70× real time on one core of an M1.

## Limitations

- Only DTS:X on lossless DTS-HD Master Audio is rendered; DTS:X on a lossy carrier is refused.
- Only the standard presentation is rendered; streams with objects play as 7.1.
- A stream whose encoder folded its heights at another gain would keep part of each height in the
  bed. None of the content measured does.

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
