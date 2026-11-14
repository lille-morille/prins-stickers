//! Drawings on disk.
//!
//! A drawing is the list of ops that made it, at `<home>/drawings/<id>.json`:
//! `{"version":1,"created":<unix secs>,"updated":<unix secs>,"printed":<n>,"ops":[...]}`.
//! It is loaded by replaying the ops into a fresh [`Canvas`]. Writes are atomic (a temp file in
//! the same folder, then a rename) and a read-modify-write happens under an exclusive lock on
//! `<id>.lock`, so two agents on one drawing queue up instead of overwriting each other.
//! Different ids never block each other.
//!
//! Every function takes the home folder through a [`Store`], so tests use their own folders
//! and never touch the process environment.

use std::collections::hash_map::RandomState;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canvas::Canvas;
use crate::config;

/// Environment variable that names the sender printed on posters.
pub const NAME_ENV: &str = "PRINS_NAME";
/// Environment variable with the default drawing id.
pub const DRAWING_ENV: &str = "PRINS_DRAWING";
/// The most characters an id can have.
pub const MAX_ID: usize = 40;
/// How many recent drawings an "unknown id" error lists.
const RECENT_SHOWN: usize = 5;

const ADJECTIVES: [&str; 64] = [
    "warm", "brave", "calm", "bright", "quick", "quiet", "happy", "lucky", "gentle", "bold",
    "clever", "cosmic", "cozy", "crisp", "eager", "fancy", "fuzzy", "golden", "jolly", "keen",
    "lively", "mellow", "merry", "mighty", "nimble", "noble", "peppy", "plucky", "proud", "rapid",
    "rustic", "shiny", "silver", "sleepy", "smooth", "snappy", "solid", "sunny", "swift", "tidy",
    "tiny", "vivid", "witty", "zesty", "amber", "azure", "coral", "dusty", "early", "fresh",
    "glad", "grand", "green", "honest", "ivory", "jaunty", "kind", "lunar", "misty", "neat",
    "olive", "polar", "royal", "sandy",
];

const NOUNS: [&str; 64] = [
    "fox", "owl", "otter", "panda", "koala", "lynx", "heron", "finch", "robin", "wren", "falcon",
    "badger", "beaver", "bison", "camel", "crane", "deer", "dolphin", "eagle", "ferret", "gecko",
    "hare", "ibis", "jaguar", "kiwi", "lemur", "llama", "mole", "moose", "newt", "ocelot", "orca",
    "parrot", "pony", "puffin", "quail", "raven", "seal", "shark", "sloth", "snail", "stoat",
    "swan", "tiger", "toad", "turtle", "vole", "walrus", "weasel", "whale", "wolf", "yak", "zebra",
    "acorn", "comet", "ember", "maple", "pebble", "river", "saturn", "thistle", "willow", "cedar",
    "daisy",
];

/// A drawing: its ops and a little bookkeeping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drawing {
    /// Not stored in the file: the file name is the id.
    #[serde(skip)]
    pub id: String,
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    /// How many times it was sent to the printer.
    #[serde(default)]
    pub printed: u32,
    #[serde(default)]
    pub ops: Vec<Value>,
}

fn one() -> u32 {
    1
}

/// One line of the drawings list.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub id: String,
    pub ops: usize,
    pub created: u64,
    pub updated: u64,
    pub printed: u32,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `[a-z0-9-]{1,40}`, so an id can never leave the drawings folder.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn check_id(id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        bail!("{id:?} is not a drawing id. Ids are 1 to {MAX_ID} characters from a-z, 0-9 and -, like warm-fox.")
    }
}

/// A pseudo-random number. Ids only need to be spread out, not secret, so this avoids a `rand`
/// dependency: the std hasher is seeded randomly per instance, and the clock and a counter are
/// mixed in.
fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    h.finish()
}

/// A fresh two word id such as `warm-fox` (not checked against the folder).
pub fn random_id() -> String {
    let r = random_u64();
    let a = ADJECTIVES[(r % 64) as usize];
    let n = NOUNS[((r >> 8) % 64) as usize];
    format!("{a}-{n}")
}

