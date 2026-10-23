//! Index configuration (Everything's "Indexes" pages): which volumes and
//! folders are indexed and what is excluded. Stored as JSON next to the index.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fold::{FoldMode, fold_mode};
use crate::query::glob_match;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VolumeConfig {
    /// Index this volume at all.
    pub include: bool,
    /// If non-empty, only these folders (full paths) on the volume are visible.
    pub include_only: Vec<String>,
}

impl Default for VolumeConfig {
    fn default() -> Self {
        VolumeConfig {
            include: true,
            include_only: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderConfig {
    /// Folder to index by walking it (non-NTFS drives, network shares, ...).
    pub path: String,
    /// Watch for changes with ReadDirectoryChangesW.
    pub watch: bool,
    /// Full rescan interval in minutes (0 = never; only on watch overflow).
    pub rescan_minutes: u32,
}

impl Default for FolderConfig {
    fn default() -> Self {
        FolderConfig {
            path: String::new(),
            watch: true,
            rescan_minutes: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub exclude_hidden: bool,
    pub exclude_system: bool,
    /// Folders to exclude: full paths (`C:\Users\me\OneDrive`) or wildcard
    /// folder-name patterns (`node_modules`, `.git`, `*cache*`).
    pub exclude_folders: Vec<String>,
    /// Wildcard file-name patterns to exclude (`*.tmp;~$*`).
    pub exclude_files: Vec<String>,
    /// If non-empty, only files matching one of these patterns are indexed.
    pub include_only_files: Vec<String>,
    /// Diacritics are significant in searches (`a` ≠ `å`). Everything's
    /// default is off; for Swedish names on is often preferable.
    pub match_diacritics: bool,
    pub auto_include_fixed: bool,
    pub auto_include_removable: bool,
    /// Keyed by drive letter, e.g. `"C"`.
    pub volumes: BTreeMap<String, VolumeConfig>,
    pub folders: Vec<FolderConfig>,
    /// Size used when Quickseek has to create a USN journal.
    pub journal_max_size_kb: u64,
    pub journal_allocation_delta_kb: u64,
    /// Store date created (8 bytes per file). Needed for `dc:` and sorting
    /// by date created. Off by default, like Everything.
    pub index_date_created: bool,
    /// Keep a precomputed rank for these sorts (4 bytes per file each).
    pub fast_sort_size: bool,
    pub fast_sort_modified: bool,
    pub fast_sort_path: bool,
    /// Network shares (UNC folders) keep their index on disk, so a restart
    /// serves the last copy instead of an empty share until it's re-walked.
    pub share_cache: bool,
    /// Re-walk shares when the service starts even if a cached copy exists
    /// (off: trust the cache until the rescan interval or a change notice).
    pub share_rewalk_on_start: bool,
    /// Date created for shares (separate from local volumes: it comes free
    /// with the directory listing, but turning it on later re-walks every share).
    pub share_index_date_created: bool,
    /// Rescan interval given to newly added shares, in minutes.
    pub share_rescan_minutes: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            exclude_hidden: false,
            exclude_system: false,
            exclude_folders: Vec::new(),
            exclude_files: Vec::new(),
            include_only_files: Vec::new(),
            match_diacritics: true,
            auto_include_fixed: true,
            auto_include_removable: false,
            volumes: BTreeMap::new(),
            folders: Vec::new(),
            journal_max_size_kb: 32 * 1024,
            journal_allocation_delta_kb: 8 * 1024,
            index_date_created: false,
            fast_sort_size: true,
            fast_sort_modified: true,
            fast_sort_path: true,
            share_cache: true,
            share_rewalk_on_start: true,
            share_index_date_created: true,
            share_rescan_minutes: 60,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::warn!("{}: {e}; using defaults", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap())?;
        std::fs::rename(tmp, path)
    }

    pub fn index_options(&self) -> crate::index::IndexOptions {
        crate::index::IndexOptions {
            created: self.index_date_created,
            fast_sort: [
                true,
                self.fast_sort_size,
                self.fast_sort_modified,
                self.fast_sort_path,
            ],
        }
    }

    /// Options for one volume or folder index (shares have their own date-created switch).
    pub fn index_options_for(&self, key: &str) -> crate::index::IndexOptions {
        let mut o = self.index_options();
        if crate::ntfs::win::is_network_path(key) {
            o.created = self.share_index_date_created;
        }
        o
    }

    pub fn fold_mode(&self) -> FoldMode {
        if self.match_diacritics {
            FoldMode::Lower
        } else {
            FoldMode::StripDiacritics
        }
    }

    pub fn volume(&self, letter: char) -> VolumeConfig {
        self.volumes
            .get(&letter.to_ascii_uppercase().to_string())
            .cloned()
            .unwrap_or_default()
    }

    /// Rules that decide which entries are visible, folded for matching.
    pub fn rules(&self, letter: char) -> Rules {
        let mode = self.fold_mode();
        let norm_path = |p: &str| {
            fold_mode(p.trim_end_matches('\\'), mode)
                .replace('/', "\\")
                .into_bytes()
        };
        let split = |list: &[String]| -> Vec<Vec<u8>> {
            list.iter()
                .flat_map(|s| s.split(';'))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| fold_mode(s, mode).into_bytes())
                .collect()
        };
        let mut folder_paths = Vec::new();
        let mut folder_patterns = Vec::new();
        for f in self
            .exclude_folders
            .iter()
            .flat_map(|s| s.split(';'))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if f.contains('\\') || f.contains('/') || f.contains(':') {
                folder_paths.push(norm_path(f));
            } else {
                folder_patterns.push(fold_mode(f, mode).into_bytes());
            }
        }
        let include_only = self
            .volume(letter)
            .include_only
            .iter()
            .map(|p| norm_path(p))
            .collect();
        Rules {
            exclude_hidden: self.exclude_hidden,
            exclude_system: self.exclude_system,
            folder_paths,
            folder_patterns,
            file_patterns: split(&self.exclude_files),
            include_files: split(&self.include_only_files),
            include_only,
        }
    }
}

/// Compiled visibility rules for one volume. Patterns and paths are folded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    pub exclude_hidden: bool,
    pub exclude_system: bool,
    pub folder_paths: Vec<Vec<u8>>,
    pub folder_patterns: Vec<Vec<u8>>,
    pub file_patterns: Vec<Vec<u8>>,
    pub include_files: Vec<Vec<u8>>,
    pub include_only: Vec<Vec<u8>>,
}

pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

impl Rules {
    pub fn is_empty(&self) -> bool {
        *self == Rules::default()
    }

