#!/usr/bin/env bash
# build.sh - build DTSX.xcframework (static library + C header + module map) from scratch.
#
# Usage: ./build.sh
# Output: ./DTSX.xcframework with the slices
#   macos-arm64_x86_64        aarch64-apple-darwin + x86_64-apple-darwin (lipo)   macOS 15.0+
#   ios-arm64                 aarch64-apple-ios                                   iOS 18.0+
#   ios-arm64_x86_64-simulator aarch64-apple-ios-sim + x86_64-apple-ios (lipo)     iOS 18.0+
#   tvos-arm64                aarch64-apple-tvos                                  tvOS 18.0+
#   tvos-arm64-simulator      aarch64-apple-tvos-sim                              tvOS 18.0+
#   xros-arm64                aarch64-apple-visionos                              visionOS 1.0+
#   xros-arm64-simulator      aarch64-apple-visionos-sim                          visionOS 1.0+
#
# Requirements: rustup (the toolchain and targets pinned in rust-toolchain.toml are installed
# on demand), Xcode command line tools (lipo, strip, xcodebuild). The decoder crate `dca`
# is fetched by cargo at the commit pinned in Cargo.toml; Cargo.lock pins everything else.
#
# All Apple targets used are tier 2 with a prebuilt std on stable Rust, so no nightly and no
# -Zbuild-std. The iOS simulator slice also carries x86_64, as LibTrueHD's and LibDovi's do,
# because a generic iOS Simulator build of a consumer links both architectures; the tvOS and
# visionOS simulators stay arm64 only (x86_64-apple-tvos has no prebuilt std, and there is no
# Intel Vision Pro).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Under target/, which git and the usual sync tools already ignore.
BUILD_DIR="$SCRIPT_DIR/target/apple"
CARGO_DIR="$BUILD_DIR/cargo"
STAGE_DIR="$BUILD_DIR/xcfw_stage"
XCFW_OUT="$SCRIPT_DIR/DTSX.xcframework"
LIB=libdtsx.a

# rustup's cargo proxy (Homebrew's rustup keeps it outside ~/.cargo/bin).
for dir in /opt/homebrew/opt/rustup/bin "$HOME/.cargo/bin"; do
    [ -d "$dir" ] && PATH="$dir:$PATH"
done
export PATH
command -v rustup >/dev/null || { echo "ERROR: rustup not found (https://rustup.rs)" >&2; exit 1; }
command -v xcodebuild >/dev/null || { echo "ERROR: Xcode command line tools not found" >&2; exit 1; }

# Minimum OS versions of the consumer (AetherEngine). rustc reads these for Apple targets and
# records them in every object's LC_BUILD_VERSION.
export MACOSX_DEPLOYMENT_TARGET=15.0
export IPHONEOS_DEPLOYMENT_TARGET=18.0
export TVOS_DEPLOYMENT_TARGET=18.0
export XROS_DEPLOYMENT_TARGET=1.0

# Keep the builder's file system out of the shipped binaries: panic locations embed source paths
# (crate registry, git checkouts, this directory), which would otherwise publish the build
# machine's home directory and layout in every slice.
export RUSTFLAGS="--remap-path-prefix=$HOME=/build --remap-path-prefix=$SCRIPT_DIR=/dtsx"

cd "$SCRIPT_DIR"
echo "==> $(rustup show active-toolchain | head -1)"
rustup install >/dev/null 2>&1 || rustup install # the toolchain and targets pinned in rust-toolchain.toml
echo "==> $(cargo --version), $(rustc --version)"

TARGETS=(
    aarch64-apple-darwin x86_64-apple-darwin
    aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
    aarch64-apple-tvos aarch64-apple-tvos-sim
    aarch64-apple-visionos aarch64-apple-visionos-sim
)

# `cargo rustc --crate-type staticlib` rather than `cargo build`: Cargo.toml also lists rlib
# (for the Rust tests), and with an rlib in the mix cargo skips LTO. Built alone, the staticlib
# gets fat LTO: all Rust code, std included, in one object.
for target in "${TARGETS[@]}"; do
    echo "==> Building $target"
    cargo rustc --release --locked --lib --crate-type staticlib \
        --target "$target" --target-dir "$CARGO_DIR"
done

rm -rf "$STAGE_DIR"
# Headers live in a DTSX/ subdirectory: SwiftPM copies every binary target's headers into
# one shared include/ directory, so a module.modulemap at the root collides with any other
# xcframework that does the same (LibTrueHD and LibDovi do).
mkdir -p "$STAGE_DIR/headers/DTSX"
cp include/dtsx.h include/module.modulemap "$STAGE_DIR/headers/DTSX/"
echo '_dtsx_*' >"$STAGE_DIR/exported_symbols"

