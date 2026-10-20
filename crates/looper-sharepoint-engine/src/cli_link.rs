//! Put the `looper-sharepoint` CLI on the PATH as a symlink to the copy inside the app, for all users
//! (`/usr/local/bin`) or just this user (`~/.local/bin`). Shared by the CLI (`install-cli`) and the tray.
//! See `specs/install-cli-design.md`.
//!
//! The decisions are pure functions over paths; the few side effects (linking, running a login shell, editing a shell
//! startup file, elevating) are small functions the hosts call.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const NAME: &str = "looper-sharepoint";
pub const SYSTEM_DIR: &str = "/usr/local/bin";
const RECORD: &str = "cli-link.json";
/// The executable inside the app bundle, as found at the end of a link's target.
const IN_BUNDLE: &str = ".app/Contents/MacOS/looper-sharepoint";
const BUNDLE_NAME: &str = "Looper SharePoint.app";
pub const PATH_LINE: &str = r#"export PATH="$HOME/.local/bin:$PATH""#;
const BLOCK_START: &str = "# >>> looper-sharepoint >>>";
const BLOCK_END: &str = "# <<< looper-sharepoint <<<";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// `/usr/local/bin`, for every user.
    System,
    /// `~/.local/bin`, for this user.
    User,
}

impl Scope {
    pub fn dir(self, home: &Path) -> PathBuf {
        match self {
            Scope::System => PathBuf::from(SYSTEM_DIR),
            Scope::User => home.join(".local/bin"),
        }
    }

    pub fn link(self, home: &Path) -> PathBuf {
        self.dir(home).join(NAME)
    }
}

/// The user's home folder.
pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).context("HOME isn't set")
}

// ---- finding the CLI ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    /// Installed somewhere permanent.
    Installed,
    /// A development build (`target/release` or `target/debug`).
    Dev,
    /// Inside a mounted disk image.
    DiskImage,
    /// A random read-only copy macOS runs a quarantined app from.
    Translocated,
}

pub fn location(cli: &Path) -> Location {
    let s = cli.to_string_lossy();
    if s.contains("/AppTranslocation/") {
        Location::Translocated
    } else if s.starts_with("/Volumes/") {
        Location::DiskImage
    } else if s.contains("/target/release/") || s.contains("/target/debug/") {
        Location::Dev
    } else {
        Location::Installed
    }
}

/// Can `cli` be linked to? Why not, if not.
pub fn check_cli(cli: &Path) -> std::result::Result<Location, String> {
    match location(cli) {
        Location::DiskImage => {
            return Err("the app is running from its disk image. Drag it to Applications first, then open it from there".into());
        }
        Location::Translocated => {
            return Err("macOS is running the app from a temporary copy (it was opened straight from Downloads). Move it \
                        to Applications (or ~/Applications) and open it again"
                .into());
        }
        _ => {}
    }
    if !is_executable(cli) {
        return Err(format!("{} isn't there, or isn't executable", cli.display()));
    }
    Ok(location(cli))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

// ---- what's at the link path ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Existing {
    Nothing,
    /// Already a link to this CLI.
    ThisCli,
    /// A link we may replace: recorded as ours, or pointing at a CLI inside a Looper SharePoint app.
    Ours(PathBuf),
    /// A link to something else.
    OtherLink(PathBuf),
    /// A regular file or folder (e.g. from `cargo install`).
    NotALink,
}

pub fn inspect(link: &Path, cli: &Path, record: Option<&Record>) -> Existing {
    let Ok(md) = std::fs::symlink_metadata(link) else { return Existing::Nothing };
    if !md.file_type().is_symlink() {
        return Existing::NotALink;
    }
    let Ok(target) = std::fs::read_link(link) else { return Existing::NotALink };
    let resolved = std::fs::canonicalize(link).ok();
    if target == cli || resolved.as_deref() == Some(cli) {
        return Existing::ThisCli;
    }
    if record.is_some_and(|r| r.link == link && r.target == target) || is_app_cli(&target) {
        Existing::Ours(target)
    } else {
        Existing::OtherLink(target)
    }
}