    /// Whether any rule needs full paths (more expensive to evaluate).
    pub fn needs_paths(&self) -> bool {
        !self.folder_paths.is_empty() || !self.include_only.is_empty()
    }

    /// Does this entry itself get excluded (ignoring its ancestors)?
    /// `path` is the folded full path, only consulted for directories when
    /// [`Rules::needs_paths`] and only built lazily by the caller.
    pub fn excludes(
        &self,
        is_dir: bool,
        attrs: u32,
        fold_name: &[u8],
        path: &mut dyn FnMut() -> Vec<u8>,
    ) -> bool {
        if (self.exclude_hidden && attrs & FILE_ATTRIBUTE_HIDDEN != 0)
            || (self.exclude_system && attrs & FILE_ATTRIBUTE_SYSTEM != 0)
        {
            return true;
        }
        if is_dir {
            if self
                .folder_patterns
                .iter()
                .any(|p| glob_match(p, fold_name))
            {
                return true;
            }
            if !self.folder_paths.is_empty() {
                let last_matches =
                    |p: &Vec<u8>| p.rsplit(|&b| b == b'\\').next() == Some(fold_name);
                if self.folder_paths.iter().any(last_matches) && self.folder_paths.contains(&path())
                {
                    return true;
                }
            }
            false
        } else {
            self.file_patterns.iter().any(|p| glob_match(p, fold_name))
                || (!self.include_files.is_empty()
                    && !self.include_files.iter().any(|p| glob_match(p, fold_name)))
        }
    }

