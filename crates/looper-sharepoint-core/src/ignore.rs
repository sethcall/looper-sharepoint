//! Local files that are never uploaded: OS junk, editor/Office lock files, our own temp folder, and `sync.ignore`.
//! See `specs/m4-design.md` §2.

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Always ignored. Matched against each path component's name.
pub const BUILTIN: &[&str] = &[
    // OS junk
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
    // Office owner/lock files and save temp files; LibreOffice locks
    "~$*",
    "~*.tmp",
    ".~lock.*#",
    // Editors: vim swap files, vim's write-test file, backup files, emacs autosave
    ".*.sw[a-p]",
    "4913",
    "*~",
    "#*#",
    // Our own download staging folder
    ".looper-sharepoint-tmp",
];

pub struct Ignore {
    names: GlobSet,
    paths: GlobSet,
}

impl Ignore {
    /// `extra` patterns are matched against both each name and the whole relative path (`/`-separated).
    pub fn new(extra: &[String]) -> Result<Self, globset::Error> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for p in BUILTIN {
            names.add(Glob::new(p)?);
        }
        for p in extra {
            names.add(Glob::new(p)?);
            paths.add(Glob::new(p)?);
        }
        Ok(Self { names: names.build()?, paths: paths.build()? })
    }

    /// Is `rel_path` (relative to the library folder) or any folder above it ignored?
    pub fn is_ignored(&self, rel_path: &str) -> bool {
        rel_path.split('/').any(|seg| self.names.is_match(seg)) || self.paths.is_match(rel_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_and_extra() {
        let i = Ignore::new(&["*.bak".into(), "build/**".into()]).unwrap();
        assert!(i.is_ignored(".DS_Store"));
        assert!(i.is_ignored("docs/.DS_Store"));
        assert!(i.is_ignored("docs/~$report.docx"));
        assert!(i.is_ignored(".~lock.notes.odt#"));
        assert!(i.is_ignored("docs/~WRL0001.tmp"), "Office save temp files");
        assert!(i.is_ignored("src/.main.rs.swp") && i.is_ignored("src/.main.rs.swo"), "vim swap files");
        assert!(i.is_ignored("src/4913") && i.is_ignored("notes.txt~") && i.is_ignored("#notes.txt#"));
        assert!(!i.is_ignored("src/main.swp.txt") && !i.is_ignored("4913.txt"));
        assert!(i.is_ignored(".looper-sharepoint-tmp/abc.part"));
        assert!(i.is_ignored("a/old.bak"));
        assert!(i.is_ignored("build/out/x.o"));
        assert!(!i.is_ignored("docs/report.docx"));
        assert!(!i.is_ignored("rebuild/x.o"));
    }
}
