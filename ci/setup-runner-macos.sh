#!/bin/sh
# Sets up a Gitea Actions runner with the "macos" label on this Mac (needs Homebrew).
#   INSTANCE=https://gitea.example.com sh setup-runner-macos.sh
#                                     asks for the runner registration token (not kept in the shell history)
# Token: Gitea -> repository Settings -> Actions -> Runners -> Create new runner (a repository runner only takes
# this repository's jobs). Regenerate the token in Gitea once the runner is registered.
# The runner runs jobs as you, with your files: only let it take jobs from repositories you control.
set -e
if [ -z "$RUNNER_TOKEN" ]; then
  printf 'Runner registration token: '
  stty -echo; read -r RUNNER_TOKEN; stty echo; echo
fi
[ -n "$RUNNER_TOKEN" ] || { echo "no token"; exit 1; }
[ -n "$INSTANCE" ] || { echo "set INSTANCE to your Gitea's address, e.g. INSTANCE=https://gitea.example.com"; exit 1; }
VERSION=${VERSION:-4.1.0}
DIR="$HOME/gitea-runner"
ARCH=$(uname -m | sed 's/x86_64/amd64/')

# Job tools: Rust with clippy (Xcode command line tools provide git and the C compiler).
xcode-select -p >/dev/null 2>&1 || xcode-select --install
brew list rustup >/dev/null 2>&1 || brew install rustup
RUSTUP_BIN="$(brew --prefix rustup)/bin"
"$RUSTUP_BIN/rustup" default stable
"$RUSTUP_BIN/rustup" component add clippy

mkdir -p "$DIR"
cd "$DIR"
URL="https://dl.gitea.com/gitea-runner/$VERSION/gitea-runner-$VERSION-darwin-$ARCH"
curl -fL -o gitea-runner "$URL"
# the checksum Gitea publishes next to the download
curl -fsSL "$URL.sha256" | awk '{print $1 "  gitea-runner"}' | shasum -a 256 -c - || { echo "checksum mismatch"; rm -f gitea-runner; exit 1; }
chmod +x gitea-runner
./gitea-runner register --no-interactive --instance "$INSTANCE" --token "$RUNNER_TOKEN" --name "$(hostname -s)-macos" --labels "macos:host"
chmod 600 .runner

# Start at login via launchd. launchd has a bare PATH, so give jobs the Homebrew/Rust one (and Docker
# Desktop's /usr/local/bin for the Linux job).
PLIST="$HOME/Library/LaunchAgents/opendeck-masterhub.gitea-runner.plist"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>opendeck-masterhub.gitea-runner</string>
  <key>ProgramArguments</key><array><string>$DIR/gitea-runner</string><string>daemon</string></array>
  <key>WorkingDirectory</key><string>$DIR</string>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>$RUSTUP_BIN:$(brew --prefix)/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$DIR/runner.log</string>
  <key>StandardErrorPath</key><string>$DIR/runner.log</string>
</dict></plist>
EOF
launchctl bootout "gui/$(id -u)" "$PLIST" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "Runner registered and started (log: $DIR/runner.log). Now regenerate the registration token in Gitea."