    /// For include-only volumes: is `path` (folded) inside, or an ancestor of,
    /// an included folder? Ancestors stay visible so the tree is navigable.
    pub fn include_only_allows(&self, path: &[u8]) -> InclusionState {
        if self.include_only.is_empty() {
            return InclusionState::Inside;
        }
        for inc in &self.include_only {
            if path == inc.as_slice()
                || (path.starts_with(inc) && path.get(inc.len()) == Some(&b'\\'))
            {
                return InclusionState::Inside;
            }
        }
        for inc in &self.include_only {
            if inc.starts_with(path) && inc.get(path.len()) == Some(&b'\\') {
                return InclusionState::Ancestor;
            }
        }
        InclusionState::Outside
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InclusionState {
    Inside,
    Ancestor,
    Outside,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_and_defaults_for_missing_fields() {
        let mut c = Config::default();
        c.exclude_folders.push(r"C:\Users\me\OneDrive".into());
        c.volumes.insert(
            "S".into(),
            VolumeConfig {
                include: false,
                include_only: vec![],
            },
        );
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&s).unwrap(), c);
        let partial: Config = serde_json::from_str(r#"{"exclude_hidden": true}"#).unwrap();
        assert!(partial.exclude_hidden && partial.match_diacritics && partial.auto_include_fixed);
    }

    #[test]
    fn rules_match_paths_patterns_and_files() {
        let c = Config {
            exclude_folders: vec![r"C:\Users\Me\OneDrive\".into(), "node_modules;.git".into()],
            exclude_files: vec!["*.tmp".into()],
            ..Default::default()
        };
        let r = c.rules('C');
        let p = |s: &str| {
            let s = s.to_string();
            move || s.clone().into_bytes()
        };
        assert!(r.excludes(true, 0, b"onedrive", &mut p(r"c:\users\me\onedrive")));
        assert!(!r.excludes(true, 0, b"onedrive", &mut p(r"c:\other\onedrive")));
        assert!(r.excludes(true, 0, b"node_modules", &mut p("x")));
        assert!(r.excludes(false, 0, b"a.tmp", &mut p("x")));
        assert!(!r.excludes(false, 0, b"a.txt", &mut p("x")));
        let hidden = Config {
            exclude_hidden: true,
            ..Default::default()
        }
        .rules('C');
        assert!(hidden.excludes(false, FILE_ATTRIBUTE_HIDDEN, b"x", &mut p("x")));
    }

    #[test]
    fn shares_have_their_own_date_created_switch() {
        let c = Config {
            index_date_created: false,
            share_index_date_created: true,
            ..Default::default()
        };
        assert!(!c.index_options_for("C").created);
        assert!(!c.index_options_for("G:").created);
        assert!(c.index_options_for(r"\\nas\media").created);
        assert!(c.share_cache && c.share_rewalk_on_start && c.share_rescan_minutes == 60);
    }

    #[test]
    fn include_only() {
        let mut c = Config::default();
        c.volumes.insert(
            "C".into(),
            VolumeConfig {
                include: true,
                include_only: vec![r"C:\Users\me".into()],
            },
        );
        let r = c.rules('C');
        assert_eq!(
            r.include_only_allows(br"c:\users\me\docs"),
            InclusionState::Inside
        );
        assert_eq!(
            r.include_only_allows(br"c:\users\me"),
            InclusionState::Inside
        );
        assert_eq!(
            r.include_only_allows(br"c:\users"),
            InclusionState::Ancestor
        );
        assert_eq!(
            r.include_only_allows(br"c:\users\meow"),
            InclusionState::Outside
        );
        assert_eq!(
            r.include_only_allows(br"c:\windows"),
            InclusionState::Outside
        );
    }
}
