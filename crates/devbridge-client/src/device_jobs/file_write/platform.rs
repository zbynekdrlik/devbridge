//! The one OS-specific question of the `file.write` executor (#130): is a
//! path a reparse point (junction, mount point, symlink, object-manager
//! link)? std's `is_symlink()` covers symlinks and junctions; Windows marks
//! every other reparse point with `FILE_ATTRIBUTE_REPARSE_POINT` — refused
//! too. Excluded from the mutation gate (`.cargo/mutants.toml`): Linux never
//! compiles the Windows half and the stub is `false` by definition.

use std::fs::Metadata;

/// `FILE_ATTRIBUTE_REPARSE_POINT`.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[cfg(windows)]
pub fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// No reparse points off Windows (symlinks are caught by `is_symlink()`).
#[cfg(not(windows))]
pub fn is_reparse_point(_metadata: &Metadata) -> bool {
    false
}
