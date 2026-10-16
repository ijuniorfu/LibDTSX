#!/usr/bin/env bash
# verify.sh - verify DTSX.xcframework against SpatialAudioBridge's DTS:X renderer.
#
# Usage: harness/verify.sh        (after ./build.sh)
#
# Working files (samples, the SpatialAudioBridge checkout, reference renders) live in
# $DTSX_WORK, default target/work/ in this repository (ignored by git).
#
# 1. Samples. FFmpeg's FATE plain DTS-HD Master Audio 7.1 stream is downloaded. DTS:X samples
#    have no stable public URL, so the two DTS:X streams are read from $DTSX_WORK/samples and
#    checked against the SHA-256 values SpatialAudioBridge validated; README.md says where to get
#    them. A missing one is reported and its checks are skipped.
# 2. Clones SpatialAudioBridge at the commit pinned below, builds its `render` example and renders
#    each DTS:X sample to 7.1.4: the bed with every fold removed, the fixed heights on their
#    speakers and the objects panned by angle.
# 3. Builds the C harness against the xcframework's macOS slice and runs it. Its own 7.1.4 render
#    of the decoder's output, by the same rule, must match SpatialAudioBridge's after the two
#    priming frames: bit for bit, but for panned objects within 1e-6, since sin and cos may round
#    differently in C and Rust; the same with 4096-byte and with random chunks; seeks at 25, 50 and 75% must
#    restart the input timeline and converge on the continuous decode; the plain stream must be
#    refused on its first frame, and stay refused.
# 4. Links a small C program against every other slice, links the macOS framework into a host that
#    also carries a second Rust library, Objective-C and C++, and compiles and runs a Swift program
#    through the framework module (`import DTSX`).
# Requires Xcode, rustup and curl.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE="$(dirname "$HERE")"
ROOT="${DTSX_WORK:-$CRATE/target/work}"
SAMPLES="$ROOT/samples"
REF="$ROOT/ref"
XCFW="$CRATE/DTSX.xcframework"
OUT="$ROOT/verify"
SAB_URL=https://github.com/kristofferR/SpatialAudioBridge
SAB_REV=c27e1c0dfd144300bf2ba9c4fb20a90d02b0587e
for dir in /opt/homebrew/opt/rustup/bin "$HOME/.cargo/bin"; do
    [ -d "$dir" ] && PATH="$dir:$PATH"
done
export PATH

[ -d "$XCFW" ] || { echo "ERROR: build the xcframework first (./build.sh)" >&2; exit 1; }
mkdir -p "$SAMPLES" "$REF" "$OUT"

# ---- samples ----------------------------------------------------------------------------
[ -f "$SAMPLES/fate-master-audio-7.1.dts" ] ||
    curl -fsSL -o "$SAMPLES/fate-master-audio-7.1.dts" https://fate-suite.ffmpeg.org/dts/master_audio_7.1_24bit.dts

# name, SHA-256 of the elementary stream, DTSX_PRESENTATION_* it carries
DTSX_SAMPLES=(
    "dtsx-object-emulator.dts 718cb0dc98f67dcf6f04778fb64ff8862fc625bb782353a6ff9d828fb05fbd69 0"
    "dtsx-experience-d4.dts 5e8fc319289ab9ce5a44c7f161feb2fe497babf442185c8c965b34e1990a675c 5"
)
present=()
for spec in "${DTSX_SAMPLES[@]}"; do
    read -r name sha presentation <<<"$spec"
    if [ ! -f "$SAMPLES/$name" ]; then
        echo "==> $name missing from $SAMPLES: its checks are skipped (see README.md, Verify)"
    elif [ "$(shasum -a 256 "$SAMPLES/$name" | cut -d' ' -f1)" != "$sha" ]; then
        echo "ERROR: $SAMPLES/$name is not the validated stream (SHA-256 differs)" >&2
        exit 1
    else
        present+=("$name $presentation")
    fi
done

