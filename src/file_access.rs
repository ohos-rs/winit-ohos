//! Access to files selected by the system picker.
//!
//! A picker URI is not a Rust filesystem path. Persist and activate the user's
//! grant before retaining a path across Ability backgrounding or process restarts.
//! These calls require `ohos.permission.FILE_ACCESS_PERSIST` and must run on a worker.

use std::path::PathBuf;

pub use ohos_fileshare_binding as permissions;

/// Persist read (or read/write) access and resolve the selected native paths.
/// An empty selection represents cancellation. Partial policy failures are errors;
/// successfully persisted policies remain available for the caller to revoke.
pub fn persist_paths(uris: &[String], writable: bool) -> Result<Vec<PathBuf>, String> {
    if uris.is_empty() {
        return Ok(Vec::new());
    }
    let paths = uris
        .iter()
        .map(|uri| {
            crate::drag_drop::file_path(uri)
                .filter(|path| path.is_absolute())
                .ok_or_else(|| format!("Cannot resolve selected file URI: {uri}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let policies: Vec<_> = uris
        .iter()
        .map(|uri| permissions::PolicyInfo {
            uri: uri.clone(),
            operation_mode: if writable { 3 } else { 1 },
        })
        .collect();
    let failed = permissions::persist_permission(&policies).map_err(|error| error.to_string())?;
    if !failed.is_empty() {
        return Err(format!(
            "Cannot persist selected file permissions: {failed:?}"
        ));
    }
    let failed = permissions::activate_permission(&policies).map_err(|error| error.to_string())?;
    if !failed.is_empty() {
        return Err(format!(
            "Cannot activate selected file permissions: {failed:?}"
        ));
    }
    Ok(paths)
}
