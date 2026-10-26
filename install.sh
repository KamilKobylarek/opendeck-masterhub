#!/bin/sh
# Builds the plugin and installs it into OpenDeck (macOS / Linux, also OpenDeck from Flathub). Restart OpenDeck afterwards.
# Windows: install.ps1.
set -e
cd "$(dirname "$0")"
cargo build --release
PLUGIN=com.masterhub.opendeck.sdPlugin
TRIPLE=$(rustc -vV | sed -n 's/^host: //p') # must match a key in manifest.json CodePaths
mkdir -p "$PLUGIN/bin/$TRIPLE"
cp target/release/opendeck-masterhub "$PLUGIN/bin/$TRIPLE/"
install() {
  mkdir -p "$1"
  rm -rf "$1/$PLUGIN"
  cp -R "$PLUGIN" "$1/"
  echo "Installed to $1/$PLUGIN"
}
case "$(uname)" in
  Darwin) install "$HOME/Library/Application Support/opendeck/plugins" ;;
  *)
    install "${XDG_CONFIG_HOME:-$HOME/.config}/opendeck/plugins"
    # OpenDeck from Flathub keeps its plugins inside the sandbox
    FLATPAK="$HOME/.var/app/me.amankhanna.opendeck/config/opendeck"
    [ -d "$FLATPAK" ] && install "$FLATPAK/plugins"
    [ -f /etc/udev/rules.d/70-masterhub.rules ] || cat <<EOF

The modules need a udev rule to be opened without root (once, then replug the MasterHUB):
  sudo cp linux/70-masterhub.rules /etc/udev/rules.d/ && sudo udevadm control --reload-rules && sudo udevadm trigger
EOF
    ;;
esac
