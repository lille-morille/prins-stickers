//! Bluetooth Low Energy: scanning for printers and picking one by id or name.
//!
//! On macOS (CoreBluetooth) a peripheral's id is a UUID that is only meaningful on this Mac, not a
//! MAC address, so a saved printer is stored as whatever `peripheral.id()` prints plus the
//! advertised name, and resolved by id first, then by name (see [`resolve_index`]).
//!
//! Everything that talks to the radio is async (`*_async`) with a blocking wrapper next to it. The
//! wrappers work both outside a tokio runtime and inside one (see [`block_on`]).

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use serde::Serialize;
use uuid::Uuid;

use crate::config::PrinterRef;

/// The M110's service (`0xFF00`).
pub const PRINTER_SERVICE: Uuid = Uuid::from_u128(0x0000ff00_0000_1000_8000_00805f9b34fb);
/// Write (without response) the printer commands here.
pub const WRITE_UUID: Uuid = Uuid::from_u128(0x0000ff02_0000_1000_8000_00805f9b34fb);
/// The printer's replies arrive here as notifications.
pub const NOTIFY_UUID: Uuid = Uuid::from_u128(0x0000ff03_0000_1000_8000_00805f9b34fb);

/// A Bluetooth device seen during a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Found {
    /// `peripheral.id()` as text: a UUID on macOS, a MAC address on Linux and Windows.
    pub id: String,
    /// The advertised local name, if any.
    pub name: Option<String>,
    /// Signal strength in dBm (closer to 0 is nearer), if the platform reports it.
    pub rssi: Option<i16>,
    #[serde(serialize_with = "uuids_as_text")]
    pub services: Vec<Uuid>,
    /// True when it advertises service 0xFF00 or its name looks like an M110's.
    pub likely_printer: bool,
}

fn uuids_as_text<S: serde::Serializer>(uuids: &[Uuid], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(uuids.iter().map(|u| u.to_string()))
}

impl Found {
    /// `Q199E63C4390578 (id 7D3C...)`, or the id alone for a nameless device.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} (id {})", self.id),
            None => format!("(no name) (id {})", self.id),
        }
    }
}

/// Does this device look like an M110? Service 0xFF00, or a name that is `Q1` plus at least six hex
/// digits (as in `Q199E63C4390578`), or one that contains `M110` or `Phomemo` in any case.
pub fn is_likely_printer(name: Option<&str>, services: &[Uuid]) -> bool {
    if services.contains(&PRINTER_SERVICE) {
        return true;
    }
    let Some(name) = name else { return false };
    let lower = name.to_ascii_lowercase();
    if lower.contains("m110") || lower.contains("phomemo") {
        return true;
    }
    match name.strip_prefix("Q1").or_else(|| name.strip_prefix("q1")) {
        Some(rest) => rest.len() >= 6 && rest.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
}

/// Likely printers first, then the strongest signal first (devices without a signal last), then by
/// name and id so the order is stable.
pub fn sort_found(found: &mut [Found]) {
    found.sort_by(|a, b| {
        b.likely_printer
            .cmp(&a.likely_printer)
            .then_with(|| b.rssi.unwrap_or(i16::MIN).cmp(&a.rssi.unwrap_or(i16::MIN)))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Index of the device a saved printer refers to: the one with the same id, else the one with the
/// same name. For a printer set through `PRINS_PRINTER` (which has no name) the id text is also
/// tried as a name. Comparisons ignore case.
pub fn resolve_index(found: &[Found], printer: &PrinterRef) -> Option<usize> {
    if let Some(i) = found
        .iter()
        .position(|f| f.id.eq_ignore_ascii_case(&printer.id))
    {
        return Some(i);
    }
    let wanted = printer.name.as_deref().unwrap_or(&printer.id);
    found.iter().position(|f| {
        f.name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
    })
}

/// Why [`pick`] did not return a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickError {
    Empty,
    NotFound(String),
    Ambiguous(String, Vec<Found>),
}

impl fmt::Display for PickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PickError::Empty => write!(
                f,
                "Give the id or name of a printer. Run `prins printer scan` to list them."
            ),
            PickError::NotFound(wanted) => write!(
                f,
                "No device in the scan matches {wanted:?}. Run `prins printer scan` and copy an id from the list."
            ),
            PickError::Ambiguous(wanted, candidates) => {
                writeln!(f, "{wanted:?} matches more than one device. Nothing was saved. Use one of these ids:")?;
                for c in candidates {
                    writeln!(f, "  {}", c.label())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PickError {}

/// Choose one scanned device from what the user typed: an exact id, else an exact name, else a
/// name prefix that matches only one device. Ignores case. Several matches at the same step are an
/// error that lists them, never a guess.
pub fn pick<'a>(found: &'a [Found], wanted: &str) -> Result<&'a Found, PickError> {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        return Err(PickError::Empty);
    }
    let steps: [&dyn Fn(&Found) -> bool; 3] = [
        &|f| f.id.eq_ignore_ascii_case(wanted),
        &|f| {
            f.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
        },
        &|f| {
            f.name.as_deref().is_some_and(|n| {
                n.len() >= wanted.len()
                    && n.is_char_boundary(wanted.len())
                    && n[..wanted.len()].eq_ignore_ascii_case(wanted)
            })
        },
    ];
    for matches in steps {
        let hits: Vec<&Found> = found.iter().filter(|f| matches(f)).collect();
        match hits.len() {
            0 => continue,
            1 => return Ok(hits[0]),
            _ => {
                return Err(PickError::Ambiguous(
                    wanted.to_string(),
                    hits.into_iter().cloned().collect(),
                ))
            }
        }
    }
    Err(PickError::NotFound(wanted.to_string()))
}

/// Run a future to completion from synchronous code. Inside a tokio runtime (where `block_on`
/// would panic) it runs on a helper thread with its own runtime.
pub fn block_on<F>(future: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    fn run<F: Future>(future: F) -> F::Output {
        match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(future),
            Err(e) => panic!("cannot start the async runtime: {e}"),
        }
    }
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| match s.spawn(|| run(future)).join() {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        })
    } else {
        run(future)
    }
}