/// A `looper-sharepoint` inside a Looper SharePoint app bundle.
fn is_app_cli(target: &Path) -> bool {
    let s = target.to_string_lossy();
    s.ends_with(IN_BUNDLE) && s.contains(BUNDLE_NAME)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Create,
    Replace,
    AlreadyInstalled,
    Stop(String),
}

pub fn decide(existing: &Existing, link: &Path, force: bool) -> Action {
    match existing {
        Existing::Nothing => Action::Create,
        Existing::ThisCli => Action::AlreadyInstalled,
        Existing::Ours(_) => Action::Replace,
        Existing::OtherLink(_) if force => Action::Replace,
        Existing::OtherLink(t) => Action::Stop(format!(
            "{} already links to {}. Left as it is (the CLI can replace a link with --force)",
            link.display(),
            t.display()
        )),
        Existing::NotALink => Action::Stop(format!(
            "{} already exists and isn't a link (perhaps from `cargo install`). Left as it is",
            link.display()
        )),
    }
}

/// Scope when none is asked for: all users if that needs no elevation, else just this user.
pub fn default_scope() -> Scope {
    if writable(Path::new(SYSTEM_DIR)) { Scope::System } else { Scope::User }
}

/// Can this user create files in `dir`? (A missing folder counts if its parent is writable.)
pub fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let d = if dir.exists() { dir } else { dir.parent().unwrap_or(dir) };
    let Ok(c) = std::ffi::CString::new(d.as_os_str().as_bytes()) else { return false };
    // SAFETY: access() reads a NUL-terminated path and has no other effect.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

// ---- making and removing links ----

