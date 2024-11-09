use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginData {
    pub username: String,
    pub password: String,
}

fn default_cache_path() -> PathBuf {
    ".cache".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub login: LoginData,
    #[serde(default = "default_cache_path")]
    pub cache_path: PathBuf,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        let data = toml::from_str(&data)?;
        Ok(data)
    }
}
