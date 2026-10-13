#!/usr/bin/env bash
# Builds everything and stages dist/app/MediaLake.app for the installer:
#   scripts/macos/build-release.sh [--version 3.0.0] [--installer]
#
# Signing (all optional; without them the app is ad-hoc signed and Gatekeeper warns on other machines):
#   MEDIALAKE_SIGN_ID            "Developer ID Application: ..."  signs the app and the nested server
#   MEDIALAKE_INSTALLER_SIGN_ID  "Developer ID Installer: ..."    signs the .pkg
#   MEDIALAKE_NOTARY_PROFILE     notarytool keychain profile      notarises and staples the .pkg
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

version="3.0.0"
installer=0
while [ $# -gt 0 ]; do
    case "$1" in
        --version) version="$2"; shift 2 ;;
        --installer) installer=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

source scripts/macos/dev-env.sh

# One server binary: llama.cpp with Metal (there is no separate CPU build on macOS). The client core is
# linked into the Swift app as a static library, so there is no dylib to embed and sign.
cargo build --release -p medialake-server
cargo rustc --release -p medialake-ffi --crate-type staticlib

ui="ui/MediaLake.Mac"
swift build -c release --package-path "$ui"
bin="$(swift build -c release --package-path "$ui" --show-bin-path)"

app="dist/app/MediaLake.app"
rm -rf dist/app
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin/MediaLake" "$app/Contents/MacOS/MediaLake"
cp target/release/medialake-server "$app/Contents/MacOS/medialake-server"
cp "$ui/Resources/Info.plist" "$app/Contents/Info.plist"
cp "$ui/Resources/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"

# Extended attributes (provenance, quarantine) become ._ files in the pkg and break strict signatures.
xattr -cr "$app"

# Nested code first, then the bundle. The hardened runtime is required for notarisation.
sign_id="${MEDIALAKE_SIGN_ID:--}"
sign=(codesign --force --options runtime --sign "$sign_id")
if [ "$sign_id" != "-" ]; then sign+=(--timestamp); fi
"${sign[@]}" "$app/Contents/MacOS/medialake-server"
"${sign[@]}" --entitlements "$ui/Resources/MediaLake.entitlements" "$app"
codesign --verify --strict "$app"
echo "staged $app ($(du -sh "$app" | cut -f1))"

if [ "$installer" = 1 ]; then
    pkg="dist/MediaLake-$version.pkg"
    work="$(mktemp -d)"
    pkgbuild --root dist/app --component-plist installer/macos/component.plist \
        --identifier com.sonvogel.medialake --version "$version" --install-location /Applications \
        --scripts installer/macos/scripts "$work/MediaLake.pkg"
    # the licence page shows the repository's LICENSE
    cp -R installer/macos/resources "$work/resources"
    cp LICENSE "$work/resources/license.txt"
    product=(productbuild --distribution installer/macos/Distribution.xml --package-path "$work" --resources "$work/resources")
    if [ -n "${MEDIALAKE_INSTALLER_SIGN_ID:-}" ]; then product+=(--sign "$MEDIALAKE_INSTALLER_SIGN_ID" --timestamp); fi
    "${product[@]}" "$pkg"
    rm -rf "$work"
    if [ -n "${MEDIALAKE_NOTARY_PROFILE:-}" ]; then
        xcrun notarytool submit "$pkg" --keychain-profile "$MEDIALAKE_NOTARY_PROFILE" --wait
        xcrun stapler staple "$pkg"
    fi
    echo "wrote $pkg"
fi
