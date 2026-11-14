//! The user's settings, stored as JSON at `~/.prins/config.json`.
//!
//! Shape: `{"printer": {"id": "...", "name": "..."}}`. Unknown keys are kept when saving, so other
//! tools (or newer versions) can share the file. `PRINS_HOME` moves the `.prins` folder, which is
//! how the tests stay away from the real one. `PRINS_PRINTER` (an id or a name) overrides the saved
//! printer for one run and is never written to the file.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Environment variable that moves the `.prins` folder (default: `~/.prins`).
pub const HOME_ENV: &str = "PRINS_HOME";
/// Environment variable that overrides the saved printer for one run.
pub const PRINTER_ENV: &str = "PRINS_PRINTER";

/// A printer the user chose. On macOS `id` is a CoreBluetooth UUID, not a MAC address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrinterRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl PrinterRef {
    /// A short human label such as `Q199E63C4390578 (id 7D3C...)`.
    pub fn describe(&self) -> String {
        match &self.name {
            Some(name) if name != &self.id => format!("{name} (id {})", self.id),
            _ => self.id.clone(),
        }
    }
}

/// Where the effective printer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The `PRINS_PRINTER` environment variable.
    Env,
    /// The config file.
    File,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub printer: Option<PrinterRef>,
    /// Keys this version does not know. Written back unchanged.
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_json::Value>,
}

/// The `.prins` folder: `$PRINS_HOME` if set and not empty, else `~/.prins`.
pub fn home_dir() -> PathBuf {
    match std::env::var_os(HOME_ENV) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".prins"),
    }
}

/// The config file: `<.prins>/config.json`.
pub fn path() -> PathBuf {
    home_dir().join("config.json")
}

impl Config {
    /// Read the config file. A missing file is an empty config; a broken one is an error that
    /// names the file, so nothing is silently overwritten.
    pub fn load() -> Result<Config> {
        Config::load_from(&path())
    }

    pub fn load_from(file: &Path) -> Result<Config> {
        let text = match fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(anyhow!("Cannot read {}: {e}.", file.display())),
        };
        if text.trim().is_empty() {
            return Ok(Config::default());
        }
        serde_json::from_str(&text).map_err(|e| {
            anyhow!(
                "{} is not valid config JSON ({e}). Fix or delete the file.",
                file.display()
            )
        })
    }

    /// Write the config file atomically: a temp file in the same folder, then a rename.
    pub fn save(&self) -> Result<()> {
        self.save_to(&path())
    }

    pub fn save_to(&self, file: &Path) -> Result<()> {
        let dir = file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir.display()))?;
        let mut text = serde_json::to_string_pretty(self).context("Cannot encode the config")?;
        text.push('\n');
        let stem = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.json".into());
        let tmp = dir.join(format!(".{stem}.tmp{}", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, file)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(anyhow!("Cannot write {}: {e}.", file.display()));
        }
        Ok(())
    }
}

/// The printer to use: `PRINS_PRINTER` if set, else the saved one. `None` when neither exists.
/// A config file that cannot be read counts as "no printer" here; `Config::load` reports why.
pub fn printer() -> Option<PrinterRef> {
    printer_with_source().map(|(p, _)| p)
}

/// Like [`printer`], and says where it came from.
pub fn printer_with_source() -> Option<(PrinterRef, Source)> {
    if let Some(p) = env_printer() {
        return Some((p, Source::Env));
    }
    Config::load()
        .ok()
        .and_then(|c| c.printer)
        .map(|p| (p, Source::File))
}

