//! Remote name ↔ local name mapping. See `specs/m1-design.md` §2.
//!
//! SharePoint already rejects most names Windows can't store, so this is mostly dormant. When it does fire,
//! unrepresentable characters become Unicode lookalikes (the same approach as rclone), so the mapping is reversible.
//! The same map is used on every OS, so a library's local tree has identical names on macOS and Windows.

use std::collections::BTreeMap;

use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NamingMode {
    /// Replace unrepresentable characters with lookalikes.
    #[default]
    Escape,
    /// Don't sync items whose names need mapping; report them.
    Skip,
}

/// Characters Windows (or both OSes) can't store, mapped to full-width lookalikes.
const DEFAULT_MAP: &[(char, char)] = &[
    ('"', '＂'),
    ('*', '＊'),
    (':', '：'),
    ('<', '＜'),
    ('>', '＞'),
    ('?', '？'),
    ('\\', '＼'),
    ('|', '｜'),
];
const TRAILING_DOT: (char, char) = ('.', '．');
const TRAILING_SPACE: (char, char) = (' ', '␠');

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
    "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Debug, Clone)]
pub struct CharMap {
    fwd: BTreeMap<char, char>,
    rev: BTreeMap<char, char>,
}

impl Default for CharMap {
    fn default() -> Self {
        Self::with_overrides(&BTreeMap::new()).expect("default map is a bijection")
    }
}

impl CharMap {
    /// The default map plus user overrides from `[naming.char_map]`. The result must be a bijection, and no
    /// mapped-to character may itself be one that needs mapping.
    pub fn with_overrides(overrides: &BTreeMap<String, String>) -> Result<Self, String> {
        let mut fwd: BTreeMap<char, char> = DEFAULT_MAP.iter().copied().collect();
        for (k, v) in overrides {
            let k = single_char(k).ok_or_else(|| format!("naming.char_map key {k:?} must be one character"))?;
            let v = single_char(v).ok_or_else(|| format!("naming.char_map value {v:?} must be one character"))?;
            fwd.insert(k, v);
        }
        let mut rev = BTreeMap::new();
        for (&k, &v) in &fwd {
            if fwd.contains_key(&v) || v == '/' || v.is_control() {
                return Err(format!("naming.char_map: {k:?} maps to {v:?}, which is not storable locally"));
            }
            if let Some(prev) = rev.insert(v, k) {
                return Err(format!("naming.char_map: {prev:?} and {k:?} both map to {v:?}"));
            }
        }
        Ok(Self { fwd, rev })
    }

    /// Map one remote path component to its local form (NFC-normalized).
    /// Returns `Err(reason)` if the name can't be represented locally under `mode`.
    pub fn to_local(&self, remote: &str, mode: NamingMode) -> Result<String, String> {
        let name: String = remote.nfc().collect();
        if name.is_empty() || name == "." || name == ".." {
            return Err(format!("invalid name {remote:?}"));
        }
        let stem = name.split('.').next().unwrap_or("");
        if WINDOWS_RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
            return Err(format!("{remote:?} is a reserved name on Windows"));
        }

        let chars: Vec<char> = name.chars().collect();
        let mut out = String::with_capacity(name.len());
        let mut changed = false;
        for (i, &c) in chars.iter().enumerate() {
            let last = i == chars.len() - 1;
            let mapped = if c == '/' || c.is_control() {
                return Err(format!("{remote:?} contains an unrepresentable character {c:?}"));
            } else if let Some(&m) = self.fwd.get(&c) {
                m
            } else if last && c == TRAILING_DOT.0 {
                TRAILING_DOT.1
            } else if last && c == TRAILING_SPACE.0 {
                TRAILING_SPACE.1
            } else {
                c
            };
            changed |= mapped != c;
            out.push(mapped);
        }
        if changed && mode == NamingMode::Skip {
            return Err(format!("{remote:?} needs name mapping and naming.mode = \"skip\""));
        }
        Ok(out)
    }

    /// Reverse of [`to_local`](Self::to_local), for names of new local files (M4 uploads). Existing items keep
    /// their exact remote name from the state DB, so a remote name that already contains a lookalike is never
    /// round-tripped through this.
    pub fn to_remote(&self, local: &str) -> String {
        let chars: Vec<char> = local.chars().collect();
        chars
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let last = i == chars.len() - 1;
                if let Some(&r) = self.rev.get(&c) {
                    r
                } else if last && c == TRAILING_DOT.1 {
                    TRAILING_DOT.0
                } else if last && c == TRAILING_SPACE.1 {
                    TRAILING_SPACE.0
                } else {
                    c
                }
            })
            .collect()
    }
}

fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// Key for detecting local path collisions: NFC + lowercase. macOS (APFS default) and Windows (NTFS) are both
/// case-insensitive, so two remote names that differ only in case or normalization would collide on disk.
pub fn local_key(rel_path: &str) -> String {
    rel_path.nfc().collect::<String>().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_pass_through() {
        let m = CharMap::default();
        assert_eq!(m.to_local("Report Q3 (final).docx", NamingMode::Escape).unwrap(), "Report Q3 (final).docx");
        assert_eq!(m.to_local("#hash%pct.txt", NamingMode::Skip).unwrap(), "#hash%pct.txt");
    }

    #[test]
    fn escapes_and_round_trips() {
        let m = CharMap::default();
        for remote in ["a:b?.txt", "what*.", "trailing ", "x|y\\z"] {
            let local = m.to_local(remote, NamingMode::Escape).unwrap();
            assert_ne!(local, remote);
            assert!(!local.contains([':', '?', '*', '|', '\\']));
            assert_eq!(m.to_remote(&local), remote);
        }
        assert!(m.to_local("a:b", NamingMode::Skip).is_err());
    }

    #[test]
    fn nfc_normalizes() {
        let m = CharMap::default();
        let nfd = "Cafe\u{301}.txt";
        assert_eq!(m.to_local(nfd, NamingMode::Escape).unwrap(), "Caf\u{e9}.txt");
        assert_eq!(local_key("CAFE\u{301}.TXT"), local_key("caf\u{e9}.txt"));
    }

    #[test]
    fn reserved_and_invalid() {
        let m = CharMap::default();
        assert!(m.to_local("CON.txt", NamingMode::Escape).is_err());
        assert!(m.to_local("lpt1", NamingMode::Escape).is_err());
        assert!(m.to_local("console.txt", NamingMode::Escape).is_ok());
        assert!(m.to_local("..", NamingMode::Escape).is_err());
        assert!(m.to_local("a\u{1}b", NamingMode::Escape).is_err());
    }

    #[test]
    fn overrides_validated() {
        let mut o = BTreeMap::new();
        o.insert(":".into(), "꞉".into());
        let m = CharMap::with_overrides(&o).unwrap();
        assert_eq!(m.to_local("a:b", NamingMode::Escape).unwrap(), "a꞉b");
        o.insert("?".into(), "꞉".into());
        assert!(CharMap::with_overrides(&o).is_err(), "not a bijection");
        let mut o = BTreeMap::new();
        o.insert(":".into(), "?".into());
        assert!(CharMap::with_overrides(&o).is_err(), "maps to an unstorable char");
    }
}
