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

## Checking a build

```sh
codesign -dvv "dist/Looper SharePoint.app"   # Authority=Developer ID Application: Michael Call (WDTD322L33)
spctl -a -vv "dist/Looper SharePoint.app"    # accepted, source=Notarized Developer ID
xcrun stapler validate dist/*.dmg
```

The app is a menu-bar app (`LSUIElement`): it has no Dock icon except while its status window is open.
