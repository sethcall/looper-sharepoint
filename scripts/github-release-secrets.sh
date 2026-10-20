#!/bin/bash
# Upload the signing certificate and the notarization key as secrets of the GitHub `release` environment, for
# .github/workflows/release.yml. Reads them from files and from a hidden prompt; nothing is printed or written.
#
# Usage: scripts/github-release-secrets.sh <DeveloperID.p12> <AuthKey_KEYID.p8> <issuer-id>
#   <DeveloperID.p12>   "Developer ID Application: Michael Call (WDTD322L33)" with its private key, exported from
#                       Keychain Access (asks for the password you gave it)
#   <AuthKey_KEYID.p8>  an App Store Connect API key (Users and Access → Integrations); the key ID is in its name
#   <issuer-id>         the Issuer ID shown above the list of keys
#
# Needs `gh auth login`. See docs/packaging.md, "Releases from GitHub".
set -euo pipefail

TEAM_ID="WDTD322L33"
FORBIDDEN_TEAMS="HXL4V4VGHK"
ENVIRONMENT="release"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
[ $# = 3 ] || { sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
p12=$1 p8=$2 issuer=$3
[ -f "$p12" ] || die "no such file: $p12"
[ -f "$p8" ] || die "no such file: $p8"
key_id=$(basename "$p8" | sed -nE 's/^AuthKey_([A-Z0-9]+)\.p8$/\1/p')
[ -n "$key_id" ] || die "expected the key's original name, AuthKey_<KEYID>.p8: $p8"
grep -q "BEGIN PRIVATE KEY" "$p8" || die "$p8 isn't an App Store Connect API key"
[[ "$issuer" =~ ^[0-9a-f-]{36}$ ]] || die "the issuer ID looks like 01234567-89ab-cdef-0123-456789abcdef: $issuer"
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner)

read -r -s -p "Password of $(basename "$p12"): " P12_PASSWORD; echo
export P12_PASSWORD
subjects=$(openssl pkcs12 -in "$p12" -nokeys -passin env:P12_PASSWORD 2>/dev/null ||
           openssl pkcs12 -legacy -in "$p12" -nokeys -passin env:P12_PASSWORD 2>/dev/null) ||
  die "can't open $p12 (wrong password?)"
keys=$(openssl pkcs12 -in "$p12" -nocerts -nodes -passin env:P12_PASSWORD 2>/dev/null ||
       openssl pkcs12 -legacy -in "$p12" -nocerts -nodes -passin env:P12_PASSWORD 2>/dev/null) || true
grep -q "PRIVATE KEY" <<<"$keys" || die "$p12 has no private key: export the identity, not just the certificate"
unset keys
for t in $FORBIDDEN_TEAMS; do
  grep -q "$t" <<<"$subjects" && die "refusing: $p12 contains a certificate of forbidden team $t"
done
grep -q "Developer ID Application: .*($TEAM_ID)" <<<"$subjects" ||
  die "$p12 has no \"Developer ID Application: … ($TEAM_ID)\" certificate"

echo "Uploading to $repo, environment '$ENVIRONMENT' (key ID $key_id)"
base64 -i "$p12" | gh secret set MACOS_CERTIFICATE_P12_BASE64 --env "$ENVIRONMENT" --repo "$repo"
printf '%s' "$P12_PASSWORD" | gh secret set MACOS_CERTIFICATE_PASSWORD --env "$ENVIRONMENT" --repo "$repo"
gh secret set NOTARY_KEY_P8 --env "$ENVIRONMENT" --repo "$repo" < "$p8"
printf '%s' "$key_id" | gh secret set NOTARY_KEY_ID --env "$ENVIRONMENT" --repo "$repo"
printf '%s' "$issuer" | gh secret set NOTARY_ISSUER --env "$ENVIRONMENT" --repo "$repo"
gh secret list --env "$ENVIRONMENT" --repo "$repo"
echo "Done. Keep the .p12 and .p8 (cicd-secrets/): GitHub never shows a secret again, and Apple downloads a .p8 only once."
