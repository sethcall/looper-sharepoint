Subject: Request: approve the "{app-name}" Entra app for a SharePoint sync pilot

Hi all,

I'd like approval for one Entra application, "{app-name}", for a pilot of {pilot-users} people. It keeps chosen SharePoint libraries in sync with a local folder, which the OneDrive client can't do reliably for our local-first work: you can't make it sync now, and you can't see when it's stuck.

- It signs in as the user, with delegated permissions only, so it can reach nothing the user can't already.
- It uses only Microsoft's standard, documented APIs, following Microsoft's developer guidance. There's no secret, TLS 1.3 is required, and it talks to Microsoft hosts only.
- It respects SharePoint: deletes go to the recycle bin, version history is kept, and mass deletes are refused.
- Expected load is {typ-pct} of the app's daily SharePoint request budget.
- Source code is available to the CRO's team on request.

Entra admin: the attached document has the exact settings (about 10 minutes, or one script). Please send me the tenant ID and the client ID when it's done.

Details are attached.

Thanks,
{requester}
