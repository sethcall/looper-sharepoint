#!/bin/bash
# Build the macOS tray app (with the CLI inside), sign it with a Developer ID, wrap it in a .dmg, notarize and staple.
#
# Usage: scripts/package-macos.sh [--no-notarize] [--adhoc]
#   --adhoc        sign with an ad-hoc signature (no certificate; runs only on this Mac). Implies --no-notarize.
#   --no-notarize  sign but don't send to Apple (the .dmg then shows a Gatekeeper warning on other Macs).
#
# Notarization uses the keychain profile NOTARY_PROFILE (`just notary-setup`), or an App Store Connect API key when
# NOTARY_KEY (the .p8 file), NOTARY_KEY_ID and NOTARY_ISSUER are set, as in .github/workflows/release.yml.
#
# Signing is pinned to TEAM_ID. Any other team, and JamKazam's team in particular, is refused, both when picking the
# identity and when checking the signed result.
set -euo pipefail

TEAM_ID="WDTD322L33"             # Michael Call: the only team this project is signed with
FORBIDDEN_TEAMS="HXL4V4VGHK"     # JamKazam, Inc.: never sign this project with it
NOTARY_PROFILE="looper-sharepoint"
APP_NAME="Looper SharePoint"

notarize=1 adhoc=0
for a in "$@"; do
  case "$a" in
    --no-notarize) notarize=0 ;;
    --adhoc) adhoc=1; notarize=0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

cd "$(dirname "$0")/.."
step() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="looper-sharepoint-tray"))')
arch=$(uname -m)
dist="dist"
dmg="$dist/Looper-SharePoint-$version-$arch.dmg"

# --- Signing identity -------------------------------------------------------------------------------------------------
if [ $adhoc = 1 ]; then
  identity="-"
  echo "ad-hoc signing: this build runs only on this Mac"
else
  matches=$(security find-identity -v -p codesigning | grep "\"Developer ID Application: .* ($TEAM_ID)\"" || true)
  [ -n "$matches" ] || die "no Developer ID Application identity for team $TEAM_ID in the keychain (see docs/packaging.md)"
  identity=$(echo "$matches" | head -1 | awk '{print $2}')
  for t in $FORBIDDEN_TEAMS; do
    echo "$matches" | grep -q "($t)" && die "refusing: the selected identity belongs to forbidden team $t"
  done
  echo "signing as: $(echo "$matches" | head -1 | sed -E 's/^ *[0-9]+\) [0-9A-F]+ //')"
  if [ $notarize = 1 ]; then
    if [ -n "${NOTARY_KEY:-}" ]; then
      [ -f "$NOTARY_KEY" ] && [ -n "${NOTARY_KEY_ID:-}" ] && [ -n "${NOTARY_ISSUER:-}" ] ||
        die "NOTARY_KEY needs the .p8 file, NOTARY_KEY_ID and NOTARY_ISSUER"
      notary=(--key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER")
    else
      notary=(--keychain-profile "$NOTARY_PROFILE")
    fi
    xcrun notarytool history "${notary[@]}" >/dev/null 2>&1 ||
      if [ -n "${NOTARY_KEY:-}" ]; then die "notarytool refused the API key in NOTARY_KEY"
      else die "no notarytool credentials named '$NOTARY_PROFILE'. Run: just notary-setup <apple-id>  (or pass --no-notarize)"; fi
  fi
fi

# Check a signed path is signed by our team and by no forbidden one.
check_team() {
  [ $adhoc = 1 ] && return 0
  local info team
  info=$(codesign -dvv "$1" 2>&1)
  team=$(echo "$info" | sed -n 's/^TeamIdentifier=//p')
  for t in $FORBIDDEN_TEAMS; do
    echo "$info" | grep -q "$t" && die "$1 is signed by forbidden team $t"
  done
  [ "$team" = "$TEAM_ID" ] || die "$1 is signed by team '$team', expected $TEAM_ID"
}

sign() { codesign --force --timestamp --options runtime --sign "$identity" "$@"; }
[ $adhoc = 1 ] && sign() { codesign --force --sign - "$@"; }

# --- Build ------------------------------------------------------------------------------------------------------------
# Packaged builds go in their own target folder, so they never replace the development binaries in target/release
# (which keep sign-in tokens in files rather than the keychain).
export CARGO_TARGET_DIR="$PWD/target/package"
step "Building the CLI"
cargo build --release -p looper-sharepoint --features release

step "Building $APP_NAME.app (unsigned)"
(cd apps/tray && cargo tauri build --bundles app --no-sign --ci --features release)
app_src="$CARGO_TARGET_DIR/release/bundle/macos/$APP_NAME.app"
[ -d "$app_src" ] || die "the build didn't produce $app_src"

rm -rf "$dist" && mkdir -p "$dist"
app="$dist/$APP_NAME.app"
cp -R "$app_src" "$app"
cp "$CARGO_TARGET_DIR/release/looper-sharepoint" "$app/Contents/MacOS/looper-sharepoint"

# Packaged builds must keep sign-in tokens in the keychain only. The development file store's switch is compiled
# out with the `release` feature; refuse to ship a binary that still contains it.
for bin in "$app/Contents/MacOS/looper-sharepoint" "$app/Contents/MacOS/looper-sharepoint-tray"; do
  if grep -q "LOOPER_SHAREPOINT_TOKEN_STORE" "$bin"; then
    die "$bin was built without the release feature (it contains the development token file store)"
  fi
done

# --- Sign the app: nested executables first, then the bundle ----------------------------------------------------------
step "Signing the app"
sign "$app/Contents/MacOS/looper-sharepoint"
sign "$app"
codesign --verify --deep --strict --verbose=2 "$app"
check_team "$app/Contents/MacOS/looper-sharepoint"
check_team "$app"

# --- Disk image -------------------------------------------------------------------------------------------------------
step "Creating $dmg"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname "$APP_NAME" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"
sign "$dmg"
check_team "$dmg"

# --- Notarize and staple ----------------------------------------------------------------------------------------------
if [ $notarize = 1 ]; then
  step "Notarizing (this usually takes 1-5 minutes)"
  out=$(xcrun notarytool submit "$dmg" "${notary[@]}" --wait --output-format json) || true
  echo "$out"
  status=$(echo "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))' 2>/dev/null || true)
  if [ "$status" != "Accepted" ]; then
    id=$(echo "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))' 2>/dev/null || true)
    [ -n "$id" ] && xcrun notarytool log "$id" "${notary[@]}" || true
    die "notarization was not accepted (status: ${status:-unknown})"
  fi
  step "Stapling"
  xcrun stapler staple "$dmg"
  xcrun stapler staple "$app"

  step "Checking with Gatekeeper"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
  spctl --assess --type execute --verbose=2 "$app"
fi

step "Done"
echo "app: $app"
echo "dmg: $dmg"
[ $notarize = 1 ] || echo "(not notarized: other Macs will warn before opening it)"