# ---- references from SpatialAudioBridge --------------------------------------------------
SAB_RENDER="$ROOT/SpatialAudioBridge/target/release/examples/render"
if [ "${#present[@]}" -gt 0 ]; then
    if [ ! -d "$ROOT/SpatialAudioBridge/.git" ]; then
        echo "==> Cloning SpatialAudioBridge at $SAB_REV"
        git clone --quiet "$SAB_URL" "$ROOT/SpatialAudioBridge"
    fi
    git -C "$ROOT/SpatialAudioBridge" cat-file -e "$SAB_REV^{commit}" 2>/dev/null ||
        git -C "$ROOT/SpatialAudioBridge" fetch --quiet "$SAB_URL" "$SAB_REV"
    git -C "$ROOT/SpatialAudioBridge" checkout --quiet "$SAB_REV"
    if [ ! -x "$SAB_RENDER" ]; then
        echo "==> Building SpatialAudioBridge's render example"
        (cd "$ROOT/SpatialAudioBridge/Rust/SpatialAudioCore" &&
            CARGO_TARGET_DIR="$ROOT/SpatialAudioBridge/target" cargo build --release --locked --example render)
    fi
    for entry in ${present[@]+"${present[@]}"}; do
        read -r name _ <<<"$entry"
        [ -s "$REF/${name%.dts}.f32" ] || "$SAB_RENDER" dtsx "$SAMPLES/$name" "$REF/${name%.dts}.f32" 7.1.4 >/dev/null
    done
fi

# ---- harness against the macOS slice ------------------------------------------------------
MAC="$XCFW/macos-arm64_x86_64"
# link_flags <slice>: compile and link against that slice's framework, found at run time by rpath.
link_flags() { echo "-I$XCFW/$1/DTSX.framework/Headers -F$XCFW/$1 -framework DTSX -Wl,-rpath,$XCFW/$1"; }
# -ffp-contract=off: the reference render rounds every product and sum separately.
clang -std=c11 -O2 -ffp-contract=off -Wall -Wextra -mmacosx-version-min=15.0 \
    -o "$OUT/harness" "$HERE/harness.c" $(link_flags macos-arm64_x86_64)
H="$OUT/harness"

failures=0
run() { # <label> <harness args...>
    local label="$1" status=0
    shift
    local log="$OUT/$(printf '%s' "$label" | tr -c 'A-Za-z0-9' '_').log"
    LAST_LOG="$log"
    echo
    echo "======== $label"
    "$H" "$@" >"$log" 2>&1 || status=$?
    grep -v -E 'deliberate panic|panicked at|RUST_BACKTRACE' "$log" || true
    if [ "$status" -ne 0 ]; then
        echo "!!!!!!!! FAILED: $label"
        failures=$((failures + 1))
    fi
}
# <label> <render> <reference> <log>: the render matches the reference after the priming frames.
same_as_reference() {
    local first
    first="$(sed -n 's/^first block at input frame //p' "$4")"
    echo "  against SpatialAudioBridge's 7.1.4 render:"
    if [ -z "$first" ] || ! "$H" --compare "$2" "$3" "$first"; then
        echo "!!!!!!!! FAILED: $1 differs from SpatialAudioBridge's render"
        failures=$((failures + 1))
    fi
}
for entry in ${present[@]+"${present[@]}"}; do
    read -r name presentation <<<"$entry"
    base="${name%.dts}"
    run "$base, 4096-byte chunks, render + seeks" \
        --expect-presentation "$presentation" --seek --render "$OUT/$base.f32" "$SAMPLES/$name"
    same_as_reference "$base" "$OUT/$base.f32" "$REF/$base.f32" "$LAST_LOG"
    run "$base, random 1..16384-byte chunks, render" \
        --random-chunks --expect-presentation "$presentation" --render "$OUT/$base-random.f32" "$SAMPLES/$name"
    if cmp -s "$OUT/$base.f32" "$OUT/$base-random.f32"; then
        echo "  identical to the 4096-byte decode"
    else
        echo "!!!!!!!! FAILED: $base decodes differently in random chunks"
        failures=$((failures + 1))
    fi
    rm -f "$OUT/$base.f32" "$OUT/$base-random.f32"
done
run "FATE master_audio_7.1_24bit.dts (plain DTS-HD MA), refused" \
    --expect-refusal -6 "$SAMPLES/fate-master-audio-7.1.dts"