/// The override from `PRINS_PRINTER`. The text is either an id or a name, so it is stored as the
/// id and matched against names too (see `ble::resolve`).
pub fn env_printer() -> Option<PrinterRef> {
    let value = std::env::var(PRINTER_ENV).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(PrinterRef {
        id: value.to_string(),
        name: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that touch process environment variables run one at a time.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prins-config-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let dir = temp_dir("missing");
        let c = Config::load_from(&dir.join("nope.json")).unwrap();
        assert_eq!(c, Config::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_then_load_round_trips_and_creates_the_folder() {
        let dir = temp_dir("roundtrip");
        let file = dir.join("deep").join("config.json");
        let config = Config {
            printer: Some(PrinterRef {
                id: "ABC".into(),
                name: Some("Q199E63C4390578".into()),
            }),
            other: BTreeMap::new(),
        };
        config.save_to(&file).unwrap();
        assert_eq!(Config::load_from(&file).unwrap(), config);
        let raw = fs::read_to_string(&file).unwrap();
        assert!(raw.contains("\"printer\"") && raw.ends_with('\n'));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_keys_survive_a_save() {
        let dir = temp_dir("unknown");
        let file = dir.join("config.json");
        fs::write(
            &file,
            r#"{"theme": {"dark": true}, "printer": {"id": "A", "future": 1}, "n": 3}"#,
        )
        .unwrap();
        let mut c = Config::load_from(&file).unwrap();
        c.printer = Some(PrinterRef {
            id: "B".into(),
            name: None,
        });
        c.save_to(&file).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["theme"]["dark"], true);
        assert_eq!(v["n"], 3);
        assert_eq!(v["printer"]["id"], "B");
        assert!(v["printer"].get("name").is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn forgetting_the_printer_keeps_other_keys() {
        let dir = temp_dir("forget");
        let file = dir.join("config.json");
        fs::write(&file, r#"{"x": 1, "printer": {"id": "A"}}"#).unwrap();
        let mut c = Config::load_from(&file).unwrap();
        c.printer = None;
        c.save_to(&file).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["x"], 1);
        assert!(v.get("printer").is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_save_leaves_no_temp_file_and_replaces_the_old_content_whole() {
        let dir = temp_dir("atomic");
        let file = dir.join("config.json");
        fs::write(&file, "x".repeat(10_000)).unwrap(); // garbage that is longer than the new file
        Config {
            printer: Some(PrinterRef {
                id: "A".into(),
                name: None,
            }),
            other: BTreeMap::new(),
        }
        .save_to(&file)
        .unwrap();
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["config.json".to_string()]);
        assert!(Config::load_from(&file).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_broken_file_is_an_error_that_names_the_file() {
        let dir = temp_dir("broken");
        let file = dir.join("config.json");
        fs::write(&file, "{not json").unwrap();
        let err = Config::load_from(&file).unwrap_err().to_string();
        assert!(
            err.contains("config.json") && err.contains("Fix or delete"),
            "{err}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_empty_file_is_an_empty_config() {
        let dir = temp_dir("empty");
        let file = dir.join("config.json");
        fs::write(&file, "  \n").unwrap();
        assert_eq!(Config::load_from(&file).unwrap(), Config::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prins_home_moves_the_folder_and_the_env_printer_overrides_the_file() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("env");
        std::env::set_var(HOME_ENV, &dir);
        std::env::remove_var(PRINTER_ENV);
        assert_eq!(path(), dir.join("config.json"));
        assert_eq!(printer(), None);

        Config {
            printer: Some(PrinterRef {
                id: "FILE-ID".into(),
                name: Some("Q1AAAAAA".into()),
            }),
            other: BTreeMap::new(),
        }
        .save()
        .unwrap();
        let (p, source) = printer_with_source().unwrap();
        assert_eq!((p.id.as_str(), source), ("FILE-ID", Source::File));

        std::env::set_var(PRINTER_ENV, "  Q1BBBBBB ");
        let (p, source) = printer_with_source().unwrap();
        assert_eq!(
            (p.id.as_str(), p.name, source),
            ("Q1BBBBBB", None, Source::Env)
        );
        // The override is never written to the file.
        assert_eq!(Config::load().unwrap().printer.unwrap().id, "FILE-ID");

        std::env::set_var(PRINTER_ENV, "   ");
        assert_eq!(printer().unwrap().id, "FILE-ID");

        std::env::remove_var(PRINTER_ENV);
        std::env::remove_var(HOME_ENV);
        let _ = fs::remove_dir_all(dir);
    }
}
