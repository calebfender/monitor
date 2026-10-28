#!/bin/sh
# Install Monitor as a background service for the current user (macOS or Linux).
#
#   curl -fsSL https://raw.githubusercontent.com/calebfender/monitor/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --peer "Server=https://server.example.ts.net:8620"
#
# Options it understands itself:
#   --from-source   build with cargo from this checkout instead of downloading a release
#   --uninstall     stop the service and remove it (keeps your history and disk scan)
# Every other option is passed to `monitor` (see `monitor --help`), e.g. --port, --bind, --name, --peer.
set -eu

REPO="calebfender/monitor"
DIR="$HOME/.local/share/monitor"
LABEL="io.github.calebfender.monitor"
OS=$(uname -s)
ARCH=$(uname -m)
FROM_SOURCE=0
UNINSTALL=0
ARGS=""

say() { printf '%s\n' "$*"; }
die() { printf 'monitor install: %s\n' "$*" >&2; exit 1; }
# Single-quote one argument for a shell command line.
quote() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
xml() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }

for a in "$@"; do
  case "$a" in
    --from-source) FROM_SOURCE=1 ;;
    --uninstall) UNINSTALL=1 ;;
    *) ARGS="$ARGS $(quote "$a")" ;;
  esac
done

case "$OS" in
  Darwin) PLIST="$HOME/Library/LaunchAgents/$LABEL.plist" ;;
  Linux) UNITS="$HOME/.config/systemd/user" ;;
  *) die "only macOS and Linux are supported (this is $OS)" ;;
esac

if [ "$UNINSTALL" = 1 ]; then
  if [ "$OS" = Darwin ]; then
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    rm -f "$PLIST"
  else
    systemctl --user disable --now monitor.service monitor-scan.timer 2>/dev/null || true
    rm -f "$UNITS/monitor.service" "$UNITS/monitor-scan.service" "$UNITS/monitor-scan.timer"
    systemctl --user daemon-reload
  fi
  rm -f "$DIR/monitor" "$DIR/scan.py"
  say "Removed the service. History and the disk scan are still in $DIR; delete that folder to remove them too."
  exit 0
fi

mkdir -p "$DIR"

# 1. The binary and the disk scanner
if [ "$FROM_SOURCE" = 1 ]; then
  SRC=$(cd "$(dirname "$0")" && pwd)
  [ -f "$SRC/Cargo.toml" ] || die "--from-source must be run from a checkout (./install.sh --from-source)"
  command -v cargo >/dev/null || die "cargo not found: install Rust from https://rustup.rs"
  (cd "$SRC" && cargo build --release --quiet)
  cp "$SRC/target/release/monitor" "$DIR/monitor.new"
  cp "$SRC/scan.py" "$DIR/scan.py"
else
  case "$OS-$ARCH" in
    Darwin-*) ASSET=monitor-macos ;;
    Linux-x86_64 | Linux-amd64) ASSET=monitor-linux-x86_64 ;;
    Linux-aarch64 | Linux-arm64) ASSET=monitor-linux-aarch64 ;;
    *) die "no prebuilt binary for $OS $ARCH; clone the repo and run ./install.sh --from-source" ;;
  esac
  URL="https://github.com/$REPO/releases/latest/download"
  say "Downloading $ASSET..."
  curl -fsSL "$URL/$ASSET" -o "$DIR/monitor.new" || die "download failed: $URL/$ASSET"
  curl -fsSL "$URL/scan.py" -o "$DIR/scan.py" || die "download failed: $URL/scan.py"
fi
chmod +x "$DIR/monitor.new"
"$DIR/monitor.new" --version >/dev/null || die "the downloaded binary doesn't run on this machine"
mv "$DIR/monitor.new" "$DIR/monitor"
# Check the options before they go into a service that would restart forever on a typo
eval "set -- $ARGS"
for a in "$@"; do case "$a" in --help | -h | --version | -V) die "$a isn't a service option" ;; esac; done

# 2. The service
if [ "$OS" = Darwin ]; then
  mkdir -p "$(dirname "$PLIST")"
  {
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
    printf '<plist version="1.0">\n<dict>\n\t<key>Label</key>\n\t<string>%s</string>\n\t<key>ProgramArguments</key>\n\t<array>\n' "$LABEL"
    printf '\t\t<string>%s</string>\n' "$(xml "$DIR/monitor")"
    for a in "$@"; do printf '\t\t<string>%s</string>\n' "$(xml "$a")"; done
    printf '\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n\t<key>Nice</key>\n\t<integer>5</integer>\n'
    printf '\t<key>StandardErrorPath</key>\n\t<string>%s</string>\n</dict>\n</plist>\n' "$(xml "$DIR/monitor.log")"
  } > "$PLIST"
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST"
else
  command -v systemctl >/dev/null || die "systemd not found: run $DIR/monitor yourself (it has no other dependencies)"
  mkdir -p "$UNITS"
  EXEC="$DIR/monitor"
  for a in "$@"; do EXEC="$EXEC $(printf '"%s"' "$(printf '%s' "$a" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/%/%%/g')")"; done
  cat > "$UNITS/monitor.service" <<EOF
[Unit]
Description=Monitor: live CPU, memory and per-app usage
[Service]
ExecStart=$EXEC
Restart=always
RestartSec=3
Nice=5
[Install]
WantedBy=default.target
EOF
  cat > "$UNITS/monitor-scan.service" <<EOF
[Unit]
Description=Monitor disk scan (writes data.json for the Disk tab)
[Service]
Type=oneshot
ExecStart=/usr/bin/env python3 $DIR/scan.py --dir $DIR
Nice=15
IOSchedulingClass=idle
EOF
  cat > "$UNITS/monitor-scan.timer" <<EOF
[Unit]
Description=Nightly disk scan for Monitor
[Timer]
OnCalendar=*-*-* 04:30
Persistent=true
[Install]
WantedBy=timers.target
EOF
  systemctl --user daemon-reload
  systemctl --user enable --now monitor-scan.timer >/dev/null
  systemctl --user enable monitor.service >/dev/null
  systemctl --user restart monitor.service
  # Without lingering, user services stop when you log out (matters on servers)
  if command -v loginctl >/dev/null && [ "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)" != yes ]; then
    say "Tip: run 'sudo loginctl enable-linger $(id -un)' so Monitor keeps running after you log out."
  fi
fi

# 3. Where it is
PORT=8620
prev=""
for a in "$@"; do
  case "$a" in --port=*) PORT=${a#--port=} ;; esac
  [ "$prev" = --port ] && PORT=$a
  prev=$a
done
sleep 1
if curl -fsS "http://127.0.0.1:$PORT/api/config" >/dev/null 2>&1; then
  say "Monitor is running: http://127.0.0.1:$PORT"
else
  say "Installed, but nothing answers on port $PORT yet."
  [ "$OS" = Darwin ] && say "Check $DIR/monitor.log" || say "Check: journalctl --user -u monitor -n 20"
fi
if [ "$OS" = Darwin ]; then
  say "For the Disk tab, run this from Terminal (about 2 min): python3 $DIR/scan.py"
  say "(macOS doesn't let background services read your home folder, so the scan can't run on a timer.)"
else
  say "The Disk tab fills in after the nightly scan, or now: python3 $DIR/scan.py"
fi
