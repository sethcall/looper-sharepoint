# cicd-secrets/

The signing and notarization secrets for release builds. **Only this README is in git**: everything else in this
folder is ignored (`.gitignore`). Copies of these secrets are kept in GitHub, in the repository's
Settings → Environments → `release`. GitHub never shows a secret again once it's saved, so this folder is the only
readable copy. See [docs/packaging.md](../docs/packaging.md), "Releases from GitHub".

| File | What it is | Uploaded as |
|---|---|---|
| `WDTD322L33.p12` | The "Developer ID Application: Michael Call (WDTD322L33)" identity: the certificate **and its private key**, exported from Keychain Access with a password | `MACOS_CERTIFICATE_P12_BASE64`, `MACOS_CERTIFICATE_PASSWORD` |
| `AuthKey_<KEYID>.p8` | An App Store Connect API key for `notarytool`. The key ID is in the file name | `NOTARY_KEY_P8`, `NOTARY_KEY_ID` |

The `.p12`'s password and the API key's **Issuer ID** aren't kept here. Keep them in a password manager. The Issuer
ID is shown in App Store Connect above the list of keys; it is the same for every key of the team.

Never use JamKazam's team (`HXL4V4VGHK`) for any of these. The upload script refuses it.

## Upload (or upload again) to GitHub

```sh
just release-secrets cicd-secrets/WDTD322L33.p12 cicd-secrets/AuthKey_<KEYID>.p8 <issuer-id>
```

The script asks for the `.p12`'s password. It checks the file is the WDTD322L33 Developer ID with its private key,
then uploads all five secrets with `gh` (sign in first with `gh auth login`).

## Recreating from scratch

If this folder is lost, start here.

1. **Developer ID certificate.** The private key only exists where the certificate request was made.
   - **If a Mac still has it in its keychain:** in Keychain Access → login → My Certificates, select
     "Developer ID Application: Michael Call (WDTD322L33)". Use File → Export Items… → `.p12`, with a new strong
     password, and save it here as `WDTD322L33.p12`.
   - **Otherwise:** make a new one.
     - In Keychain Access, use Certificate Assistant → Request a Certificate From a Certificate Authority… (saved to
       disk).
     - At developer.apple.com → Certificates, sign in with the WDTD322L33 account. Create a "Developer ID
       Application" certificate from that request, download it, and open it so it joins the keychain.
     - Export it as above. Revoking the old certificate isn't needed, but it's good practice if the key was lost
       somewhere unsafe. Apps already signed with it keep working unless it's revoked.
2. **API key.** Apple only lets you download a `.p8` once, so a lost one is replaced, not recovered.
   - In App Store Connect → Users and Access → Integrations → App Store Connect API, revoke the old key.
   - Generate a new team key with the **Developer** role and download `AuthKey_<KEYID>.p8` into this folder. Keep its
     original name.
   - Note the Issuer ID.
3. **Upload** with `just release-secrets` (above). Then check with a pre-release tag:
   `git tag v<version>-rc.1 && git push origin v<version>-rc.1`.

For local signed builds, the certificate also needs to be in the login keychain: double-click the `.p12`. Local
notarization uses `just notary-setup <apple-id>`, or this API key through `NOTARY_KEY`, `NOTARY_KEY_ID` and
`NOTARY_ISSUER`.