# prelink <target> <arch> <ld platform> <min OS> <sdk>: prints the path of a static library
# holding one relocatable object in which only the C API (_dtsx_*) is global. Even
# after LTO, a Rust staticlib exports rust_eh_personality and a few std symbols unmangled or
# with toolchain-wide hashes; a second Rust static library in the same app (LibTrueHD and LibDovi
# both ship one) then fails to link with duplicate symbols. `ld -r` with an exported
# symbol list turns every other global, compiler-builtins included, into a local symbol.
prelink() {
    local target="$1" arch="$2" platform="$3" minos="$4" sdk="$5"
    local work="$BUILD_DIR/prelink/$target" lib="$CARGO_DIR/$target/release/$LIB"
    rm -rf "$work"
    mkdir -p "$work/objs"
    if [ -n "$(ar -t "$lib" | sort | uniq -d)" ]; then
        echo "ERROR: duplicate member names in $lib; ar -x would lose objects" >&2
        exit 1
    fi
    (cd "$work/objs" && ar -x "$lib" && rm -f __.SYMDEF*)
    ld -r -arch "$arch" \
        -platform_version "$platform" "$minos" "$(xcrun --sdk "$sdk" --show-sdk-version)" \
        -exported_symbols_list "$STAGE_DIR/exported_symbols" \
        -o "$work/DTSX.o" "$work"/objs/*.o 2> >(grep -v 'built for newer' >&2)
    # -S drops debug sections only; the exported entry points keep their names.
    strip -S "$work/DTSX.o"
    libtool -static -o "$work/$LIB" "$work/DTSX.o" 2>/dev/null
    echo "$work/$LIB"
}

# stage_slice <name> <lib>...: one library per slice, lipo'd when given two.
stage_slice() {
    local name="$1"
    shift
    mkdir -p "$STAGE_DIR/$name"
    if [ "$#" -gt 1 ]; then
        lipo -create "$@" -output "$STAGE_DIR/$name/$LIB"
    else
        cp "$1" "$STAGE_DIR/$name/$LIB"
    fi
}

stage_slice macos \
    "$(prelink aarch64-apple-darwin arm64 macos "$MACOSX_DEPLOYMENT_TARGET" macosx)" \
    "$(prelink x86_64-apple-darwin x86_64 macos "$MACOSX_DEPLOYMENT_TARGET" macosx)"
stage_slice ios "$(prelink aarch64-apple-ios arm64 ios "$IPHONEOS_DEPLOYMENT_TARGET" iphoneos)"
stage_slice ios-sim \
    "$(prelink aarch64-apple-ios-sim arm64 ios-simulator "$IPHONEOS_DEPLOYMENT_TARGET" iphonesimulator)" \
    "$(prelink x86_64-apple-ios x86_64 ios-simulator "$IPHONEOS_DEPLOYMENT_TARGET" iphonesimulator)"
stage_slice tvos "$(prelink aarch64-apple-tvos arm64 tvos "$TVOS_DEPLOYMENT_TARGET" appletvos)"
stage_slice tvos-sim \
    "$(prelink aarch64-apple-tvos-sim arm64 tvos-simulator "$TVOS_DEPLOYMENT_TARGET" appletvsimulator)"
stage_slice xros "$(prelink aarch64-apple-visionos arm64 xros "$XROS_DEPLOYMENT_TARGET" xros)"
stage_slice xros-sim \
    "$(prelink aarch64-apple-visionos-sim arm64 xros-simulator "$XROS_DEPLOYMENT_TARGET" xrsimulator)"

rm -rf "$XCFW_OUT"
echo "==> Assembling DTSX.xcframework"
args=()
for slice in macos ios ios-sim tvos tvos-sim xros xros-sim; do
    args+=(-library "$STAGE_DIR/$slice/$LIB" -headers "$STAGE_DIR/headers")
done
xcodebuild -create-xcframework "${args[@]}" -output "$XCFW_OUT" >/dev/null

echo "==> Done: $XCFW_OUT"
for dir in "$XCFW_OUT"/*/; do
    lib="$dir$LIB"
    [ -f "$lib" ] || continue
    printf '  %-28s %6s  %s\n' "$(basename "$dir")" "$(du -h "$lib" | cut -f1)" \
        "$(lipo -archs "$lib")"
done
echo "==> Global symbols (every slice exports exactly these):"
nm -gU "$XCFW_OUT/tvos-arm64/$LIB" | awk 'NF == 3 {print "  " $3}'
