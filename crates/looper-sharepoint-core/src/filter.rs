//! `include` / `exclude` globs from a `[[library]]` block, matched against the item's remote path relative to the
//! library root (`/`-separated, remote names). SharePoint delta can only run at the library root, so folder scoping is
//! entirely client-side.

use globset::{Glob, GlobSet, GlobSetBuilder};

#[derive(Debug, Clone)]
pub struct Filter {
    include: Option<GlobSet>,
    exclude: GlobSet,
}

impl Filter {
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self, globset::Error> {
        let build = |pats: &[String]| -> Result<GlobSet, globset::Error> {
            let mut b = GlobSetBuilder::new();
            for p in pats {
                b.add(Glob::new(p)?);
            }
            b.build()
        };
        Ok(Self {
            include: if include.is_empty() { None } else { Some(build(include)?) },
            exclude: build(exclude)?,
        })
    }

    /// Should this file be synced?
    pub fn includes_file(&self, rel_path: &str) -> bool {
        !self.exclude.is_match(rel_path) && self.include.as_ref().is_none_or(|i| i.is_match(rel_path))
    }

    /// Should this folder be created even if no included file lives under it? Only when there are no include
    /// patterns (mirror everything) or the folder itself matches one.
    pub fn includes_dir(&self, rel_path: &str) -> bool {
        !self.excludes_dir(rel_path) && self.include.as_ref().is_none_or(|i| i.is_match(rel_path))
    }

    /// An excluded folder prunes everything under it.
    pub fn excludes_dir(&self, rel_path: &str) -> bool {
        self.exclude.is_match(rel_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(inc: &[&str], exc: &[&str]) -> Filter {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        Filter::new(&s(inc), &s(exc)).unwrap()
    }

    #[test]
    fn everything_by_default() {
        let x = f(&[], &[]);
        assert!(x.includes_file("a/b/c.txt"));
        assert!(x.includes_dir("a/b"));
    }

    #[test]
    fn include_and_exclude() {
        let x = f(&["specs/**", "*.md"], &["**/~$*", "**/*.tmp"]);
        assert!(x.includes_file("specs/a/b.docx"));
        assert!(x.includes_file("notes/readme.md"));
        assert!(!x.includes_file("other/b.docx"));
        assert!(!x.includes_file("specs/~$lock.docx"));
        assert!(!x.includes_file("specs/x.tmp"));
        assert!(!x.includes_dir("other"));
    }

    #[test]
    fn excluded_folder() {
        let x = f(&[], &["build", "build/**"]);
        assert!(x.excludes_dir("build"));
        assert!(!x.includes_file("build/out.bin"));
        assert!(x.includes_file("src/main.rs"));
    }
}
