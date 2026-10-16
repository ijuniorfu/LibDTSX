#!/usr/bin/env bash
# build.sh - build DTSX.xcframework (dynamic framework + C header + module map) from scratch.
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
# Every slice is a dynamic framework (DTSX.framework, install name @rpath/...), embedded by Xcode,
# never a static library: the `dca` decoder is a port of FFmpeg's LGPL-2.1-or-later dca decoder,
# and LGPL section 6 lets an app link it closed source only if a user can swap in a modified
# library (AetherEngine#798). That is the shape FFmpegBuild ships for the same reason.
#
# Requirements: rustup (the toolchain and targets pinned in rust-toolchain.toml are installed
# on demand), Xcode command line tools (lipo, strip, dsymutil, codesign, xcodebuild). The decoder crate `dca`
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
DYLIB=libdtsx.dylib
FW=DTSX
# The framework bundle version, the release this build is tagged as.
VERSION=2.0.0

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
# Line tables, so a crash inside the library symbolicates: rustc writes a .dSYM next to each
# dylib (split-debuginfo=packed, which cargo does not default to), which rides along in the
# xcframework for every device slice, and the shipped binary is stripped of local symbols.
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed

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

# `cargo rustc --crate-type cdylib` rather than `cargo build`: Cargo.toml lists staticlib and rlib
# (the rlib for the Rust tests), and with an rlib in the mix cargo skips LTO. Built alone, the
# cdylib gets fat LTO, and rustc exports only the `#[no_mangle]` C API from it: rust_eh_personality
# and std stay inside this image, so the library neither collides with another Rust library in the
# app nor adds a personality routine to the host's image (AetherEngine#752 cannot happen here).
for target in "${TARGETS[@]}"; do
    echo "==> Building $target"
    cargo rustc --release --locked --lib --crate-type cdylib \
        --target "$target" --target-dir "$CARGO_DIR" \
        -- -C link-arg=-Wl,-headerpad_max_install_names
done

rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

# make_framework <slice> <platform> <min OS> <CFBundleSupportedPlatforms> <target>...: one
# framework bundle per slice, its binary lipo'd from the targets given, and for a device slice a
# matching dSYM. macOS gets the versioned (deep) bundle, the other platforms the shallow one.
make_framework() {
    local slice="$1" platform="$2" minos="$3" supported="$4"
    shift 4
    local fw="$STAGE_DIR/$slice/$FW.framework" dsym="$STAGE_DIR/$slice/$FW.framework.dSYM"
    local root="$fw" install="@rpath/$FW.framework/$FW"
    if [ "$platform" = macos ]; then
        root="$fw/Versions/A"
        install="@rpath/$FW.framework/Versions/A/$FW"
    fi
    mkdir -p "$root/Headers" "$root/Modules"

    local bins=() dwarfs=() target
    for target in "$@"; do
        bins+=("$CARGO_DIR/$target/release/$DYLIB")
        dwarfs+=("$CARGO_DIR/$target/release/$DYLIB.dSYM/Contents/Resources/DWARF/$DYLIB")
    done
    lipo -create "${bins[@]}" -output "$root/$FW"
    # install_name_tool, strip and codesign leave LC_UUID alone, so the dSYM still pairs with it.
    install_name_tool -id "$install" "$root/$FW"
    strip -x "$root/$FW"

    cp include/dtsx.h "$root/Headers/"
    cp include/module.modulemap "$root/Modules/"

    local plist="$root/Info.plist"
    [ "$platform" = macos ] && { mkdir -p "$root/Resources"; plist="$root/Resources/Info.plist"; }
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>$FW</string>
<key>CFBundleIdentifier</key><string>com.aetherengine.$FW</string>
<key>CFBundleName</key><string>$FW</string>
<key>CFBundleVersion</key><string>$VERSION</string>
<key>CFBundleShortVersionString</key><string>$VERSION</string>
<key>CFBundlePackageType</key><string>FMWK</string>
<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
<key>CFBundleSupportedPlatforms</key><array><string>$supported</string></array>
<key>MinimumOSVersion</key><string>$minos</string>
</dict></plist>
EOF

    if [ "$platform" = macos ]; then
        ln -s A "$fw/Versions/Current"
        local item
        for item in "$FW" Headers Modules Resources; do ln -s "Versions/Current/$item" "$fw/$item"; done
    fi
    codesign --force --sign - "$fw" 2>/dev/null

    case "$slice" in *simulator*) return ;; esac
    mkdir -p "$dsym/Contents/Resources/DWARF"
    lipo -create "${dwarfs[@]}" -output "$dsym/Contents/Resources/DWARF/$FW"
    cat >"$dsym/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleDevelopmentRegion</key><string>English</string>
