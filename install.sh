#!/usr/bin/env bash
# Builds Clueless and installs it for the current user.
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

KEY="${XDG_CONFIG_HOME:-$HOME/.config}/clueless/deepgram.key"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN="$HOME/.local/bin"

for tool in cargo parec pactl pdftotext pdfinfo gdbus pkg-config; do
    command -v "$tool" >/dev/null || { echo "Missing required program: $tool" >&2; exit 1; }
done
pkg-config --atleast-version=4.12 gtk4 || { echo "GTK 4.12 or newer is required" >&2; exit 1; }

pkg-config --exists openssl || { echo "The OpenSSL development files are required" >&2; exit 1; }

cargo build --release --locked

install -Dm755 target/release/clueless "$BIN/clueless"
mkdir -p "$DATA/applications"
cat > "$DATA/applications/io.github.clueless.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Clueless
Comment=Translucent meeting assistant
Exec="$BIN/clueless"
Icon=audio-input-microphone
Terminal=false
Categories=Utility;
StartupWMClass=io.github.clueless
EOF

echo "Installed. Start it from the application menu or run: $BIN/clueless"
echo "Sign in to your provider: claude auth login, agy acp --login, or codex login"
[ -s "$KEY" ] || [ -n "${DEEPGRAM_API_KEY:-}" ] ||
    echo "Add your Deepgram API key: (umask 077; mkdir -p '$(dirname "$KEY")'; cat > '$KEY')"
