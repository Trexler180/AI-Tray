//! Small versioned on-disk caches for append-only usage logs.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[cfg(windows)]
fn replace_file(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let succeeded = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    version: u32,
    data: T,
}

fn cache_path(name: &str) -> Option<PathBuf> {
    let mut root = dirs::config_dir().or_else(dirs::home_dir)?;
    root.push("AI Usage Tray");
    root.push(format!("{name}-history-cache.json"));
    Some(root)
}

pub fn load<T: Default + DeserializeOwned>(name: &str, version: u32) -> T {
    cache_path(name)
        .and_then(|path| load_path(&path))
        .filter(|envelope| envelope.version == version)
        .map(|envelope| envelope.data)
        .unwrap_or_default()
}

fn load_path<T: DeserializeOwned>(path: &std::path::Path) -> Option<Envelope<T>> {
    fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
}

pub fn save<T: Serialize>(name: &str, version: u32, data: &T) -> std::io::Result<()> {
    let path =
        cache_path(name).ok_or_else(|| std::io::Error::other("no configuration directory"))?;
    save_path(&path, version, data)
}

fn save_path<T: Serialize>(path: &std::path::Path, version: u32, data: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp-aiusage");
    let body = serde_json::to_vec(&Envelope { version, data })?;
    fs::write(&temporary, body)?;
    replace_file(&temporary, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_save_replaces_an_existing_cache() {
        let root = std::env::temp_dir().join(format!("ai-usage-cache-{}", uuid::Uuid::new_v4()));
        let path = root.join("cache.json");
        save_path(&path, 1, &vec![1u64]).unwrap();
        save_path(&path, 1, &vec![2u64, 3]).unwrap();
        let loaded: Envelope<Vec<u64>> = load_path(&path).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.data, vec![2, 3]);
        let _ = fs::remove_dir_all(root);
    }
}