# ---- every slice links; Swift sees the module ----------------------------------------------
echo
echo "======== link check per slice"
cat >"$OUT/link.c" <<'EOF'
#include "dtsx.h"
int main(void) {
    DTSXDecoder *d = dtsx_decoder_create();
    DTSXBlock b;
    int st = dtsx_decoder_pull(d, &b);
    dtsx_decoder_destroy(d);
    return dtsx_selftest() == 0 && st == DTSX_NEED_MORE_DATA ? 0 : 1;
}
EOF
for spec in "ios-arm64 arm64-apple-ios18.0 iphoneos" \
    "ios-arm64_x86_64-simulator arm64-apple-ios18.0-simulator iphonesimulator" \
    "ios-arm64_x86_64-simulator x86_64-apple-ios18.0-simulator iphonesimulator" \
    "tvos-arm64 arm64-apple-tvos18.0 appletvos" \
    "tvos-arm64-simulator arm64-apple-tvos18.0-simulator appletvsimulator" \
    "xros-arm64 arm64-apple-xros1.0 xros" \
    "xros-arm64-simulator arm64-apple-xros1.0-simulator xrsimulator" \
    "macos-arm64_x86_64 x86_64-apple-macos15.0 macosx"; do
    read -r slice triple sdk <<<"$spec"
    if xcrun --sdk "$sdk" clang -target "$triple" "$OUT/link.c" $(link_flags "$slice") \
        -o "$OUT/link-$slice" 2>"$OUT/link-$slice.log"; then
        echo "  $slice ($triple): links"
    else
        echo "  $slice ($triple): LINK FAILED"
        cat "$OUT/link-$slice.log"
        failures=$((failures + 1))
    fi
done
arch -x86_64 "$OUT/link-macos-arm64_x86_64" 2>/dev/null && echo "  macOS x86_64 slice runs under Rosetta: selftest OK" ||
    echo "  (macOS x86_64 slice not run: Rosetta unavailable or failed)"

# ---- next to a second Rust library, in a host with Objective-C and C++ ---------------------
# Compact unwind encodes at most three personality routines per image (AetherEngine#752). The
# framework is an image of its own and exports nothing but the C API, so a host carrying a Rust
# static library, Objective-C and C++ stays at three, and every one of them, ours included, must
# still catch its own exception or panic. Both link orders.
echo
echo "======== next to a second Rust library, Objective-C and C++"
cat >"$OUT/other_rust.rs" <<'EOF'
#[unsafe(no_mangle)]
pub extern "C" fn other_rust_catches() -> i32 {
    std::panic::catch_unwind(|| panic!("other_rust: deliberate panic, caught")).is_err() as i32
}
EOF
cat >"$OUT/host_objc.m" <<'EOF'
#import <Foundation/Foundation.h>
#include "dtsx.h"
int host_cxx_catches(void);
int other_rust_catches(void);
int main(void) {
    int objc = 0;
    @try { @throw [NSException exceptionWithName:@"host" reason:@"caught" userInfo:nil]; }
    @catch (NSException *e) { objc = 1; }
    return objc && host_cxx_catches() && other_rust_catches() && dtsx_selftest() == 0 ? 0 : 1;
}
EOF
cat >"$OUT/host_cxx.cpp" <<'EOF'
#include <stdexcept>
extern "C" int host_cxx_catches(void) {
    try { throw std::runtime_error("host"); } catch (const std::exception &) { return 1; }
}
EOF
if (cd "$CRATE" && rustc --edition 2024 -O --crate-type staticlib --target aarch64-apple-darwin \
        "$OUT/other_rust.rs" -o "$OUT/libother_rust.a") 2>"$OUT/coexist.log" &&
    xcrun clang -c -mmacosx-version-min=15.0 -fobjc-exceptions -I"$MAC/DTSX.framework/Headers" \
        "$OUT/host_objc.m" -o "$OUT/host_objc.o" 2>>"$OUT/coexist.log" &&
    xcrun clang++ -c -mmacosx-version-min=15.0 "$OUT/host_cxx.cpp" -o "$OUT/host_cxx.o" 2>>"$OUT/coexist.log"; then
    for order in other-first dtsx-first; do
        read -r -a dtsx <<<"$(link_flags macos-arm64_x86_64)"
        if [ "$order" = other-first ]; then libs=("$OUT/libother_rust.a" "${dtsx[@]}")
        else libs=("${dtsx[@]}" "$OUT/libother_rust.a"); fi
        if xcrun clang++ -mmacosx-version-min=15.0 "$OUT/host_objc.o" "$OUT/host_cxx.o" "${libs[@]}" \
                -framework Foundation -o "$OUT/coexist-$order" 2>>"$OUT/coexist.log" &&
            "$OUT/coexist-$order" 2>/dev/null; then
            echo "  $order: links, all four catch"
        else
            echo "  $order: FAILED"
            cat "$OUT/coexist.log"
            failures=$((failures + 1))
        fi
    done
