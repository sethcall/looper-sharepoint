# Quick start

Sync a SharePoint site, library or folder to a folder on your Mac, both ways, and keep it in sync.

## Start syncing

You need:

- the app (`Looper-SharePoint-<version>.dmg`);
- your organization's **setup link** (`looper-sharepoint://setup?…`) or **setup file** (`Contoso.looper-sharepoint`),
  from whoever set this up;
- access to the site in SharePoint (if you can open it in the browser, you can sync it).

Then:

1. **Install:** open the dmg, drag **Looper SharePoint** to Applications, and open it from there. A cloud appears in
   the menu bar.
2. **Set up:** click the setup link, or open the setup file. Looper SharePoint shows your organization and app, and
   asks before changing anything. (Or choose **Set Up…** from the menu and paste the link.)
3. **Sign in** with your work account when the browser opens.
4. **Add a site or folder:** in SharePoint, open the site, library or folder you want and copy the address from the
   browser's address bar. Choose **Add a SharePoint Folder…** from the menu, paste it, and choose a local folder: any
   new or empty folder, e.g. `~/workspace/engineering`.

That's it:

- Files download now. A folder's contents go straight into the folder you chose: no site or library names in the
  path.
- Your edits upload within seconds; changes in SharePoint arrive within about 30 seconds.
- After the first sync, Looper SharePoint starts when you log in, so this keeps working after a restart (**Start at
  Login** in the menu turns it off).
- Add more sites or folders the same way. Each gets its own folder. **Stop syncing…** in the status window never
  deletes anything.

## In Terminal

Choose **Install Command Line Tool…** in the menu first, or use a built `looper-sharepoint`. The same four steps:

```sh
looper-sharepoint setup "looper-sharepoint://setup?tenant=…&client_id=…&app_name=Contoso%20Sync"   # or the .looper-sharepoint file
looper-sharepoint login
looper-sharepoint add "https://contoso.sharepoint.com/sites/Engineering" --to ~/workspace/engineering
looper-sharepoint start-at-login on
```

More `add` examples:

```sh
# A folder: copy the address while you're in it (the browser's AllItems.aspx?id=… address works too)
looper-sharepoint add "https://contoso.sharepoint.com/sites/Engineering/Shared Documents/Specs" --to ~/workspace/specs
# A sharing link to a folder, only downloading (never uploading your changes)
looper-sharepoint add "https://contoso.sharepoint.com/:f:/s/Engineering/EaBcD…" --to ~/workspace/board --read-only
# A site with several libraries: pick one
looper-sharepoint add https://contoso.sharepoint.com/sites/Engineering --library "Design Files" --to ~/workspace/design
```

Then:

```sh
looper-sharepoint status                 # what's syncing, live or polling, the last sync and any failure
looper-sharepoint sync                   # sync now and wait
looper-sharepoint run                    # keep it in sync in this terminal (the menu-bar app does the same)
looper-sharepoint remove specs           # stop syncing one; never deletes files
```

`looper-sharepoint --help` lists everything, and each command's `--help` has examples.

## Setting this up for others

Once your admin has registered the app ([admin-setup.md](admin-setup.md)) and it works for you:

```sh
looper-sharepoint setup export --link                            # a link to paste into chat or email
looper-sharepoint setup export -o Contoso.looper-sharepoint      # or a file to attach (--with-libraries adds yours)
```

Send the dmg, the setup link (or file), and the link to the SharePoint site. Neither holds passwords or secrets: they
name your organization's tenant and its app registration. A setup link or file can't change how syncing behaves.

Chat apps and Outlook don't always make `looper-sharepoint://` links clickable. The file always works, and so does
pasting the link into **Set Up…**.

## If something's off

| What | Do |
|---|---|
| Clicking the setup link does nothing | Install and open the app first, then click it again, or paste it into **Set Up…** |
| "Approval required" when signing in | Your admin hasn't granted the app's permissions for the organization yet ([admin-setup.md](admin-setup.md)) |
| "…isn't empty" when adding | Choose a new or empty folder, or tick **Use it anyway** (`--adopt`): files only in that folder are then uploaded |
| A file appears in `not-uploaded/` | SharePoint refused it (you can only read that folder, or it's gone). Your copy is kept there; see the user guide |
| Anything else | `looper-sharepoint status`, the status window, and [user-guide.md](user-guide.md) §9 |
