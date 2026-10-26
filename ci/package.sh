#!/bin/sh
# Packs the plugin folder, with whatever binaries are in its bin/, as dist/opendeck-masterhub-<name>.zip.
# Unzip it into OpenDeck's plugins folder. VERSION (e.g. 1.2.0 or v1.2.0) sets the manifest version.
set -e
cd "$(dirname "$0")/.."
NAME=${1:?usage: package.sh <name>}
PLUGIN=com.masterhub.opendeck.sdPlugin
STAGE=$(mktemp -d)
cp -R "$PLUGIN" "$STAGE/"
if [ -n "$VERSION" ]; then
  # only a plain version reaches the manifest (and sed)
  echo "$VERSION" | grep -Eq '^v?[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' || { echo "VERSION '$VERSION' is not like v1.2.3"; exit 1; }
  sed -i.bak "s/\"Version\": \"[^\"]*\"/\"Version\": \"${VERSION#v}\"/" "$STAGE/$PLUGIN/manifest.json"
  rm "$STAGE/$PLUGIN/manifest.json.bak"
fi
mkdir -p dist
rm -f "dist/opendeck-masterhub-$NAME.zip"
(cd "$STAGE" && zip -qr - "$PLUGIN") > "dist/opendeck-masterhub-$NAME.zip"
rm -rf "$STAGE"
echo "dist/opendeck-masterhub-$NAME.zip"
