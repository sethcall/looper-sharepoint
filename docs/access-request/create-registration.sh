#!/bin/bash
# Create the Entra app registration for looper-sharepoint and grant tenant-wide admin consent.
# Run by a tenant administrator, after `az login`. Same settings as the click-through steps in the access request.
#
# Usage: create-registration.sh "<display name>"      e.g.  create-registration.sh "Contoso SharePoint Sync"
set -euo pipefail
NAME="${1:?usage: create-registration.sh \"<display name>\"}"

GRAPH=00000003-0000-0000-c000-000000000000   # Microsoft Graph
# Delegated permissions (Microsoft Graph's well-known IDs):
#   Files.ReadWrite.All, Sites.Read.All, offline_access, User.Read, openid, profile
APPID=$(az ad app create --display-name "$NAME" --sign-in-audience AzureADMyOrg \
  --is-fallback-public-client true --public-client-redirect-uris http://localhost \
  --required-resource-accesses '[{"resourceAppId":"'$GRAPH'","resourceAccess":[
    {"id":"863451e7-0667-486c-a5d6-d135439485f0","type":"Scope"},
    {"id":"205e70e5-aba6-4c52-a976-6d2d46c48043","type":"Scope"},
    {"id":"7427e0e9-2fba-42fe-b0c0-848c9e6a8182","type":"Scope"},
    {"id":"e1fe6dd8-ba31-4d61-89e7-88639da4683d","type":"Scope"},
    {"id":"37f7f235-527c-4136-accd-4a02d197296e","type":"Scope"},
    {"id":"14dad69e-099b-42c9-810b-d002981feec1","type":"Scope"}]}]' --query appId -o tsv)

SPID=$(az ad sp create --id "$APPID" --query id -o tsv)
GRAPH_SP=$(az ad sp show --id "$GRAPH" --query id -o tsv)

# Tenant-wide admin consent (the same as "Grant admin consent" in the portal).
az rest --method post --url https://graph.microsoft.com/v1.0/oauth2PermissionGrants --body \
  "{\"clientId\":\"$SPID\",\"consentType\":\"AllPrincipals\",\"resourceId\":\"$GRAPH_SP\",
    \"scope\":\"Files.ReadWrite.All Sites.Read.All offline_access User.Read openid profile\"}" >/dev/null

# Recommended: only assigned users and groups can sign in. Assign the pilot group afterwards in
# Enterprise applications → "$NAME" → Users and groups.
az ad sp update --id "$SPID" --set appRoleAssignmentRequired=true

echo "Directory (tenant) ID:   $(az account show --query tenantId -o tsv)"
echo "Application (client) ID: $APPID"
