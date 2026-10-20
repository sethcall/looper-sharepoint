# Access request kit

Templates for asking an organisation's Entra admin and security team to approve the app registration. They're
organisation-neutral; the values live in the gitignored `local/access-request.env`.

1. Copy `request.example.env` to `local/access-request.env` and fill it in.
2. Run `just access-request`. It needs asciidoctor and pandoc.
3. Send `dist/<org>-access-request-email.txt` as the email, with `dist/<org>-access-request.docx` attached.

| File | What it is |
|---|---|
| `attachment.adoc` | The attachment: why, security, load on SharePoint, and a section for the Entra admin |
| `email.md` | The email body |
| `create-registration.sh` | Azure CLI script the admin can run instead of clicking through |
| `request.example.env` | The values to fill in |

The load figures are computed from the license count and pilot size, using the measured costs in
[../sharepoint/throttling.md](../sharepoint/throttling.md).