/// The sender printed on posters: `PRINS_NAME`, else the `name` in `<home>/config.json`, else "".
/// The model never sets it.
pub fn sender(home: &Path) -> String {
    if let Ok(v) = std::env::var(NAME_ENV) {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    config_name(home).unwrap_or_default()
}

/// The `name` key of `<home>/config.json`, if there is one.
pub fn config_name(home: &Path) -> Option<String> {
    let config = config::Config::load_from(&home.join("config.json")).ok()?;
    let name = config.other.get("name")?.as_str()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The drawings of one `.prins` folder.
#[derive(Debug, Clone)]
pub struct Store {
    home: PathBuf,
}

/// A drawing held under its lock. Dropping it releases the lock.
pub struct Locked {
    pub drawing: Drawing,
    path: PathBuf,
    _lock: File,
}

impl Locked {
    /// Write the drawing (atomically) and stamp `updated`.
    pub fn save(&mut self) -> Result<()> {
        self.drawing.updated = now();
        write_atomic(&self.path, &self.drawing)
    }
}

impl Store {
    pub fn new(home: impl Into<PathBuf>) -> Store {
        Store { home: home.into() }
    }

    /// The store of `$PRINS_HOME` (default `~/.prins`).
    pub fn from_env() -> Store {
        Store::new(config::home_dir())
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn drawings_dir(&self) -> PathBuf {
        self.home.join("drawings")
    }

    /// Where `save_design` and `load_design` keep named designs.
    pub fn designs_dir(&self) -> PathBuf {
        self.home.join("designs")
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.drawings_dir().join(format!("{id}.json"))
    }

    fn lock_path(&self, id: &str) -> PathBuf {
        self.drawings_dir().join(format!("{id}.lock"))
    }

    /// The sender for posters in this home folder.
    pub fn sender(&self) -> String {
        sender(&self.home)
    }

    pub fn exists(&self, id: &str) -> bool {
        valid_id(id) && self.path(id).is_file()
    }

    /// Create an empty drawing and return its id. With `name` that exact id is used (and must be
    /// free); without, a random two word id, checked for collisions.
    pub fn create(&self, name: Option<&str>) -> Result<String> {
        fs::create_dir_all(self.drawings_dir())
            .with_context(|| format!("Cannot create {}", self.drawings_dir().display()))?;
        if let Some(name) = name {
            check_id(name)?;
            return if self.try_create(name)? {
                Ok(name.to_string())
            } else {
                Err(anyhow!(
                    "A drawing named {name} already exists. Pick another name, or leave --name out."
                ))
            };
        }
        for _ in 0..200 {
            let id = random_id();
            if self.try_create(&id)? {
                return Ok(id);
            }
        }
        // Practically unreachable (4096 ids), but never loop forever.
        for n in 2..10_000 {
            let id = format!("{}-{n}", random_id());
            if id.len() <= MAX_ID && self.try_create(&id)? {
                return Ok(id);
            }
        }
        bail!("Cannot find a free drawing id. Run `prins drawings --prune 30` to clear old ones.")
    }

    /// Reserve `id` under its lock and write an empty drawing. False if it already exists.
    fn try_create(&self, id: &str) -> Result<bool> {
        let lock = self.open_lock(id)?;
        lock.lock()
            .with_context(|| format!("Cannot lock drawing {id}"))?;
        if self.path(id).exists() {
            return Ok(false);
        }
        let stamp = now();
        let drawing = Drawing {
            id: id.to_string(),
            version: 1,
            created: stamp,
            updated: stamp,
            printed: 0,
            ops: vec![],
        };
        write_atomic(&self.path(id), &drawing)?;
        Ok(true)
    }

    fn open_lock(&self, id: &str) -> Result<File> {
        fs::create_dir_all(self.drawings_dir())
            .with_context(|| format!("Cannot create {}", self.drawings_dir().display()))?;
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path(id))
            .with_context(|| format!("Cannot open the lock file of drawing {id}"))
    }

    /// Read a drawing. No lock is needed: writes are atomic renames, so a reader sees the old
    /// file or the new one.
    pub fn read(&self, id: &str) -> Result<Drawing> {
        check_id(id)?;
        let path = self.path(id);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(self.unknown(id)),
            Err(e) => bail!("Cannot read {}: {e}.", path.display()),
        };
        let mut drawing: Drawing = serde_json::from_str(&text)
            .map_err(|e| anyhow!("{} is not a valid drawing ({e}).", path.display()))?;
        drawing.id = id.to_string();
        Ok(drawing)
    }

    /// The error for an id that has no drawing: names the 5 most recent ones.
    pub fn unknown(&self, id: &str) -> anyhow::Error {
        let recent: Vec<String> = self
            .list()
            .unwrap_or_default()
            .into_iter()
            .take(RECENT_SHOWN)
            .map(|s| s.id)
            .collect();
        if recent.is_empty() {
            anyhow!("No drawing named {id}, and there are no drawings yet. Run `prins new` to start one.")
        } else {
            anyhow!(
                "No drawing named {id}. Most recent: {}. Run `prins new` to start another.",
                recent.join(", ")
            )
        }
    }

    /// Lock a drawing (waiting if another process holds it) and read it.
    pub fn open_locked(&self, id: &str) -> Result<Locked> {
        check_id(id)?;
        if !self.path(id).is_file() {
            return Err(self.unknown(id));
        }
        let lock = self.open_lock(id)?;
        lock.lock()
            .with_context(|| format!("Cannot lock drawing {id}"))?;
        // Read after the lock, so a writer that finished while we waited is seen.
        let drawing = self.read(id)?;
        Ok(Locked {
            drawing,
            path: self.path(id),
            _lock: lock,
        })
    }

    /// Read-modify-write under the lock. The file is written only when `f` succeeds and changed
    /// the ops or the printed count; a failed call leaves it exactly as it was.
    pub fn update<T>(&self, id: &str, f: impl FnOnce(&mut Drawing) -> Result<T>) -> Result<T> {
        let mut locked = self.open_locked(id)?;
        let before = (locked.drawing.ops.clone(), locked.drawing.printed);
        let out = f(&mut locked.drawing)?;
        if (&locked.drawing.ops, locked.drawing.printed) != (&before.0, before.1) {
            locked.save()?;
        }
        Ok(out)
    }

    /// Replay a drawing's ops into a fresh canvas (external pictures allowed, the sender set).
    pub fn canvas(&self, drawing: &Drawing) -> Result<Canvas> {
        replay(&drawing.id, &drawing.ops, &self.sender())
    }

    /// All drawings, newest first (by `updated`, then id).
    pub fn list(&self) -> Result<Vec<Summary>> {
        let dir = self.drawings_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => bail!("Cannot read {}: {e}.", dir.display()),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            let Ok(drawing) = self.read(id) else { continue };
            out.push(Summary {
                id: id.to_string(),
                ops: drawing.ops.len(),
                created: drawing.created,
                updated: drawing.updated,
                printed: drawing.printed,
            });
        }
        out.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    /// Delete drawings not updated for `days` days. A drawing that is locked right now is
    /// skipped. Returns the ids removed.
    pub fn prune(&self, days: u64) -> Result<Vec<String>> {
        self.prune_at(days, now())
    }

    pub fn prune_at(&self, days: u64, now: u64) -> Result<Vec<String>> {
        let cutoff = now.saturating_sub(days.saturating_mul(86_400));
        let mut removed = Vec::new();
        for s in self.list()? {
            if s.updated >= cutoff {
                continue;
            }
            let lock = self.open_lock(&s.id)?;
            if lock.try_lock().is_err() {
                continue;
            }
            fs::remove_file(self.path(&s.id))
                .with_context(|| format!("Cannot remove drawing {}", s.id))?;
            let _ = fs::remove_file(self.lock_path(&s.id));
            removed.push(s.id);
        }
        Ok(removed)
    }
}

/// Replay ops into a fresh canvas.
pub fn replay(id: &str, ops: &[Value], sender: &str) -> Result<Canvas> {
    let mut canvas = Canvas::new(true, sender);
    canvas
        .apply_all(ops)
        .map_err(|e| anyhow!("Drawing {id} no longer replays: {e}"))?;
    Ok(canvas)
}

/// Write the drawing as compact JSON to a temp file next to `path`, then rename it into place.
fn write_atomic(path: &Path, drawing: &Drawing) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir.display()))?;
    let mut text = serde_json::to_string(drawing).context("Cannot encode the drawing")?;
    text.push('\n');
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "drawing.json".into());
    let tmp = dir.join(format!(
        ".{stem}.tmp{}-{}",
        std::process::id(),
        random_u64()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        bail!("Cannot write {}: {e}.", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn temp_store(tag: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "prins-store-{tag}-{}-{}",
            std::process::id(),
            random_u64()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (Store::new(&dir), dir)
    }

    #[test]
    fn ids_are_validated() {
        for ok in ["warm-fox", "a", "x1-y2", &"a".repeat(40)] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in [
            "",
            "Warm",
            "../x",
            "a/b",
            "a b",
            "a.json",
            "x_y",
            "é",
            &"a".repeat(41),
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
        let (store, dir) = temp_store("invalid");
        assert!(store.read("../etc/passwd").is_err());
        assert!(store.open_locked("../x").is_err());
        assert!(store.create(Some("../x")).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wordlists_have_no_duplicates() {
        assert_eq!(ADJECTIVES.iter().collect::<HashSet<_>>().len(), 64);
        assert_eq!(NOUNS.iter().collect::<HashSet<_>>().len(), 64);
        assert!(ADJECTIVES.iter().chain(NOUNS.iter()).all(|w| valid_id(w)));
    }

    #[test]
    fn generated_ids_are_valid_and_unique_in_a_folder() {
        let (store, dir) = temp_store("ids");
        let mut seen = HashSet::new();
        for _ in 0..300 {
            let id = store.create(None).unwrap();
            assert!(valid_id(&id), "{id}");
            assert!(id.contains('-'));
            assert!(seen.insert(id), "collision");
            // The file must be a valid, empty drawing at once.
        }
        assert_eq!(store.list().unwrap().len(), 300);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn named_drawings_must_be_free() {
        let (store, dir) = temp_store("named");
        assert_eq!(store.create(Some("door-sign")).unwrap(), "door-sign");
        let err = store.create(Some("door-sign")).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn round_trip_keeps_ops_and_counters() {
        let (store, dir) = temp_store("roundtrip");
        let id = store.create(None).unwrap();
        let fresh = store.read(&id).unwrap();
        assert_eq!(fresh.ops.len(), 0);
        assert_eq!(fresh.printed, 0);
        assert_eq!(fresh.id, id);
        store
            .update(&id, |d| {
                d.ops.push(json!({"op": "text", "s": "héllo ✓"}));
                d.printed = 2;
                Ok(())
            })
            .unwrap();
        let back = store.read(&id).unwrap();
        assert_eq!(back.ops, vec![json!({"op": "text", "s": "héllo ✓"})]);
        assert_eq!(back.printed, 2);
        assert_eq!(back.created, fresh.created);
        let raw: Value =
            serde_json::from_str(&fs::read_to_string(store.path(&id)).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);
        assert!(raw.get("id").is_none());
        assert_eq!(raw["ops"][0]["s"], "héllo ✓");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_update_leaves_the_file_alone() {
        let (store, dir) = temp_store("failed");
        let id = store.create(None).unwrap();
        store
            .update(&id, |d| {
                d.ops.push(json!({"op": "text", "s": "keep"}));
                Ok(())
            })
            .unwrap();
        let before = fs::read(store.path(&id)).unwrap();
        let err = store.update(&id, |d| -> Result<()> {
            d.ops.clear();
            bail!("nope")
        });
        assert!(err.is_err());
        assert_eq!(fs::read(store.path(&id)).unwrap(), before);
        // A successful call that changes nothing does not rewrite it either.
        store.update(&id, |_| Ok(())).unwrap();
        assert_eq!(fs::read(store.path(&id)).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_id_lists_the_five_most_recent() {
        let (store, dir) = temp_store("unknown");
        let err = store.read("nothing-here").unwrap_err().to_string();
        assert!(
            err.contains("no drawings yet") && err.contains("prins new"),
            "{err}"
        );
        for n in 0..7u64 {
            let id = format!("d{n}");
            store.create(Some(&id)).unwrap();
            let mut locked = store.open_locked(&id).unwrap();
            locked.drawing.updated = 1_000 + n;
            write_atomic(&locked.path, &locked.drawing).unwrap();
        }
        let err = store.read("nothing-here").unwrap_err().to_string();
        assert!(err.contains("nothing-here"), "{err}");
        assert!(err.contains("d6, d5, d4, d3, d2"), "{err}");
        assert!(!err.contains("d1"), "{err}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_save_leaves_no_temp_file() {
        let (store, dir) = temp_store("atomic");
        let id = store.create(Some("one")).unwrap();
        fs::write(store.path(&id), "x".repeat(10_000)).unwrap(); // longer than the new content
        let err = store.read(&id).unwrap_err().to_string();
        assert!(err.contains("not a valid drawing"), "{err}");
        let d = Drawing {
            id: id.clone(),
            version: 1,
            created: 1,
            updated: 1,
            printed: 0,
            ops: vec![json!({"op": "clear"})],
        };
        write_atomic(&store.path(&id), &d).unwrap();
        assert_eq!(store.read(&id).unwrap().ops.len(), 1);
        let names: HashSet<String> = fs::read_dir(store.drawings_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| n == "one.json" || n == "one.lock"),
            "{names:?}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn two_threads_on_one_id_never_lose_an_update() {
        let (store, dir) = temp_store("contention");
        let id = store.create(Some("shared")).unwrap();
        let store = Arc::new(store);
        let handles: Vec<_> = (0..2)
            .map(|t| {
                let store = Arc::clone(&store);
                let id = id.clone();
                std::thread::spawn(move || {
                    for n in 0..40 {
                        store
                            .update(&id, |d| {
                                d.ops.push(json!({"op": "text", "s": format!("{t}-{n}")}));
                                d.printed += 1;
                                Ok(())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let d = store.read(&id).unwrap();
        assert_eq!(d.ops.len(), 80);
        assert_eq!(d.printed, 80);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn different_ids_do_not_block_each_other() {
        let (store, dir) = temp_store("independent");
        store.create(Some("a")).unwrap();
        store.create(Some("b")).unwrap();
        let held = store.open_locked("a").unwrap();
        // With "a" locked, "b" must still be writable (this would hang if locks were shared).
        store
            .update("b", |d| {
                d.printed = 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(held.drawing.printed, 0);
        drop(held);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_removes_only_old_drawings() {
        let (store, dir) = temp_store("prune");
        for id in ["old", "fresh"] {
            store.create(Some(id)).unwrap();
        }
        let mut old = store.open_locked("old").unwrap();
        old.drawing.updated = 100;
        write_atomic(&old.path, &old.drawing).unwrap();
        drop(old);
        let day = 86_400;
        let now = 100 + 10 * day;
        let mut fresh = store.open_locked("fresh").unwrap();
        fresh.drawing.updated = now - day;
        write_atomic(&fresh.path, &fresh.drawing).unwrap();
        drop(fresh);
        assert_eq!(store.prune_at(30, now).unwrap(), Vec::<String>::new());
        assert_eq!(store.prune_at(5, now).unwrap(), vec!["old".to_string()]);
        assert!(!store.exists("old"));
        assert!(!store.drawings_dir().join("old.lock").exists());
        assert!(store.exists("fresh"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn list_is_newest_first() {
        let (store, dir) = temp_store("list");
        for (id, t) in [("a", 5u64), ("b", 9), ("c", 7)] {
            store.create(Some(id)).unwrap();
            let mut l = store.open_locked(id).unwrap();
            l.drawing.updated = t;
            write_atomic(&l.path, &l.drawing).unwrap();
        }
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["b", "c", "a"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_sender_comes_from_the_config_file_and_keeps_other_keys() {
        let (store, dir) = temp_store("sender");
        assert_eq!(config_name(store.home()), None);
        fs::write(
            store.home().join("config.json"),
            r#"{"name": " Anders ", "printer": {"id": "A"}}"#,
        )
        .unwrap();
        assert_eq!(config_name(store.home()).as_deref(), Some("Anders"));
        let _ = fs::remove_dir_all(dir);
    }
}
