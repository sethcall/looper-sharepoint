# Packaging the macOS app

`just package-and-sign` builds **Looper SharePoint.app** (the tray app, with the `looper-sharepoint` CLI inside at
`Contents/MacOS/looper-sharepoint`). It signs it with a Developer ID, puts it in a `.dmg`, notarizes the `.dmg` with
Apple, and staples the result. The output is in `dist/`, ready to download from the web.

The CLI inside the bundle is what **Install Command Line Tool…** (and `install-cli`) link onto the PATH; see the user
guide §2.1. It must stay next to the tray's executable, at the same version, or the menu item is disabled.

| Command | Result |
|---|---|
| `just package` | Ad-hoc signed. Runs on this Mac only; no certificate needed |
| `just package-and-sign` | Developer ID signed, notarized and stapled. Opens on any Mac without a warning |
| `just package-and-sign-no-notarize` | Developer ID signed only. Other Macs warn before opening it |

The script is [scripts/package-macos.sh](../scripts/package-macos.sh). It builds for this Mac's architecture only
(arm64 here).

## Sign-in tokens: keychain only in packaged builds

The script builds both binaries with the `release` feature.
- **Packaged builds** keep sign-in tokens only in the macOS keychain.
- **Development builds** (no `release` feature) keep them in owner-only files under the data folder's
  `dev-tokens/`. Every rebuild of an unsigned binary would otherwise trigger keychain prompts.
  `LOOPER_SHAREPOINT_TOKEN_STORE=keychain` makes a development build use the keychain.
- **A separate build folder:** packaged builds are made in `target/package/`, so they never replace the development
  binaries in `target/release/`.
- **The guard:** the `release` feature compiles the file store out. The script refuses to package a binary that
  still contains it.
- **Signing in twice:** a sign-in made with a development build isn't visible to the packaged app, and the other way
  round.

## Who signs it

The app is signed **only** by team `WDTD322L33` ("Developer ID Application: Michael Call").

- The script picks the keychain identity for that team and fails if there isn't one.
- After signing, it checks the team ID on the CLI, the app and the `.dmg`. It fails on any other team.
- It explicitly refuses JamKazam's team (`HXL4V4VGHK`), which also has signing identities on the build Mac. This
  project must never be signed with it.

## One-time setup on a build Mac

1. **The Developer ID certificate, with its private key, in the login keychain.** Check with:
   ```sh
   security find-identity -v -p codesigning | grep WDTD322L33
   ```
   - A `.cer` downloaded from developer.apple.com only works on the Mac that made its certificate request. That's
     where the private key is.
   - To set up another Mac, either export a `.p12` from Keychain Access on the original Mac, or make a new request
     on the new Mac.
   - Local copies of the certificate and request are kept in `certs/`, which is gitignored.
2. **Notarization credentials** in the keychain, under the profile name `looper-sharepoint`:
   - Create an app-specific password at https://account.apple.com → Sign-In and Security → App-Specific Passwords.
   - Then run:
     ```sh
     just notary-setup <apple-id>
     ```
     This asks for the password and stores it in the keychain, never in the repo.

## Releases from GitHub

Pushing a tag `v<version>` runs [.github/workflows/release.yml](../.github/workflows/release.yml) on a GitHub macOS
runner (arm64). It builds, signs, notarizes and staples the app with `scripts/package-macos.sh`, and attaches the
`.dmg` to a GitHub release.

```sh
git tag v0.1.0 && git push origin v0.1.0       # the tag must match the workspace version
git tag v0.1.0-rc.1 && git push origin v0.1.0-rc.1   # a pre-release
```

Each full release is marked **latest**. It carries the dmg twice, under its versioned name and as
`Looper-SharePoint.dmg`, so this link always downloads the newest one:

  https://github.com/sethcall/looper-sharepoint/releases/latest/download/Looper-SharePoint.dmg

Pre-releases (tags with a `-` suffix) are never marked latest.

[.github/workflows/ci.yml](../.github/workflows/ci.yml) runs clippy and the tests on every push and pull request. It
uses no secrets.

- **On `main`, it also packages the app unsigned** (`--adhoc`). This checks packaging and saves the build folder and
  the Tauri CLI to the Actions cache.
- **Release runs only restore those caches.** A tag's run can't read caches saved by other tags, and never saves any.
- **The Tauri CLI version** (2.12.1) is written in both workflows; change it in both.

**Where the secrets are kept.**
- The signing certificate and the notarization key are secrets of the repository's `release` **environment**.
- Only workflow runs for `v*` tags can use that environment. Pull requests, including ones from forks, never see them.
- In the runner, the certificate goes into a temporary keychain with a random password, and the `.p8` goes into the
  runner's temp folder. Both are deleted at the end of the run.
- The signing pins (team `WDTD322L33` only, never `HXL4V4VGHK`) are the same as for a local build.

| Secret | What it is |
|---|---|
| `MACOS_CERTIFICATE_P12_BASE64` | The Developer ID Application identity (certificate and private key), as a base64 `.p12` |
| `MACOS_CERTIFICATE_PASSWORD` | The `.p12`'s password |
| `NOTARY_KEY_P8` | An App Store Connect API key, used by `notarytool` instead of an Apple ID and app-specific password |
| `NOTARY_KEY_ID`, `NOTARY_ISSUER` | That key's ID and its issuer ID |

**Setting them up (once, and again when the certificate or key is replaced):**
1. **Export the identity.** In Keychain Access → login → My Certificates, select "Developer ID Application:
   Michael Call (WDTD322L33)". Export it as a `.p12` with a strong password into `cicd-secrets/` (gitignored, see its [README](../cicd-secrets/README.md)).
2. **Make an API key.** In App Store Connect → Users and Access → Integrations → App Store Connect API, create a
   team key with the **Developer** role. Download `AuthKey_<KEYID>.p8`; Apple only lets you download it once. Note
   the **Issuer ID** shown above the list of keys.
3. **Upload them.** This checks that the `.p12` is the WDTD322L33 Developer ID with its private key, asks for its
   password, and uploads all five secrets with `gh`:
   ```sh
   just release-secrets cicd-secrets/WDTD322L33.p12 cicd-secrets/AuthKey_<KEYID>.p8 <issuer-id>
   ```
4. **Keep** both files in `cicd-secrets/`: GitHub never shows a secret again.

**To revoke:**
- Revoke the API key in App Store Connect; the next release then fails at notarization.
- Replace the certificate at developer.apple.com (revoking a Developer ID certificate also affects apps already
  signed with it).
- Delete the secrets in the repository's Settings → Environments → release.

The same API key also works for local builds: set `NOTARY_KEY`, `NOTARY_KEY_ID` and `NOTARY_ISSUER` before
`just package-and-sign`.

## Checking a build

```sh
codesign -dvv "dist/Looper SharePoint.app"   # Authority=Developer ID Application: Michael Call (WDTD322L33)
spctl -a -vv "dist/Looper SharePoint.app"    # accepted, source=Notarized Developer ID
xcrun stapler validate dist/*.dmg
```

The app is a menu-bar app (`LSUIElement`): it has no Dock icon except while its status window is open.
