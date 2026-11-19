#!/bin/sh
# kroppi installer (per-user, no root needed except for system packages)
#   ./install.sh              install or update
#   ./install.sh --uninstall  remove kroppi
set -e

REPO=wagidevnull/kroppi
SHARE="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN="$HOME/.local/bin"
APPS="$SHARE/applications"
ICONS="$SHARE/icons/hicolor"

cd "$(dirname "$0")"
say()  { printf '\n\033[1;35m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

refresh_desktop() {
    command -v update-desktop-database >/dev/null && update-desktop-database -q "$APPS" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$ICONS" 2>/dev/null || true
}

if [ "$1" = "--uninstall" ]; then
    say "Removing kroppi"
    rm -rf "$SHARE/lol.wagi.kroppi"
    rm -f "$BIN/kroppi" "$APPS/kroppi.desktop" \
          "$ICONS/scalable/apps/kroppi.svg" "$ICONS/256x256/apps/kroppi.png"
    refresh_desktop
    echo "Done. (Your exported videos were left alone.)"
    exit 0
fi
[ -z "$1" ] || die "unknown option: $1"
[ "$(id -u)" -ne 0 ] || die "run this as your normal user, not root"

# ------------------------------------------------------------------ system packages
# kroppi's window is WebKitGTK; GStreamer plays the videos.
pm=""
if command -v pacman >/dev/null; then
    runtime="webkit2gtk-4.1 gst-libav gst-plugins-good ffmpeg"
    pm="sudo pacman -S --needed"
elif command -v apt-get >/dev/null; then
    runtime="libwebkit2gtk-4.1-0 gstreamer1.0-libav gstreamer1.0-plugins-good ffmpeg"
    build_deps="libwebkit2gtk-4.1-dev"
    pm="sudo apt-get install -y"
elif command -v dnf >/dev/null; then
    runtime="webkit2gtk4.1 gstreamer1-plugins-good gstreamer1-libav ffmpeg"
    build_deps="webkit2gtk4.1-devel"
    pm="sudo dnf install -y"
elif command -v zypper >/dev/null; then
    runtime="libwebkit2gtk-4_1-0 gstreamer-plugins-good gstreamer-plugins-libav ffmpeg"
    build_deps="webkit2gtk3-soup2-devel"
    pm="sudo zypper install -y"
fi

have_webkit() {
    # Debian keeps ldconfig in /sbin, which isn't on a normal user's PATH
    { ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null; } | grep -q 'libwebkit2gtk-4\.1\.so\.0'
}
have_h264() { ! command -v gst-inspect-1.0 >/dev/null || gst-inspect-1.0 avdec_h264 >/dev/null 2>&1; }

have_ffmpeg() { command -v ffmpeg >/dev/null && command -v ffprobe >/dev/null; }

if ! have_webkit || ! have_h264 || ! have_ffmpeg; then
    say "System packages"
    echo "kroppi needs WebKitGTK (its window), GStreamer plugins (to play videos) and ffmpeg (to save them)."
    if [ -n "$pm" ] && [ -t 0 ]; then
        printf 'Install them now with:\n  %s %s\n[Y/n] ' "$pm" "$runtime"
        read -r ans
        case "$ans" in
            [nN]*) ;;
            *) $pm $runtime || warn "package install failed." ;;
        esac
    elif [ -n "$pm" ]; then
        echo "Install them with:  $pm $runtime"
    else
        echo "Install WebKitGTK 4.1, the GStreamer libav + good plugins, and ffmpeg with your package manager."
    fi
fi
have_ffmpeg || warn "ffmpeg not found: kroppi needs ffmpeg and ffprobe to open and save videos."

# ------------------------------------------------------------------ get the program
# Build it if Rust and the WebKitGTK headers are here, otherwise download the
# prebuilt binary from the latest GitHub release.
fetch_binary() {
    arch=$(uname -m)
    mkdir -p target
    out=target/kroppi-$arch
    url=https://github.com/$REPO/releases/latest/download/kroppi-$arch
    say "Downloading kroppi for $arch"
    if command -v curl >/dev/null; then
        get() { curl -fsSL -o "$2" "$1"; }
    elif command -v wget >/dev/null; then
        get() { wget -qO "$2" "$1"; }
    else
        die "need cargo (to build) or curl/wget (to download)"
    fi
    get "$url" "$out" || die "no prebuilt kroppi for $arch - install Rust (cargo) and re-run to build it"
    get "$url.sha256" "$out.sha256" || die "couldn't download the checksum"
    (cd target && sed "s|  .*|  kroppi-$arch|" "kroppi-$arch.sha256" | sha256sum -c --quiet -) \
        || die "checksum mismatch - download corrupted?"
    chmod +x "$out"
    NEW_BIN=$out
}

if command -v cargo >/dev/null && pkg-config --exists webkit2gtk-4.1 2>/dev/null; then
    say "Building kroppi (the first build takes a few minutes)"
    (cd src-tauri && cargo build --release --locked)
    NEW_BIN=src-tauri/target/release/kroppi
else
    if command -v cargo >/dev/null && [ -n "$build_deps" ]; then
        echo "(To build from source instead, install $build_deps.)"
    fi
    fetch_binary
fi

# ------------------------------------------------------------------ install files
say "Installing kroppi"
mkdir -p "$BIN" "$APPS" "$ICONS/scalable/apps" "$ICONS/256x256/apps"
install -m755 "$NEW_BIN" "$BIN/kroppi"
cp icons/kroppi.svg "$ICONS/scalable/apps/kroppi.svg"
cp icons/kroppi-256.png "$ICONS/256x256/apps/kroppi.png"
# Use the full path: app launchers (e.g. ones that start apps via systemd)
# often don't have ~/.local/bin on their PATH.
sed "s|^Exec=kroppi|Exec=$BIN/kroppi|" kroppi.desktop > "$APPS/kroppi.desktop"
refresh_desktop
"$BIN/kroppi" --version

if ldd "$BIN/kroppi" 2>/dev/null | grep -q 'not found'; then
    warn "some libraries are missing, so kroppi won't start until they're installed:"
    ldd "$BIN/kroppi" | grep 'not found'
fi

case ":$PATH:" in
    *":$BIN:"*) ;;
    *) warn "$BIN is not in your PATH - add it, or launch kroppi from your app menu." ;;
esac

cat <<EOF

Done! Launch kroppi from your app menu, or run:
  kroppi               open the app
  kroppi video.mp4     open a video straight away

Uninstall: ./install.sh --uninstall
EOF
