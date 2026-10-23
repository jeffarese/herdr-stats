#!/bin/sh
# Put bin/herdr-stats and bin/herdr-stats-bar in place for this checkout.
#
# Herdr runs this as the plugin's [[build]] step on `herdr plugin install` and
# `herdr plugin update`. `herdr plugin link` never runs it: run it yourself for
# a local checkout. It downloads the prebuilt binaries for the manifest's
# version from the GitHub release, checks them against the release's
# SHA256SUMS, and falls back to `cargo build --release` when there is no
# binary for this machine or the download or the check fails.
#
#   HERDR_STATS_BUILD=source      skip the download and build with cargo
#   HERDR_STATS_DOWNLOAD_BASE=    release download URL to use instead of
#                                 GitHub's (the directory holding the assets)
set -u

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
cd "$ROOT" || exit 1
BIN_DIR="$ROOT/bin"
BINARIES="herdr-stats herdr-stats-bar"
REPO_URL="https://github.com/jeffarese/herdr-stats"

say() { printf 'herdr-stats: %s\n' "$*" >&2; }

version=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' herdr-plugin.toml | head -n 1)
if [ -z "$version" ]; then
    say "cannot read the version from herdr-plugin.toml"
    exit 1
fi

asset_name() {
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) return 1 ;;
    esac
    arch=$(uname -m)
    # A shell under Rosetta reports x86_64 on Apple silicon; take the native build.
    if [ "$os" = macos ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
        arch=arm64
    fi
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    printf 'herdr-stats-%s-%s.tar.gz\n' "$os" "$arch"
}

fetch() { # url destination
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 --connect-timeout 15 --max-time 300 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -T 30 --tries=2 -O "$2" "$1"
    else
        say "neither curl nor wget is available"
        return 1
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        say "neither sha256sum nor shasum is available"
        return 1
    fi
}

# Move the binaries in DIR into bin/, each in one rename, so a running daemon or
# tab bar tick never sees half a binary. The daemon notices its binary changed
# and exits; the next tick starts the new one.
install_from() { # dir
    mkdir -p "$BIN_DIR" || return 1
    for name in $BINARIES; do
        if ! cp "$1/$name" "$BIN_DIR/.$name.new.$$" || ! chmod 755 "$BIN_DIR/.$name.new.$$"; then
            rm -f "$BIN_DIR/.$name.new.$$"
            return 1
        fi
    done
    for name in $BINARIES; do
        mv -f "$BIN_DIR/.$name.new.$$" "$BIN_DIR/$name" || return 1
    done
}

download() {
    asset=$(asset_name) || {
        say "no prebuilt binaries for $(uname -s) $(uname -m)"
        return 1
    }
    base="${HERDR_STATS_DOWNLOAD_BASE:-$REPO_URL/releases/download/v$version}"
    case "$base" in
        https://* | file://*) ;;
        *) say "warning: HERDR_STATS_DOWNLOAD_BASE is not https: $base" ;;
    esac
    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t herdr-stats) || return 1
    say "downloading $asset $version"
    ok=1
    if fetch "$base/$asset" "$tmp/$asset" && fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
        expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name {print $1; exit}' "$tmp/SHA256SUMS")
        actual=$(sha256 "$tmp/$asset")
        if [ -z "$expected" ]; then
            say "SHA256SUMS has no entry for $asset"
        elif [ "$expected" != "$actual" ]; then
            say "checksum mismatch for $asset (expected $expected, got $actual)"
        elif install_checked "$tmp/$asset"; then
            ok=0
        fi
    else
        say "download failed"
    fi
    rm -rf "$tmp"
    return $ok
}

# Unpack a verified archive inside bin/ and run the binaries there rather than
# in $TMPDIR, which may be mounted noexec. Only binaries that report the
# manifest version are moved into place.
install_checked() { # archive
    mkdir -p "$BIN_DIR" || return 1
    unpack="$BIN_DIR/.unpack.$$"
    rm -rf "$unpack" && mkdir "$unpack" || return 1
    if ! tar -xzf "$1" -C "$unpack"; then
        say "could not unpack the download"
        rm -rf "$unpack"
        return 1
    fi
    for name in $BINARIES; do
        reported=$("$unpack/$name" --version 2>/dev/null) || {
            say "the downloaded $name does not run on this machine"
            rm -rf "$unpack"
            return 1
        }
        if [ "$reported" != "$name $version" ]; then
            say "downloaded $name reports '$reported', expected '$name $version'"
            rm -rf "$unpack"
            return 1
        fi
    done
    install_from "$unpack"
    status=$?
    rm -rf "$unpack"
    return $status
}

build_from_source() {
    cargo=$(command -v cargo 2>/dev/null || true)
    # herdr may run this without the login PATH that rustup sets up.
    if [ -z "$cargo" ] && [ -x "$HOME/.cargo/bin/cargo" ]; then
        cargo="$HOME/.cargo/bin/cargo"
    fi
    if [ -z "$cargo" ]; then
        say "cargo is not installed (building herdr-stats needs Rust 1.85 or newer)"
        return 1
    fi
    say "building from source with cargo"
    locked=
    [ -f Cargo.lock ] && locked=--locked
    # target/ is left in place: in a development clone it is the build cache.
    if ! "$cargo" build --release $locked; then
        say "cargo build failed (herdr-stats needs Rust 1.85 or newer)"
        return 1
    fi
    install_from "$ROOT/target/release"
}

if [ "${HERDR_STATS_BUILD:-}" != source ] && download; then
    say "installed $BIN_DIR/herdr-stats and herdr-stats-bar $version"
    exit 0
fi
if build_from_source; then
    say "installed $BIN_DIR/herdr-stats and herdr-stats-bar $version"
    exit 0
fi
say "could not install herdr-stats $version: no prebuilt binary could be downloaded"
say "and building from source failed. Install Rust 1.85 or newer"
say "(https://rustup.rs), then reinstall the plugin:"
say "  herdr plugin install jeffarese/herdr-stats"
exit 1
