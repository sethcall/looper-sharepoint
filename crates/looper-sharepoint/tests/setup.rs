//! `setup` and `setup export`, offline: `specs/onboarding-design.md` §4. Config and data in a temp directory.

use std::path::Path;
use std::process::Command;

fn run(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_looper-sharepoint"))
        .args(args)
        .env("HOME", dir)
        .env("LOOPER_SHAREPOINT_CONFIG", dir.join("config.toml"))
        .env("LOOPER_SHAREPOINT_DATA_DIR", dir.join("data"))
        .env_remove("LOOPER_SHAREPOINT_TEST_USERNAME")
        .env_remove("LOOPER_SHAREPOINT_TEST_PASSWORD")
        .output()
        .unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

const LINK: &str = "looper-sharepoint://setup?tenant=11111111-2222-3333-4444-555555555555&client_id=66666666-7777-8888-9999-000000000000&app_name=Contoso%20Sync";

#[test]
fn setup_from_link_and_file_and_export() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path();
    let config = d.join("config.toml");

    // From a link: a new config with just the account; then the next step is signing in.
    let (ok, out) = run(d, &["setup", LINK, "--yes"]);
    assert!(ok, "{out}");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("tenant = \"11111111-2222-3333-4444-555555555555\"") && text.contains("app_name = \"Contoso Sync\""), "{text}");
    assert!(out.contains("looper-sharepoint login"), "{out}");
    // Again: same organization, nothing new.
    assert!(run(d, &["setup", LINK, "--yes"]).0);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);

    // Export: a link that round-trips, and a file.
    let (ok, out) = run(d, &["setup", "export", "--link"]);
    assert!(ok && out.trim() == LINK, "{out}");
    let file = d.join("Contoso.looper-sharepoint");
    assert!(run(d, &["setup", "export", "-o", file.to_str().unwrap()]).0);
    assert!(std::fs::read_to_string(&file).unwrap().starts_with("# Contoso Sync: setup for looper-sharepoint."));

    // A file with a library adds it (with comments in the config kept).
    std::fs::write(&config, format!("# mine\n{text}")).unwrap();
    let with_lib = format!(
        "{}\n[[library]]\nsite = \"https://contoso.sharepoint.com/sites/eng\"\nlibrary = \"Documents\"\nfolder = \"Specs\"\npath = \"~/work/specs\"\n",
        std::fs::read_to_string(&file).unwrap()
    );
    std::fs::write(&file, with_lib).unwrap();
    let (ok, out) = run(d, &["setup", file.to_str().unwrap(), "--yes"]);
    assert!(ok, "{out}");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.starts_with("# mine\n") && text.contains("path = \"~/work/specs\"") && text.contains("folder = \"Specs\""), "{text}");
    // Again: the library is already there, so it isn't added twice.
    assert!(run(d, &["setup", file.to_str().unwrap(), "--yes"]).0);
    assert_eq!(std::fs::read_to_string(&config).unwrap().matches("[[library]]").count(), 1);

    // Another organization: refused, unless --replace.
    let other = LINK.replace("11111111", "99999999");
    let (ok, out) = run(d, &["setup", &other, "--yes"]);
    assert!(!ok && out.contains("--replace"), "{out}");
    let (ok, out) = run(d, &["setup", &other, "--yes", "--replace"]);
    assert!(ok, "{out}");
    assert!(std::fs::read_to_string(&config).unwrap().contains("99999999-2222"));

    // A setup file can't change sync settings.
    std::fs::write(&file, "[account]\ntenant = \"t\"\nclient_id = \"c\"\n[sync]\ndirection = \"download_only\"\n").unwrap();
    let (ok, out) = run(d, &["setup", file.to_str().unwrap(), "--yes"]);
    assert!(!ok && out.contains("sync"), "{out}");
    // Not on a terminal, without --yes: refuses rather than guessing.
    let (ok, out) = run(d, &["setup", &other]);
    assert!(!ok && out.contains("--yes"), "{out}");
}
