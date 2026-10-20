//! `install-cli` / `uninstall-cli` end to end, "just me", with HOME and the data folder in a temp directory, so the
//! real shell files and PATH are never touched. See `specs/install-cli-design.md` §9.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Command;

fn run(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_looper-sharepoint"))
        .args(args)
        .env("HOME", home)
        .env("SHELL", "/bin/zsh")
        .env("LOOPER_SHAREPOINT_DATA_DIR", home.join("data"))
        .env("LOOPER_SHAREPOINT_CONFIG", home.join("no-config.toml"))
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn install_again_and_uninstall_just_me() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let link = home.join(".local/bin/looper-sharepoint");
    let zprofile = home.join(".zprofile");
    std::fs::write(&zprofile, "# mine\n").unwrap();

    let (ok, out) = run(home, &["install-cli", "--user", "--add-to-path"]);
    assert!(ok, "{out}");
    let me = std::fs::canonicalize(env!("CARGO_BIN_EXE_looper-sharepoint")).unwrap();
    assert_eq!(std::fs::read_link(&link).unwrap(), me, "{out}");
    let profile = std::fs::read_to_string(&zprofile).unwrap();
    assert!(profile.starts_with("# mine\n# >>> looper-sharepoint >>>\n"), "{profile}");
    assert!(home.join("data/cli-link.json").exists());

    // Again: nothing changes, and the PATH is now right, so no second block.
    let (ok, out) = run(home, &["install-cli", "--user", "--add-to-path"]);
    assert!(ok, "{out}");
    assert!(out.contains("Already installed"), "{out}");
    assert_eq!(std::fs::read_to_string(&zprofile).unwrap(), profile);

    // A file that isn't ours is never replaced.
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "someone else's").unwrap();
    let (ok, out) = run(home, &["install-cli", "--user", "--force"]);
    assert!(!ok && out.contains("isn't a link"), "{out}");
    assert_eq!(std::fs::read_to_string(&link).unwrap(), "someone else's");
    std::fs::remove_file(&link).unwrap();
    assert!(run(home, &["install-cli", "--user"]).0);

    let (ok, out) = run(home, &["uninstall-cli", "--yes"]);
    assert!(ok, "{out}");
    assert!(std::fs::symlink_metadata(&link).is_err(), "link removed: {out}");
    assert_eq!(std::fs::read_to_string(&zprofile).unwrap(), "# mine\n", "only our block removed");
    assert!(!home.join("data/cli-link.json").exists());

    let (ok, out) = run(home, &["uninstall-cli"]);
    assert!(ok && out.contains("nothing to remove"), "{out}");
}