/// Open the first Bluetooth adapter, with a hint when there is none.
pub(crate) async fn adapter() -> Result<Adapter> {
    let manager = Manager::new().await.map_err(|e| {
        anyhow!("Cannot use Bluetooth ({e}). Turn Bluetooth on, and on macOS allow Bluetooth for this terminal in System Settings > Privacy & Security > Bluetooth.")
    })?;
    let adapters = manager
        .adapters()
        .await
        .context("Cannot list Bluetooth adapters")?;
    adapters.into_iter().next().ok_or_else(|| {
        anyhow!("No Bluetooth adapter found. Turn Bluetooth on, and on macOS allow Bluetooth for this terminal in System Settings > Privacy & Security > Bluetooth.")
    })
}

/// What the adapter has seen so far, each device with its handle.
pub(crate) async fn snapshot(adapter: &Adapter) -> Result<Vec<(Found, Peripheral)>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in adapter
        .peripherals()
        .await
        .context("Cannot read scan results")?
    {
        let id = p.id().to_string();
        if !seen.insert(id.clone()) {
            continue;
        }
        let props = p.properties().await.ok().flatten();
        let (name, rssi, services) = match props {
            Some(props) => (
                props.local_name.or(props.advertisement_name),
                props.rssi,
                props.services,
            ),
            None => (None, None, Vec::new()),
        };
        let name = name.filter(|n| !n.trim().is_empty());
        let likely_printer = is_likely_printer(name.as_deref(), &services);
        out.push((
            Found {
                id,
                name,
                rssi,
                services,
                likely_printer,
            },
            p,
        ));
    }
    Ok(out)
}

/// Listen for `timeout` and return every device heard, likely printers first, then by signal.
/// Read-only: it connects to nothing and saves nothing.
pub async fn scan_async(timeout: Duration) -> Result<Vec<Found>> {
    let adapter = adapter().await?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("Cannot start the Bluetooth scan")?;
    tokio::time::sleep(timeout).await;
    let result = snapshot(&adapter).await;
    let _ = adapter.stop_scan().await;
    let mut found: Vec<Found> = result?.into_iter().map(|(f, _)| f).collect();
    sort_found(&mut found);
    Ok(found)
}

