use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::*;

#[derive(Clone)]
pub(crate) struct RootOwnershipEntry {
    pub(crate) root: IndexRoot,
    pub(crate) path: PathBuf,
    pub(crate) key: String,
    pub(crate) exclusions: Arc<Vec<String>>,
}

#[derive(Default)]
pub(crate) struct RootOwnershipPlan {
    entries: Vec<RootOwnershipEntry>,
}

impl RootOwnershipPlan {
    pub(crate) fn build(roots: &[IndexRoot]) -> Self {
        let mut seen = HashSet::new();
        let mut effective = Vec::new();
        for root in roots.iter().filter(|root| root.enabled) {
            let Some(configured_path) = root.path.as_deref() else {
                continue;
            };
            let path = lexical_absolute_path(configured_path);
            if !path.is_dir() {
                continue;
            }
            let key = normalized_root_path_key(&path);
            if !seen.insert(key.clone()) {
                continue;
            }
            effective.push((root.clone(), path, key));
        }

        let entries = effective
            .iter()
            .map(|(root, path, key)| {
                let exclusions = effective
                    .iter()
                    .filter(|(_, _, candidate_key)| candidate_key != key)
                    .filter(|(_, _, candidate_key)| path_key_is_within(candidate_key, key))
                    .map(|(_, _, candidate_key)| candidate_key.clone())
                    .collect::<Vec<_>>();
                let mut effective_root = root.clone();
                effective_root.path = Some(path.clone());
                RootOwnershipEntry {
                    root: effective_root,
                    path: path.clone(),
                    key: key.clone(),
                    exclusions: Arc::new(exclusions),
                }
            })
            .collect();
        Self { entries }
    }

    pub(crate) fn entries(&self) -> &[RootOwnershipEntry] {
        &self.entries
    }

    pub(crate) fn owner(&self, path: &Path) -> Option<&RootOwnershipEntry> {
        let path_key = normalized_root_path_key(path);
        self.owner_by_key(&path_key)
    }

    pub(crate) fn owner_by_key(&self, path_key: &str) -> Option<&RootOwnershipEntry> {
        self.entries
            .iter()
            .filter(|entry| path_key_is_within(path_key, &entry.key))
            .filter(|entry| {
                !entry
                    .exclusions
                    .iter()
                    .any(|excluded| path_key_is_within(path_key, excluded))
            })
            .max_by_key(|entry| entry.key.len())
    }
}

pub(crate) fn lexical_absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        required_file_operation("working directory", Path::new("."), "read directory", || {
            std::env::current_dir()
        })
        .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn normalized_root_path_key(path: &Path) -> String {
    let mut value = lexical_absolute_path(path)
        .to_string_lossy()
        .replace('/', "\\");
    if let Some(stripped) = value.strip_prefix(r"\\?\UNC\") {
        value = format!(r"\\{stripped}");
    } else if let Some(stripped) = value.strip_prefix(r"\\?\") {
        value = stripped.to_string();
    }
    while value.len() > 3 && value.ends_with('\\') {
        value.pop();
    }
    fold_text(&value)
}

pub(crate) fn path_key_is_within(path_key: &str, root_key: &str) -> bool {
    path_key == root_key
        || if root_key.ends_with('\\') {
            path_key.starts_with(root_key)
        } else {
            path_key
                .strip_prefix(root_key)
                .is_some_and(|suffix| suffix.starts_with('\\'))
        }
}

pub(crate) fn path_key_relative_depth(path_key: &str, root_key: &str) -> Option<usize> {
    if !path_key_is_within(path_key, root_key) {
        return None;
    }
    if path_key == root_key {
        return Some(0);
    }
    Some(
        path_key[root_key.len()..]
            .trim_start_matches('\\')
            .split('\\')
            .filter(|component| !component.is_empty())
            .count(),
    )
}

#[cfg(test)]
pub(crate) fn path_is_within_root(path: &Path, root: &Path) -> bool {
    let path_key = normalized_root_path_key(path);
    let root_key = normalized_root_path_key(root);
    path_key_is_within(&path_key, &root_key)
}

