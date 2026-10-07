//! Shared SQLite store helpers for CLI subcommands and the serve API.

use anyhow::Result;
use phonton_store::Store;

pub fn default_store_path() -> Option<std::path::PathBuf> {
    phonton_extensions::phonton_home().map(|h| h.join("store.sqlite3"))
}

pub fn open_persistent_store() -> Result<Store> {
    let path = default_store_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine ~/.phonton path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = Store::open(path)?;
    // Memory written and read here belongs to the repository Phonton runs in.
    Ok(
        match std::env::current_dir().and_then(std::fs::canonicalize) {
            Ok(dir) => store.with_memory_scope(dir.display().to_string()),
            Err(_) => store,
        },
    )
}