/// Blocking version of [`scan_async`].
pub fn scan(timeout: Duration) -> Result<Vec<Found>> {
    block_on(scan_async(timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: Option<&str>, rssi: Option<i16>) -> Found {
        let services = Vec::new();
        Found {
            id: id.into(),
            name: name.map(String::from),
            rssi,
            likely_printer: is_likely_printer(name, &services),
            services,
        }
    }

    #[test]
    fn the_known_printer_name_is_likely() {
        assert!(is_likely_printer(Some("Q199E63C4390578"), &[]));
        assert!(is_likely_printer(Some("Q1ABCDEF"), &[]));
        assert!(is_likely_printer(Some("q1abcdef12"), &[]));
    }

    #[test]
    fn m110_and_phomemo_names_are_likely() {
        assert!(is_likely_printer(Some("M110"), &[]));
        assert!(is_likely_printer(Some("Phomemo T02"), &[]));
        assert!(is_likely_printer(Some("my-m110-printer"), &[]));
    }

    #[test]
    fn the_printer_service_is_likely_whatever_the_name() {
        assert!(is_likely_printer(None, &[PRINTER_SERVICE]));
        assert!(is_likely_printer(
            Some("Whatever"),
            &[Uuid::nil(), PRINTER_SERVICE]
        ));
    }

    #[test]
    fn other_devices_are_not_likely() {
        assert!(!is_likely_printer(None, &[]));
        assert!(!is_likely_printer(Some("Q1"), &[]));
        assert!(!is_likely_printer(Some("Q1XYZ123456"), &[])); // not hex
        assert!(!is_likely_printer(Some("Q1AB"), &[])); // too short to mean anything
        assert!(!is_likely_printer(Some("iPhone"), &[]));
        assert!(!is_likely_printer(
            Some("AirPods Pro"),
            &[Uuid::from_u128(0x180f)]
        ));
        assert!(!is_likely_printer(Some("Q2199E63C"), &[]));
    }

    #[test]
    fn the_service_uuid_is_ff00() {
        assert_eq!(
            PRINTER_SERVICE.to_string(),
            "0000ff00-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            WRITE_UUID.to_string(),
            "0000ff02-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            NOTIFY_UUID.to_string(),
            "0000ff03-0000-1000-8000-00805f9b34fb"
        );
    }

    #[test]
    fn sorting_puts_printers_first_then_strong_signals() {
        let mut v = vec![
            dev("a", Some("Speaker"), Some(-30)),
            dev("b", None, None),
            dev("c", Some("Q199E63C4390578"), Some(-80)),
            dev("d", Some("Watch"), Some(-60)),
            dev("e", Some("M110"), Some(-50)),
        ];
        sort_found(&mut v);
        let ids: Vec<_> = v.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["e", "c", "a", "d", "b"]);
    }

    #[test]
    fn a_saved_printer_resolves_by_id_before_name() {
        let found = vec![
            dev("ID-2", Some("Q1AAAAAA"), None),
            dev("ID-1", Some("Q1BBBBBB"), None),
        ];
        let by_both = PrinterRef {
            id: "ID-1".into(),
            name: Some("Q1AAAAAA".into()),
        };
        assert_eq!(resolve_index(&found, &by_both), Some(1)); // id wins
        let renamed_id = PrinterRef {
            id: "OLD".into(),
            name: Some("q1aaaaaa".into()),
        };
        assert_eq!(resolve_index(&found, &renamed_id), Some(0)); // falls back to the name
        let env_style = PrinterRef {
            id: "Q1BBBBBB".into(),
            name: None,
        };
        assert_eq!(resolve_index(&found, &env_style), Some(1)); // id text tried as a name
        let gone = PrinterRef {
            id: "X".into(),
            name: Some("Y".into()),
        };
        assert_eq!(resolve_index(&found, &gone), None);
    }

    #[test]
    fn pick_prefers_id_then_exact_name_then_unique_prefix() {
        let found = vec![
            dev("7D3C-1", Some("Q199E63C4390578"), None),
            dev("7D3C-2", Some("Q199E63CFFFFFFF"), None),
            dev("AAAA", Some("Q199E63C"), None),
            dev("BBBB", None, None),
        ];
        assert_eq!(pick(&found, "7d3c-2").unwrap().id, "7D3C-2");
        assert_eq!(pick(&found, "Q199E63C").unwrap().id, "AAAA"); // exact name beats the prefixes
        assert_eq!(pick(&found, "q199e63c43").unwrap().id, "7D3C-1"); // unique prefix
        assert_eq!(pick(&found, "BBBB").unwrap().id, "BBBB");
    }

    #[test]
    fn pick_refuses_ambiguous_prefixes_and_lists_the_candidates() {
        let found = vec![
            dev("1", Some("Q1AAAAAA01"), None),
            dev("2", Some("Q1AAAAAA02"), None),
        ];
        let err = pick(&found, "Q1AAAAAA").unwrap_err();
        match &err {
            PickError::Ambiguous(_, c) => assert_eq!(c.len(), 2),
            other => panic!("{other:?}"),
        }
        let text = err.to_string();
        assert!(
            text.contains("Nothing was saved")
                && text.contains("Q1AAAAAA01")
                && text.contains("id 2"),
            "{text}"
        );
    }

    #[test]
    fn pick_refuses_two_devices_with_the_same_name() {
        let found = vec![
            dev("1", Some("Q1AAAAAA"), None),
            dev("2", Some("Q1AAAAAA"), None),
        ];
        assert!(matches!(
            pick(&found, "Q1AAAAAA"),
            Err(PickError::Ambiguous(..))
        ));
        assert_eq!(pick(&found, "2").unwrap().id, "2");
    }

    #[test]
    fn pick_refuses_empty_and_unknown_arguments() {
        let found = vec![dev("1", Some("Q1AAAAAA"), None)];
        assert_eq!(pick(&found, "  "), Err(PickError::Empty));
        assert_eq!(pick(&found, ""), Err(PickError::Empty));
        assert!(matches!(
            pick(&found, "nothing"),
            Err(PickError::NotFound(_))
        ));
        assert!(matches!(pick(&[], "Q1"), Err(PickError::NotFound(_))));
    }

    #[test]
    fn pick_does_not_panic_on_odd_text() {
        let found = vec![dev("1", Some("Q1\u{e9}\u{e9}\u{e9}"), None)];
        assert!(pick(&found, "Q1\u{e9}").is_ok());
        assert!(pick(
            &found,
            "\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}"
        )
        .is_err());
    }

    #[test]
    fn block_on_works_inside_and_outside_a_runtime() {
        assert_eq!(block_on(async { 1 + 1 }), 2);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let v = rt.block_on(async { block_on(async { 3 }) });
        assert_eq!(v, 3);
    }
}