pub(crate) fn path_is_in_excluded_subtree(path: &Path, exclusions: &[String]) -> bool {
    let path_key = normalized_root_path_key(path);
    exclusions
        .iter()
        .any(|excluded| path_key_is_within(&path_key, excluded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::current_dir()
                .unwrap()
                .join("TEMP")
                .join("root-ownership-tests")
                .join(format!("{name}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn root(raw: &str, path: PathBuf, enabled: bool, score: i32) -> IndexRoot {
        IndexRoot {
            raw: raw.to_string(),
            path: Some(path),
            enabled,
            score,
            max_depth: SEARCH_DEPTH_ALL,
            label: String::new(),
            keywords: Vec::new(),
        }
    }

    #[test]
    fn plan_keeps_first_duplicate_and_deepest_owner() {
        let parent = TestDirectory::new("duplicates");
        let child = parent.0.join("child");
        fs::create_dir_all(&child).unwrap();
        let roots = vec![
            root("parent-first", parent.0.clone(), true, 10),
            root("parent-duplicate", parent.0.join("."), true, 20),
            root("child", child.clone(), true, 30),
        ];

        let plan = RootOwnershipPlan::build(&roots);

        assert_eq!(plan.entries().len(), 2);
        assert_eq!(
            plan.owner(&parent.0.join("file.txt")).unwrap().root.raw,
            "parent-first"
        );
        assert_eq!(
            plan.owner(&child.join("file.txt")).unwrap().root.raw,
            "child"
        );
        assert_eq!(
            plan.entries()[0].exclusions.as_slice(),
            &[normalized_root_path_key(&child)]
        );
    }

    #[test]
    fn disabled_and_missing_roots_do_not_participate() {
        let existing = TestDirectory::new("enabled-duplicate");
        let missing = existing.0.join("missing");
        let roots = vec![
            root("disabled", existing.0.clone(), false, 10),
            root("enabled", existing.0.clone(), true, 20),
            root("missing", missing, true, 30),
        ];

        let plan = RootOwnershipPlan::build(&roots);

        assert_eq!(plan.entries().len(), 1);
        assert_eq!(plan.entries()[0].root.raw, "enabled");
    }

    #[test]
    fn lexical_containment_needs_no_existing_filesystem_path() {
        let root = PathBuf::from(r"C:\Mixed\Root\.\Folder");
        let child = PathBuf::from(r"c:/mixed/root/folder/sub/../file.txt");
        let sibling = PathBuf::from(r"C:\Mixed\Root\FolderElsewhere\file.txt");

        assert!(path_is_within_root(&child, &root));
        assert!(!path_is_within_root(&sibling, &root));
    }
    #[test]
    fn launch_item_depth_is_relative_to_deepest_owning_root() {
        let parent = TestDirectory::new("relative-depth");
        let child = parent.0.join("games");
        fs::create_dir_all(&child).unwrap();
        let plan = RootOwnershipPlan::build(&[
            root("parent", parent.0.clone(), true, 10),
            root("child", child.clone(), true, 20),
        ]);
        let owner = plan.owner(&child.join("arcade").join("run.exe")).unwrap();
        let root_file = launch_item_from_path_with_type(
            child.join("run.exe"),
            false,
            &owner.root,
            &ScoringConfig::default(),
        )
        .unwrap();
        let nested_file = launch_item_from_path_with_type(
            child.join("arcade").join("run.exe"),
            false,
            &owner.root,
            &ScoringConfig::default(),
        )
        .unwrap();
        let root_folder = launch_item_from_path_with_type(
            child.join("arcade"),
            true,
            &owner.root,
            &ScoringConfig::default(),
        )
        .unwrap();
        let nested_folder = launch_item_from_path_with_type(
            child.join("arcade").join("retro"),
            true,
            &owner.root,
            &ScoringConfig::default(),
        )
        .unwrap();

        assert_eq!(owner.root.raw, "child");
        assert_eq!(root_file.relative_depth, 0);
        assert_eq!(nested_file.relative_depth, 1);
        assert_eq!(root_folder.relative_depth, 1);
        assert_eq!(nested_folder.relative_depth, 2);
    }

    #[test]
    fn paths_outside_root_use_zero_relative_depth() {
        let root = root("root", PathBuf::from(r"C:\ROOT"), true, 0);
        let item = launch_item_from_path_with_type(
            PathBuf::from(r"D:\Other\run.exe"),
            false,
            &root,
            &ScoringConfig::default(),
        )
        .unwrap();
        assert_eq!(item.relative_depth, 0);
    }
}
