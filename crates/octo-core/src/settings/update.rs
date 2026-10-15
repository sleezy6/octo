//! The release check settings (the `Updates` section).

use serde::{Deserialize, Serialize};

/// Whether Octo looks for its own new releases, and where. Read fresh at every check, so a
/// change applies without a restart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UpdateSettings {
    /// Ask GitHub every 6 hours whether a newer Octo release is out, and say so on the dashboard
    /// (default: true). Off means Octo never contacts GitHub for this.
    /// Environment variable: UPDATES__CHECK
    pub check: bool,

    /// The GitHub repository whose releases count, as "owner/name" (default: sleezy6/octo).
    /// This modified version names its own repository, so upstream's releases are never offered
    /// as an update over it.
    /// Environment variable: UPDATES__REPO
    pub repo: String,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check: true,
            repo: "sleezy6/octo".to_string(),
        }
    }
}
