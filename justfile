# looper-sharepoint tasks. `just` lists them.

default:
    @just --list

# Build everything (release)
build:
    cargo build --release --workspace

# Unit tests (the live SharePoint tests need .env; see README)
test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Build the tray .app and .dmg with an ad-hoc signature (runs on this Mac only; no certificate needed)
package:
    scripts/package-macos.sh --adhoc

# Build, sign with the Developer ID (team WDTD322L33), notarize and staple the .dmg for download from the web
package-and-sign:
    scripts/package-macos.sh

# Same, but skip notarization (signed only; other Macs will warn)
package-and-sign-no-notarize:
    scripts/package-macos.sh --no-notarize

# One-time: store notarization credentials in the keychain (asks for an app-specific password)
notary-setup apple_id:
    xcrun notarytool store-credentials looper-sharepoint --apple-id {{apple_id}} --team-id WDTD322L33

# Build the access request (.docx attachment + email text) into dist/ from local/access-request.env
access-request:
    scripts/access-request.py
