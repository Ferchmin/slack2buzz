//! Reading a Slack export, whether it is still a `.zip` or has been unzipped.
//!
//! Operators unzip exports as often as not, and the fixtures in this repo are
//! plain directories so they stay reviewable in a diff. Both are supported
//! behind one interface.
//!
//! Slack zips sometimes wrap everything in a single top-level directory and
//! sometimes do not. [`Export::open`] strips that prefix if present, so every
//! path used elsewhere in the crate is relative to the export root.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// An opened Slack export.
#[derive(Debug)]
pub enum Export {
    Dir {
        root: PathBuf,
    },
    Zip {
        archive: zip::ZipArchive<std::fs::File>,
        /// Top-level wrapper directory to strip, with trailing slash.
        prefix: String,
        /// Every entry name, prefix already stripped.
        names: Vec<String>,
    },
}

impl Export {
    pub fn open(path: &Path) -> Result<Self> {
        if path.is_dir() {
            return Ok(Self::Dir {
                root: path.to_path_buf(),
            });
        }

        let file = std::fs::File::open(path)
            .with_context(|| format!("opening export {}", path.display()))?;
        let archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading {} as a zip archive", path.display()))?;

        let raw: Vec<String> = archive.file_names().map(str::to_string).collect();
        let prefix = detect_prefix(&raw);
        let names = raw
            .iter()
            .filter_map(|n| n.strip_prefix(&prefix))
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect();

        Ok(Self::Zip {
            archive,
            prefix,
            names,
        })
    }

    /// Read and deserialise a JSON file at the export root.
    ///
    /// Returns `Ok(None)` when the file is absent — most of the top-level
    /// manifests are optional, and a workspace with no private channels simply
    /// has no `groups.json`. A file that exists but does not parse is an
    /// error, not an absence.
    pub fn read_json<T: serde::de::DeserializeOwned>(&mut self, name: &str) -> Result<Option<T>> {
        let Some(bytes) = self.read_bytes(name)? else {
            return Ok(None);
        };
        let value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {name} from the export"))?;
        Ok(Some(value))
    }

    pub fn read_bytes(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Dir { root } => {
                let path = root.join(name);
                if !path.is_file() {
                    return Ok(None);
                }
                let bytes =
                    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                Ok(Some(bytes))
            }
            Self::Zip {
                archive, prefix, ..
            } => {
                let full = format!("{prefix}{name}");
                let mut entry = match archive.by_name(&full) {
                    Ok(e) => e,
                    Err(zip::result::ZipError::FileNotFound) => return Ok(None),
                    Err(e) => return Err(e).with_context(|| format!("reading {name} from zip")),
                };
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut buf)
                    .with_context(|| format!("reading {name} from zip"))?;
                Ok(Some(buf))
            }
        }
    }

    /// Names of the per-day message files inside one channel directory,
    /// sorted. Slack names them `YYYY-MM-DD.json`, so lexical order is
    /// chronological order.
    pub fn channel_day_files(&self, channel_dir: &str) -> Result<Vec<String>> {
        match self {
            Self::Dir { root } => {
                let dir = root.join(channel_dir);
                if !dir.is_dir() {
                    return Ok(Vec::new());
                }
                let mut days = BTreeSet::new();
                for entry in
                    std::fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))?
                {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".json") {
                        days.insert(format!("{channel_dir}/{name}"));
                    }
                }
                Ok(days.into_iter().collect())
            }
            Self::Zip { names, .. } => {
                let want = format!("{channel_dir}/");
                let mut days: Vec<String> = names
                    .iter()
                    .filter(|n| n.starts_with(&want) && n.ends_with(".json"))
                    // Only direct children; Slack does not nest, but a
                    // hand-edited zip might.
                    .filter(|n| !n[want.len()..].contains('/'))
                    .cloned()
                    .collect();
                days.sort();
                Ok(days)
            }
        }
    }

    /// Directory names at the export root. These are the per-conversation
    /// message directories; Slack names them after the channel (`general`) or
    /// the DM id (`D024BE7LH`).
    pub fn top_level_dirs(&self) -> Result<Vec<String>> {
        match self {
            Self::Dir { root } => {
                let mut dirs = BTreeSet::new();
                for entry in std::fs::read_dir(root)
                    .with_context(|| format!("listing {}", root.display()))?
                {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        dirs.insert(entry.file_name().to_string_lossy().to_string());
                    }
                }
                Ok(dirs.into_iter().collect())
            }
            Self::Zip { names, .. } => {
                let mut dirs = BTreeSet::new();
                for name in names {
                    if let Some((head, rest)) = name.split_once('/') {
                        if !rest.is_empty() {
                            dirs.insert(head.to_string());
                        }
                    }
                }
                Ok(dirs.into_iter().collect())
            }
        }
    }
}

/// Find the single wrapping directory to strip, if there is exactly one.
///
/// Returns `""` when entries live at the archive root, or when there are
/// several top-level entries (in which case none of them is a wrapper).
fn detect_prefix(names: &[String]) -> String {
    let mut heads = BTreeSet::new();
    let mut has_root_file = false;

    for name in names {
        match name.split_once('/') {
            Some((head, _)) => {
                heads.insert(head);
            }
            None => has_root_file = true,
        }
    }

    // A file at the root means the root *is* the export root.
    if has_root_file || heads.len() != 1 {
        return String::new();
    }
    // Sole top-level directory. Treat it as a wrapper only if the manifests
    // we expect live inside it rather than beside it.
    let head = heads.iter().next().copied().unwrap_or_default();
    let wrapped = format!("{head}/users.json");
    if names.iter().any(|n| n == &wrapped) {
        format!("{head}/")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    // A panic IS the failure report in a test; Buzz's CONTRIBUTING allows it.
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn no_prefix_when_manifests_are_at_the_root() {
        let names = vec![
            "users.json".to_string(),
            "channels.json".to_string(),
            "general/2024-01-01.json".to_string(),
        ];
        assert_eq!(detect_prefix(&names), "");
    }

    #[test]
    fn single_wrapper_directory_is_stripped() {
        let names = vec![
            "export/users.json".to_string(),
            "export/channels.json".to_string(),
            "export/general/2024-01-01.json".to_string(),
        ];
        assert_eq!(detect_prefix(&names), "export/");
    }

    #[test]
    fn a_lone_channel_directory_is_not_mistaken_for_a_wrapper() {
        // No users.json inside it, so `general` is content, not a wrapper.
        let names = vec!["general/2024-01-01.json".to_string()];
        assert_eq!(detect_prefix(&names), "");
    }

    #[test]
    fn several_top_level_directories_means_no_wrapper() {
        let names = vec!["a/users.json".to_string(), "b/users.json".to_string()];
        assert_eq!(detect_prefix(&names), "");
    }
}
