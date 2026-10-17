//! A thread's reporting checkout, independent of its execution environment and permissions.

use codex_utils_path_uri::LegacyAppPathString;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Saved path selection for status surfaces. Clients validate it on the workspace host before use.
/// A missing checkout does not erase the selection: the same path may become available again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct StatusLineWorktree {
    /// Environment that owns these paths; never resolve them on another host.
    pub environment_id: String,
    /// Absolute checkout root used for reporting, not the execution cwd.
    pub root: LegacyAppPathString,
    /// Absolute common Git directory identifying the repository, not a checkout incarnation.
    pub git_common_dir: LegacyAppPathString,
}
