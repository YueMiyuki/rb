use serde::{Deserialize, Serialize};

/// One key, several entries when discovered inputs differ. An `env!` value, for instance.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UnitManifest {
    pub key: String,
    pub entries: Vec<Entry>,
}

pub const MAX_ENTRIES: usize = 8;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entry {
    pub label: String,
    pub created: u64,
    pub last_used: u64,
    pub outputs: Vec<OutputFile>,
    /// Relative dirs to recreate. A build script's `OUT_DIR`, for example.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symlinks: Vec<(String, String)>,
    /// Discovered while building, and not already in the key.
    #[serde(default)]
    pub inputs: ExtraInputs,
    /// Build-script output, with paths still written as `{OUT_DIR}` tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Diagnostics to replay on a cache hit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFile {
    pub name: String,
    pub blob: String,
    #[serde(default)]
    pub exec: bool,
    pub size: u64,
    /// Still has `{OUT_DIR}` and friends. Expanded when we materialize.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub templated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraInputs {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvInput>,
}

impl ExtraInputs {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.env.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInput {
    pub path: String,
    /// None if the file wasn't there.
    pub hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvInput {
    pub name: String,
    pub value: Option<String>,
}

impl UnitManifest {
    pub fn upsert(&mut self, entry: Entry) {
        self.entries.retain(|e| e.inputs != entry.inputs);
        self.entries.insert(0, entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.sort_by_key(|e| std::cmp::Reverse(e.last_used));
            self.entries.truncate(MAX_ENTRIES);
        }
    }

    pub fn last_used(&self) -> u64 {
        self.entries.iter().map(|e| e.last_used).max().unwrap_or(0)
    }
}
