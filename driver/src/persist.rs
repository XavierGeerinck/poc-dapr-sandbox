//! Restart recovery: each component's `Init` properties, persisted so that
//! a restarted driver restores its pools before daprd calls `Init` again.
//!
//! One JSON file per component, `<dir>/<sha256(component) hex>.json`,
//! holding `{"version": 1, "component": ..., "properties": {...}}`, written
//! atomically (temp file, fsync, rename, fsync of the directory). The
//! directory is mode 0700 and the files 0600: properties may carry secrets.
//!
//! A later `Init` of the component overwrites its file. Nothing deletes it:
//! daprd sends no RPC when it removes a component (it only reconciles the
//! component's instances away and closes the connection, as it also does
//! when it shuts down), so the driver cannot tell a removal from a restart.
//! A removed component's pool is thus restored by every later driver start,
//! until its file is deleted by hand.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::warn;

const VERSION: u64 = 1;
/// Larger files are not ours.
const MAX_FILE_SIZE: u64 = 1 << 20;

/// The state directory of persisted components.
#[derive(Debug, Clone)]
pub struct StateDir {
    dir: PathBuf,
}

/// A persisted component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persisted {
    pub component: String,
    pub properties: HashMap<String, String>,
}

impl StateDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file holding `component`'s properties.
    pub fn path_for(&self, component: &str) -> PathBuf {
        let digest = Sha256::digest(component.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{hex}.json"))
    }

    /// Atomically replaces `component`'s file with `properties`.
    pub fn save(&self, component: &str, properties: &HashMap<String, String>) -> io::Result<()> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let body = serde_json::to_vec_pretty(&json!({
            "version": VERSION,
            "component": component,
            "properties": properties,
        }))?;
        let path = self.path_for(component);
        let tmp = self.dir.join(format!(
            ".{}.{}-{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        // Make the rename durable; not all filesystems support it.
        if let Ok(d) = fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// Every valid persisted component, sorted by name. Skips (with a
    /// warning) files that are unreadable, malformed or misnamed; a missing
    /// directory holds none.
    pub fn load(&self) -> io::Result<Vec<Persisted>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let is_json = path.extension().is_some_and(|e| e == "json")
                && !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'));
            if !is_json {
                continue;
            }
            match self.read(&path) {
                Ok(p) => out.push(p),
                Err(e) => warn!(file = %path.display(), "skipping persisted component: {e}"),
            }
        }
        out.sort_by(|a, b| a.component.cmp(&b.component));
        Ok(out)
    }

    fn read(&self, path: &Path) -> Result<Persisted, String> {
        let md = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !md.is_file() {
            return Err("not a regular file".into());
        }
        if md.len() > MAX_FILE_SIZE {
            return Err(format!("larger than {MAX_FILE_SIZE} bytes"));
        }
        let body = fs::read(path).map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        if v.get("version").and_then(Value::as_u64) != Some(VERSION) {
            return Err(format!("not version {VERSION}"));
        }
        let component = v
            .get("component")
            .and_then(Value::as_str)
            .ok_or("no component name")?
            .to_string();
        if self.path_for(&component) != path {
            return Err(format!("does not belong to component {component:?}"));
        }
        let properties = v
            .get("properties")
            .and_then(Value::as_object)
            .ok_or("no properties")?
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => Ok((k.clone(), s.clone())),
                _ => Err(format!("property {k:?} is not a string")),
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        Ok(Persisted {
            component,
            properties,
        })
    }
}
