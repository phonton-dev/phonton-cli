//! Shared SQLite store helpers for CLI subcommands and the serve API.

use anyhow::Result;
use phonton_store::Store;

pub fn default_store_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".phonton").join("store.sqlite3"))
}

pub fn open_persistent_store() -> Result<Store> {
    let path = default_store_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine ~/.phonton path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Store::open(path)
}
