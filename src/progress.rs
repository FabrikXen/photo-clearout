//! The tool's saved state: small JSON files in the project's `.state/` folder.
//!
//! - `progress.json`: where you stopped
//! - `settings.json`: your grid size
//! - `marked.json`: photos marked for the trash, kept until trashed
//! - `trashed.json`: photos trashed this session, so they can be restored (undo)

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::immich::Asset;
use crate::paging::Cursor;

pub const MIN_COLS: u8 = 1;
pub const MAX_COLS: u8 = 12;

/// Where you stopped.
pub type ProgressStore = JsonStore<Cursor>;
/// Your preferences.
pub type SettingsStore = JsonStore<Settings>;
/// Photos marked for the trash, in this or an unfinished earlier session.
pub type MarkStore = JsonStore<Marked>;
/// Photos trashed this session, for the summary screen and undo.
pub type TrashedStore = JsonStore<Trashed>;

pub type Marked = AssetList;
pub type Trashed = AssetList;

/// A list of photos, kept with their date and name so the review and summary
/// screens can show them without asking Immich again.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetList {
    pub assets: Vec<Asset>,
}

impl AssetList {
    /// The ids in the list, for quick lookups (lists can hold 20,000 photos).
    #[must_use]
    pub fn ids(&self) -> HashSet<&str> {
        self.assets.iter().map(|a| a.id.as_str()).collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The grid is cols × cols photos. None = the page's default.
    pub cols: Option<u8>,
}

/// One value of type `T`, stored as JSON in one file.
pub struct JsonStore<T> {
    path: PathBuf,
    // Two browser requests saving at once must not clash on the temp file.
    lock: Mutex<()>,
    _type: PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned + Default> JsonStore<T> {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
            _type: PhantomData,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved value, or the default if there is none.
    /// A damaged file is moved aside rather than stopping the tool. A file
    /// that can't be read is reported and treated as empty *for display
    /// only*: `update` refuses to save over it (see `try_read`).
    pub fn load(&self) -> T {
        let _guard = self.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.try_read().unwrap_or_else(|e| {
            eprintln!("warning: could not read {}: {e}", self.path.display());
            T::default()
        })
    }

    /// Saved atomically: write a temp file, flush it to disk, then rename it
    /// over the old one. A crash mid-save leaves the previous value intact,
    /// never half a file.
    ///
    /// # Errors
    /// If the file can't be written.
    pub fn save(&self, value: &T) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.write(value)
    }

    /// Load, change and save as one step, so two requests changing the
    /// value at the same time can't overwrite each other's change.
    ///
    /// If the file exists but can't be read, nothing is saved: writing the
    /// default plus the change would wipe what's in the file.
    ///
    /// # Errors
    /// If the file exists but can't be read, or can't be written.
    pub fn update(&self, change: impl FnOnce(&mut T)) -> io::Result<T>
    where
        T: Clone,
    {
        let _guard = self.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut value = self.try_read()?;
        change(&mut value);
        self.write(&value)?;
        Ok(value)
    }

    /// The default only when there is no file yet, or when the file is
    /// damaged (it's moved aside first, so nothing is lost). Any other
    /// problem reading it is an error.
    fn try_read(&self) -> io::Result<T> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
            Err(e) => return Err(e),
        };
        match serde_json::from_str(&text) {
            Ok(value) => Ok(value),
            Err(e) => {
                let aside = self.path.with_extension("json.corrupt");
                fs::rename(&self.path, &aside)?;
                eprintln!(
                    "warning: {} was unreadable ({e}); moved it to {} and using defaults",
                    self.path.display(),
                    aside.display()
                );
                Ok(T::default())
            }
        }
    }

    fn write(&self, value: &T) -> io::Result<()> {
        let dir = self.path.parent().filter(|d| !d.as_os_str().is_empty());
        if let Some(dir) = dir {
            // Only you can read the state: it holds file names and places.
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec(value).map_err(io::Error::other)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&json)?;
        // On disk before the rename, so a power cut can't leave an empty file.
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        if let Some(dir) = dir {
            // Makes the rename itself survive a power cut.
            fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

/// Create the state folder if needed and make it private to you (0700).
/// Also tightens a folder made by an older version (0755).
///
/// # Errors
/// If the folder can't be created or its permissions can't be changed.
pub fn prepare_state_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn missing_file_means_start_from_the_beginning() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProgressStore::new(dir.path().join("progress.json"));
        assert_eq!(store.load(), Cursor::default());
    }

    #[test]
    fn saved_position_is_loaded_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProgressStore::new(dir.path().join("sub/dir/progress.json"));
        let cursor = Cursor {
            after: Some(Utc.with_ymd_and_hms(2009, 5, 1, 12, 0, 0).unwrap()),
            seen_at_boundary: vec!["abc".into()],
        };
        store.save(&cursor).unwrap();
        assert_eq!(store.load(), cursor);
        assert!(!dir.path().join("sub/dir/progress.json.tmp").exists());
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        fs::write(&path, "{ this is not json").unwrap();
        let store = ProgressStore::new(path.clone());

        assert_eq!(store.load(), Cursor::default());
        assert!(!path.exists());
        assert!(dir.path().join("progress.json.corrupt").exists());
    }

    #[test]
    fn update_changes_and_saves_in_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("settings.json"));
        let after = store.update(|s| s.cols = Some(4)).unwrap();
        assert_eq!(after.cols, Some(4));
        assert_eq!(store.load().cols, Some(4));
    }

    #[test]
    fn update_never_saves_over_a_file_it_could_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marked.json");
        fs::write(&path, r#"{"assets":[]}"#).unwrap();
        // Unreadable, but not missing or damaged.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let store = MarkStore::new(path.clone());

        assert!(store.update(|m| m.assets.clear()).is_err());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"assets":[]}"#, "file untouched");
    }

    #[test]
    fn state_files_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = SettingsStore::new(state.join("settings.json"));
        store.save(&Settings { cols: Some(3) }).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&state.join("settings.json")), 0o600);
    }

    #[test]
    fn prepare_state_dir_tightens_an_old_folder() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(state.join("marked.json"), "{}").unwrap();
        fs::set_permissions(state.join("marked.json"), fs::Permissions::from_mode(0o644)).unwrap();

        prepare_state_dir(&state).unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&state.join("marked.json")), 0o600);
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("settings.json"));
        assert_eq!(store.load(), Settings::default());
        store.save(&Settings { cols: Some(7) }).unwrap();
        assert_eq!(store.load().cols, Some(7));
    }
}