<key>CFBundleIdentifier</key><string>com.apple.xcode.dsym.com.aetherengine.$FW</string>
<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
<key>CFBundlePackageType</key><string>dSYM</string>
<key>CFBundleSignature</key><string>????</string>
<key>CFBundleShortVersionString</key><string>$VERSION</string>
<key>CFBundleVersion</key><string>$VERSION</string>
</dict></plist>
EOF
}

SLICES=(
    "macos-arm64_x86_64 macos $MACOSX_DEPLOYMENT_TARGET MacOSX aarch64-apple-darwin x86_64-apple-darwin"
    "ios-arm64 ios $IPHONEOS_DEPLOYMENT_TARGET iPhoneOS aarch64-apple-ios"
    "ios-arm64_x86_64-simulator ios $IPHONEOS_DEPLOYMENT_TARGET iPhoneSimulator aarch64-apple-ios-sim x86_64-apple-ios"
    "tvos-arm64 tvos $TVOS_DEPLOYMENT_TARGET AppleTVOS aarch64-apple-tvos"
    "tvos-arm64-simulator tvos $TVOS_DEPLOYMENT_TARGET AppleTVSimulator aarch64-apple-tvos-sim"
    "xros-arm64 xros $XROS_DEPLOYMENT_TARGET XROS aarch64-apple-visionos"
    "xros-arm64-simulator xros $XROS_DEPLOYMENT_TARGET XRSimulator aarch64-apple-visionos-sim"
)
args=()
for spec in "${SLICES[@]}"; do
    read -r -a fields <<<"$spec"
    make_framework "${fields[@]}"
    args+=(-framework "$STAGE_DIR/${fields[0]}/$FW.framework")
    [ -d "$STAGE_DIR/${fields[0]}/$FW.framework.dSYM" ] &&
        args+=(-debug-symbols "$STAGE_DIR/${fields[0]}/$FW.framework.dSYM")
done

rm -rf "$XCFW_OUT"
echo "==> Assembling DTSX.xcframework"
xcodebuild -create-xcframework "${args[@]}" -output "$XCFW_OUT" >/dev/null

echo "==> Done: $XCFW_OUT"
for spec in "${SLICES[@]}"; do
    read -r slice platform _ <<<"$spec"
    bin="$XCFW_OUT/$slice/$FW.framework/$FW"
    printf '  %-28s %6s  %s\n' "$slice" "$(du -hL "$bin" | cut -f1)" "$(lipo -archs "$bin")"
    # Exactly the C API is exported, under an @rpath install name, from every architecture.
    for arch in $(lipo -archs "$bin"); do
        exports="$(nm -gU -arch "$arch" "$bin" | awk 'NF == 3 {print $3}')"
        if grep -qv '^_dtsx_' <<<"$exports"; then
            echo "ERROR: $slice $arch exports more than the C API:" >&2
            grep -v '^_dtsx_' <<<"$exports" >&2
            exit 1
        fi
    done
    id="$(otool -D "$bin" | tail -1)"
    case "$id" in @rpath/$FW.framework/*) ;; *) echo "ERROR: $slice install name is $id" >&2; exit 1 ;; esac
done
echo "==> Exported symbols (every slice exports exactly these):"
nm -gU "$XCFW_OUT/tvos-arm64/$FW.framework/$FW" | awk 'NF == 3 {print "  " $3}'