/// Create or replace the link without elevation: `mkdir -p`, then a new link renamed over the old one.
pub fn link_unprivileged(cli: &Path, link: &Path) -> Result<()> {
    let dir = link.parent().context("link has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{NAME}.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(cli, &tmp).with_context(|| format!("creating a link in {}", dir.display()))?;
    std::fs::rename(&tmp, link).with_context(|| format!("putting the link at {}", link.display()))
}

/// The commands that create the link with elevation. Nothing else runs as root.
pub fn privileged_link_commands(cli: &Path, link: &Path) -> Vec<Vec<String>> {
    let dir = link.parent().unwrap_or(Path::new(SYSTEM_DIR));
    vec![
        vec!["/bin/mkdir".into(), "-p".into(), dir.display().to_string()],
        vec!["/bin/ln".into(), "-sfn".into(), cli.display().to_string(), link.display().to_string()],
    ]
}

pub fn privileged_remove_commands(link: &Path) -> Vec<Vec<String>> {
    vec![vec!["/bin/rm".into(), "-f".into(), link.display().to_string()]]
}

/// Run commands with `sudo`, prompting in the terminal (CLI).
pub fn run_with_sudo(cmds: &[Vec<String>]) -> Result<()> {
    for c in cmds {
        let ok = Command::new("/usr/bin/sudo").args(c).status().context("running sudo")?.success();
        if !ok {
            bail!("`sudo {}` failed. If you're not an administrator, use --user instead", c.join(" "));
        }
    }
    Ok(())
}

/// Run commands with macOS's administrator prompt (password or Touch ID) (tray). `Ok(false)`: cancelled.
pub fn run_with_admin_prompt(cmds: &[Vec<String>], why: &str) -> Result<bool> {
    let script = cmds.iter().map(|c| c.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join(" && ");
    let apple = format!(
        "do shell script \"{}\" with prompt \"{}\" with administrator privileges",
        applescript_escape(&script),
        applescript_escape(why)
    );
    let out = Command::new("/usr/bin/osascript").arg("-e").arg(&apple).output().context("running osascript")?;
    if out.status.success() {
        return Ok(true);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // -128: the user cancelled.
    if err.contains("(-128)") {
        return Ok(false);
    }
    bail!("{}", err.trim())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

// ---- ownership record ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub link: PathBuf,
    pub target: PathBuf,
    pub scope: Scope,
    pub installed_at: String,
    /// The shell startup file we added `~/.local/bin` to, if any.
    #[serde(default)]
    pub path_edit: Option<PathBuf>,
}

/// Is the recorded link still ours (a link to what we recorded, or to a CLI in a Looper SharePoint app)?
pub fn still_ours(r: &Record) -> bool {
    std::fs::symlink_metadata(&r.link).is_ok_and(|m| m.file_type().is_symlink())
        && std::fs::read_link(&r.link).is_ok_and(|t| t == r.target || is_app_cli(&t))
}

pub fn load_record(data_dir: &Path) -> Option<Record> {
    let s = std::fs::read_to_string(data_dir.join(RECORD)).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save_record(data_dir: &Path, r: &Record) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(data_dir.join(RECORD), serde_json::to_string_pretty(r)?)?;
    Ok(())
}

pub fn clear_record(data_dir: &Path) -> Result<()> {
    match std::fs::remove_file(data_dir.join(RECORD)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ---- repair at launch ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repair {
    /// Nothing to do: it points at a working CLI.
    Fine,
    /// Our link points at a CLI that's gone: point it at this one.
    Repoint,
    /// As `Repoint`, but that needs elevation: offer Repair Command Line Tool… in the menu, never prompt at launch.
    NeedsAdmin,
    /// The link is gone or was replaced by something else: forget the record, touch nothing.
    Forget,
}

pub fn repair(record: &Record) -> Repair {
    let link = &record.link;
    let Ok(md) = std::fs::symlink_metadata(link) else { return Repair::Forget };
    let Ok(target) = std::fs::read_link(link) else { return Repair::Forget };
    if !md.file_type().is_symlink() || (target != record.target && !is_app_cli(&target)) {
        return Repair::Forget;
    }
    if is_executable(&target) {
        // Ours and working, possibly another copy of the app: one copy owns the command.
        return Repair::Fine;
    }
    match record.scope {
        Scope::User => Repair::Repoint,
        Scope::System if writable(link.parent().unwrap_or(link)) => Repair::Repoint,
        Scope::System => Repair::NeedsAdmin,
    }
}

// ---- the user's PATH ----

/// The PATH a new terminal gets: asks the login shell, as apps started by launchd have a minimal PATH. `None` if
/// the shell didn't answer within 3 s.
pub fn login_shell_path(shell: &str) -> Option<String> {
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", r#"printf '<<%s>>' "$PATH""#])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    parse_marked(&out)
}

/// The text between `<<` and `>>`: interactive startup files may print other things around it.
pub fn parse_marked(out: &str) -> Option<String> {
    let start = out.rfind("<<")? + 2;
    let end = start + out[start..].find(">>")?;
    Some(out[start..end].to_string())
}

pub fn path_has(path: &str, dir: &Path) -> bool {
    path.split(':').any(|p| Path::new(p) == dir)
}

/// The first `looper-sharepoint` on `path`.
pub fn which(path: &str) -> Option<PathBuf> {
    path.split(':').filter(|p| !p.is_empty()).map(|p| Path::new(p).join(NAME)).find(|c| is_executable(c))
}

/// Another `looper-sharepoint` that a new terminal would run instead of `link`.
pub fn shadowed_by(path: &str, link: &Path) -> Option<PathBuf> {
    let first = which(path)?;
    let same = first == link || std::fs::canonicalize(&first).ok() == std::fs::canonicalize(link).ok();
    (!same).then_some(first)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathFix {
    /// Append [`PATH_LINE`] to this file, between markers.
    File(PathBuf),
    /// `fish -c 'fish_add_path -U ~/.local/bin'`.
    Fish,
    /// Unknown shell: show the line to add by hand.
    Manual,
}

/// Login-shell files, since macOS Terminal opens login shells.
pub fn path_fix(shell: &str, home: &Path) -> PathFix {
    match Path::new(shell).file_name().and_then(|n| n.to_str()) {
        Some("zsh") => PathFix::File(home.join(".zprofile")),
        Some("bash") => PathFix::File(home.join(".bash_profile")),
        Some("fish") => PathFix::Fish,
        _ => PathFix::Manual,
    }
}

/// Append the marked PATH block to `file`. `false`: already there (the markers or the exact line), nothing added.
pub fn add_path_block(file: &Path) -> Result<bool> {
    let current = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    if current.contains(BLOCK_START) || current.lines().any(|l| l.trim() == PATH_LINE) {
        return Ok(false);
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file).with_context(|| format!("opening {}", file.display()))?;
    let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
    write!(f, "{sep}{BLOCK_START}\n{PATH_LINE}\n{BLOCK_END}\n")?;
    Ok(true)
}

/// Remove the marked block from `file`, and nothing else. `false`: it wasn't there.
pub fn remove_path_block(file: &Path) -> Result<bool> {
    let Ok(current) = std::fs::read_to_string(file) else { return Ok(false) };
    let Some(start) = current.find(BLOCK_START) else { return Ok(false) };
    let Some(end_rel) = current[start..].find(BLOCK_END) else { return Ok(false) };
    let mut end = start + end_rel + BLOCK_END.len();
    if current[end..].starts_with('\n') {
        end += 1;
    }
    std::fs::write(file, format!("{}{}", &current[..start], &current[end..]))?;
    Ok(true)
}

pub fn fish_add_path() -> Result<()> {
    let ok = Command::new("fish").args(["-c", "fish_add_path -U ~/.local/bin"]).status().context("running fish")?.success();
    if !ok {
        bail!("fish_add_path failed");
    }
    Ok(())
}

/// The user's login shell: `$SHELL`, else the account's shell (apps started by launchd may not have `$SHELL`).
pub fn user_shell() -> String {
    if let Some(s) = std::env::var("SHELL").ok().filter(|s| !s.is_empty()) {
        return s;
    }
    // SAFETY: getpwuid returns a pointer into static storage (or null); it's read at once, on this thread.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            return std::ffi::CStr::from_ptr((*pw).pw_shell).to_string_lossy().into_owned();
        }
    }
    "/bin/zsh".into()
}

/// In the `admin` group (so "all users" is offered).
pub fn is_admin() -> bool {
    Command::new("/usr/bin/id").arg("-Gn").output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().any(|g| g == "admin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn locations() {
        assert_eq!(location(Path::new("/Volumes/Looper SharePoint/Looper SharePoint.app/Contents/MacOS/looper-sharepoint")), Location::DiskImage);
        assert_eq!(location(Path::new("/private/var/folders/x/AppTranslocation/ABC/d/Looper SharePoint.app/Contents/MacOS/looper-sharepoint")), Location::Translocated);
        assert_eq!(location(Path::new("/Users/a/src/target/release/looper-sharepoint")), Location::Dev);
        assert_eq!(location(Path::new("/Applications/Looper SharePoint.app/Contents/MacOS/looper-sharepoint")), Location::Installed);
        assert!(check_cli(Path::new("/Volumes/X/looper-sharepoint")).unwrap_err().contains("disk image"));
    }

    #[test]
    fn existing_and_actions() {
        let t = tempfile::tempdir().unwrap();
        let cli = t.path().join("A/Looper SharePoint.app/Contents/MacOS/looper-sharepoint");
        let old = t.path().join("B/Looper SharePoint.app/Contents/MacOS/looper-sharepoint");
        exe(&cli);
        let link = t.path().join("bin/looper-sharepoint");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();

        assert_eq!(inspect(&link, &cli, None), Existing::Nothing);
        assert_eq!(decide(&Existing::Nothing, &link, false), Action::Create);

        std::os::unix::fs::symlink(&cli, &link).unwrap();
        assert_eq!(inspect(&link, &cli, None), Existing::ThisCli);

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&old, &link).unwrap();
        assert_eq!(inspect(&link, &cli, None), Existing::Ours(old.clone()), "another app copy's CLI, even unrecorded");
        assert_eq!(decide(&Existing::Ours(old), &link, false), Action::Replace);

        std::fs::remove_file(&link).unwrap();
        let other = t.path().join("elsewhere/looper-sharepoint");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_eq!(inspect(&link, &cli, None), Existing::OtherLink(other.clone()));
        let rec = Record { link: link.clone(), target: other.clone(), scope: Scope::User, installed_at: now(), path_edit: None };
        assert_eq!(inspect(&link, &cli, Some(&rec)), Existing::Ours(other.clone()), "recorded as ours");
        assert!(matches!(decide(&Existing::OtherLink(other.clone()), &link, false), Action::Stop(_)));
        assert_eq!(decide(&Existing::OtherLink(other), &link, true), Action::Replace);

        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "binary").unwrap();
        assert_eq!(inspect(&link, &cli, None), Existing::NotALink);
        assert!(matches!(decide(&Existing::NotALink, &link, true), Action::Stop(_)), "--force never replaces a file");
    }

    #[test]
    fn links_and_repairs() {
        let t = tempfile::tempdir().unwrap();
        let cli = t.path().join("A/Looper SharePoint.app/Contents/MacOS/looper-sharepoint");
        exe(&cli);
        let link = t.path().join("home/.local/bin/looper-sharepoint");
        link_unprivileged(&cli, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), cli);
        link_unprivileged(&cli, &link).unwrap(); // replacing works too

        let rec = Record { link: link.clone(), target: cli.clone(), scope: Scope::User, installed_at: now(), path_edit: None };
        assert_eq!(repair(&rec), Repair::Fine);
        std::fs::remove_file(&cli).unwrap(); // the app moved
        assert_eq!(repair(&rec), Repair::Repoint);
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "someone else's").unwrap();
        assert_eq!(repair(&rec), Repair::Forget);
        std::fs::remove_file(&link).unwrap();
        assert_eq!(repair(&rec), Repair::Forget);
    }

    #[test]
    fn path_markers_and_shadowing() {
        assert_eq!(parse_marked("Welcome!\n<</usr/bin:/bin>>").as_deref(), Some("/usr/bin:/bin"));
        assert_eq!(parse_marked("nothing"), None);
        assert!(path_has("/a:/Users/x/.local/bin:/b", Path::new("/Users/x/.local/bin")));
        assert!(!path_has("/a:/b", Path::new("/Users/x/.local/bin")));

        let t = tempfile::tempdir().unwrap();
        let first = t.path().join("cargo/looper-sharepoint");
        let ours = t.path().join("local/looper-sharepoint");
        exe(&first);
        exe(&ours);
        let path = format!("{}:{}", first.parent().unwrap().display(), ours.parent().unwrap().display());
        assert_eq!(shadowed_by(&path, &ours), Some(first.clone()));
        assert_eq!(shadowed_by(&path, &first), None);
    }

    #[test]
    fn shell_block_is_idempotent_and_removable() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(path_fix("/bin/zsh", t.path()), PathFix::File(t.path().join(".zprofile")));
        assert_eq!(path_fix("/opt/homebrew/bin/fish", t.path()), PathFix::Fish);
        assert_eq!(path_fix("/bin/tcsh", t.path()), PathFix::Manual);

        let f = t.path().join(".zprofile");
        std::fs::write(&f, "eval \"$(brew shellenv)\"").unwrap();
        assert!(add_path_block(&f).unwrap());
        assert!(!add_path_block(&f).unwrap(), "second time is a no-op");
        let s = std::fs::read_to_string(&f).unwrap();
        assert_eq!(s.matches(PATH_LINE).count(), 1);
        assert!(s.starts_with("eval \"$(brew shellenv)\"\n# >>>"));
        assert!(remove_path_block(&f).unwrap());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "eval \"$(brew shellenv)\"\n");

        let g = t.path().join(".bash_profile");
        std::fs::write(&g, format!("{PATH_LINE}\n")).unwrap();
        assert!(!add_path_block(&g).unwrap(), "the exact line is already there");
    }

    #[test]
    fn admin_prompt_script_is_quoted() {
        let cmds = privileged_link_commands(Path::new("/Applications/It's.app/x"), Path::new("/usr/local/bin/looper-sharepoint"));
        let script = cmds.iter().map(|c| c.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join(" && ");
        assert_eq!(script, r"'/bin/mkdir' '-p' '/usr/local/bin' && '/bin/ln' '-sfn' '/Applications/It'\''s.app/x' '/usr/local/bin/looper-sharepoint'");
    }
}
