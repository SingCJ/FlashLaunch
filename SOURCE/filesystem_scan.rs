use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::*;

pub(crate) const ENTRY_BATCH_SIZE: usize = 512;

#[derive(Clone)]
pub(crate) struct FilesystemEntry {
    pub(crate) path: PathBuf,
    pub(crate) file_name: OsString,
    pub(crate) is_dir: bool,
}

pub(crate) struct DirectoryEntryBatches {
    pub(crate) batches: Vec<Vec<FilesystemEntry>>,
    pub(crate) child_directories: Vec<PathBuf>,
}

pub(crate) fn enumerate_directory_batches(
    directory: &Path,
    exclusions: &[String],
) -> DirectoryEntryBatches {
    let mut batches = Vec::new();
    let mut batch = Vec::with_capacity(ENTRY_BATCH_SIZE);
    let mut child_directories = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return DirectoryEntryBatches {
            batches,
            child_directories,
        };
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if is_dir && path_is_in_excluded_subtree(&path, exclusions) {
            continue;
        }
        if is_dir && is_reparse_point(&path) {
            continue;
        }
        if is_dir {
            child_directories.push(path.clone());
        }
        batch.push(FilesystemEntry {
            path,
            file_name,
            is_dir,
        });
        if batch.len() >= ENTRY_BATCH_SIZE {
            batches.push(std::mem::take(&mut batch));
            batch = Vec::with_capacity(ENTRY_BATCH_SIZE);
        }
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    DirectoryEntryBatches {
        batches,
        child_directories,
    }
}