else
    echo "  COEXISTENCE CHECK COULD NOT BUILD ITS HOST"
    cat "$OUT/coexist.log"
    failures=$((failures + 1))
fi

echo
echo "======== Swift through the framework module"
cat >"$OUT/swift_check.swift" <<'EOF'
import Foundation
import DTSX

let data = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1]))
guard let decoder = dtsx_decoder_create() else { exit(1) }
defer { dtsx_decoder_destroy(decoder) }
var block = DTSXBlock()
var blocks = 0, updates = 0, objects = 0
var status = DTSX_OK
var offset = 0
while status == DTSX_OK || status == DTSX_NEED_MORE_DATA, offset < data.count {
    let n = min(65_536, data.count - offset)
    status = data[offset..<offset + n].withUnsafeBytes {
        dtsx_decoder_push(decoder, $0.bindMemory(to: UInt8.self).baseAddress, n, 0)
    }
    offset += n
    while status == DTSX_OK {
        status = dtsx_decoder_pull(decoder, &block)
        guard status == DTSX_OK else { break }
        blocks += 1
        if let md = block.metadata {
            updates += Int(md.pointee.update_count)
            objects = Int(md.pointee.object_count)
        }
    }
}
if CommandLine.arguments.count > 2 {
    // The plain stream: refused, with a message.
    let message = String(cString: dtsx_decoder_last_error(decoder))
    print("Swift: refused with \(status) (\(message))")
    exit(status == DTSX_ERR_NOT_DTSX && blocks == 0 ? 0 : 1)
}
var info = DTSXStreamInfo()
_ = dtsx_decoder_get_stream_info(decoder, &info)
let name = String(cString: dtsx_presentation_name(info.presentation))
print("Swift: \(blocks) blocks, \(name), \(info.channel_count) ch (\(objects) objects) @ \(info.sample_rate) Hz, \(updates) metadata updates, selftest \(dtsx_selftest())")
exit(blocks > 0 && status == DTSX_NEED_MORE_DATA ? 0 : 1)
EOF
swift_sample=""
for entry in ${present[@]+"${present[@]}"}; do read -r name _ <<<"$entry"; swift_sample="$SAMPLES/$name"; done
if xcrun swiftc -O -F "$MAC" -Xlinker -rpath -Xlinker "$MAC" "$OUT/swift_check.swift" \
    -o "$OUT/swift_check" 2>"$OUT/swift.log" &&
    "$OUT/swift_check" "$SAMPLES/fate-master-audio-7.1.dts" refused 2>/dev/null &&
    { [ -z "$swift_sample" ] || "$OUT/swift_check" "$swift_sample" 2>/dev/null; }; then
    :
else
    echo "  SWIFT CHECK FAILED"
    cat "$OUT/swift.log"
    failures=$((failures + 1))
fi

echo
if [ "${#present[@]}" -lt "${#DTSX_SAMPLES[@]}" ]; then
    echo "NOTE: $((${#DTSX_SAMPLES[@]} - ${#present[@]})) DTS:X sample(s) missing; their checks did not run"
fi
if [ "$failures" -eq 0 ]; then echo "ALL CHECKS PASSED"; else echo "$failures CHECK(S) FAILED"; exit 1; fi
