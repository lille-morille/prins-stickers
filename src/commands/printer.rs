//! `prins printer ...`: find, choose and test the label printer.
//!
//! The flow an agent follows: `scan`, show the list to the user, ask which device is the printer,
//! and only then `save <id>`. Nothing here saves a printer unless `save` is given an explicit id
//! or name. Messages are short plain sentences because agents read them.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;

use crate::ble::{self, Found, PickError};
use crate::config::{self, Config, PrinterRef, Source};
use crate::m110::{self, M110Error, PrintOptions, PrintReport};

/// How long `save` listens when it checks the device against a fresh scan.
const SAVE_SCAN_SECONDS: u64 = 6;
/// Other (not printer) devices shown in the text list before it says how many more there are.
const OTHER_DEVICES_SHOWN: usize = 15;

const NO_PRINTER: &str =
    "No printer saved. Run `prins printer scan`, then `prins printer save <id>`.";

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum PrinterCmd {
    /// List nearby Bluetooth devices, likely printers first. Saves nothing.
    ///
    /// Read-only: it only listens. Show the list to the user and ask which device is their
    /// printer, then run `prins printer save <id>`. The printer must be on and not connected to
    /// the phone app.
    Scan {
        /// How long to listen, in seconds.
        #[arg(long, default_value_t = 6)]
        seconds: u64,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Save the printer the user chose. Needs an explicit id or name.
    ///
    /// The argument is matched against a fresh scan: id first, then exact name, then a name
    /// prefix that matches only one device. Ambiguous arguments are refused and the candidates are
    /// listed. Writes ~/.prins/config.json.
    Save {
        /// The id or name from `prins printer scan`.
        target: String,
        /// Also save a device that does not look like an M110 (no 0xFF00 service, name not
        /// like Q199E63C4390578).
        #[arg(long)]
        yes: bool,
        /// Save the id as given, without scanning. For a printer that is off right now.
        #[arg(long)]
        no_verify: bool,
    },
    /// Show the saved printer, and where it came from.
    Show,
    /// Remove the saved printer from the config file.
    Forget,
    /// Print ONE test label (a bordered pattern). With --dry-run nothing is sent.
    Test {
        /// Report what would be sent, and do not touch the printer.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print ONE calibration label (a full-width frame with ticks). With --dry-run nothing is sent.
    Calibrate {
        /// Report what would be sent, and do not touch the printer.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Run one subcommand. Output goes to stdout; failures are errors whose text says what to do next.
pub fn run(cmd: PrinterCmd) -> Result<()> {
    match cmd {
        PrinterCmd::Scan { seconds, json } => scan(seconds, json),
        PrinterCmd::Save {
            target,
            yes,
            no_verify,
        } => save(&target, yes, no_verify),
        PrinterCmd::Show => show(),
        PrinterCmd::Forget => forget(),
        PrinterCmd::Test { dry_run } => print_image(m110::test_pattern(), "test", dry_run),
        PrinterCmd::Calibrate { dry_run } => print_image(
            m110::calibration_image(m110::WIDTH_BYTES),
            "calibration",
            dry_run,
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// scan

fn scan(seconds: u64, json: bool) -> Result<()> {
    let seconds = seconds.clamp(1, 60);
    let found = ble::scan(Duration::from_secs(seconds))?;
    let saved = config::printer();
    if json {
        println!("{}", scan_json(&found, saved.as_ref(), seconds));
    } else {
        print!("{}", scan_text(&found, saved.as_ref(), seconds));
    }
    Ok(())
}

fn scan_json(found: &[Found], saved: Option<&PrinterRef>, seconds: u64) -> String {
    let value = serde_json::json!({
        "seconds": seconds,
        "saved_printer": saved,
        "devices": found,
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
}

fn signal_word(rssi: Option<i16>) -> String {
    match rssi {
        Some(r) if r >= -60 => format!("{r} dBm, strong"),
        Some(r) if r >= -80 => format!("{r} dBm, medium"),
        Some(r) => format!("{r} dBm, weak"),
        None => "no signal reading".into(),
    }
}

/// Why a device is listed as a likely printer, so the user can judge a doubtful one.
fn why_likely(f: &Found) -> &'static str {
    let by_name = ble::is_likely_printer(f.name.as_deref(), &[]);
    let by_service = f.services.contains(&ble::PRINTER_SERVICE);
    match (by_name, by_service) {
        (true, true) => "the name looks like an M110's and it offers the printer service",
        (true, false) => "the name looks like an M110's",
        _ => "it offers the printer service (0xFF00), but the name is not an M110's",
    }
}

fn is_saved(device: &Found, saved: Option<&PrinterRef>) -> bool {
    saved.is_some_and(|s| ble::resolve_index(std::slice::from_ref(device), s) == Some(0))
}

fn scan_text(found: &[Found], saved: Option<&PrinterRef>, seconds: u64) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let likely: Vec<&Found> = found.iter().filter(|f| f.likely_printer).collect();
    let others: Vec<&Found> = found.iter().filter(|f| !f.likely_printer).collect();

    if found.is_empty() {
        let _ = writeln!(out, "No Bluetooth devices found in {seconds} s.");
        let _ = writeln!(out, "Check that:");
        let _ = writeln!(
            out,
            "  - the printer is switched on, and not connected to the phone app;"
        );
        let _ = writeln!(out, "  - Bluetooth is on;");
        let _ = writeln!(
            out,
            "  - on macOS, this terminal may use Bluetooth (System Settings > Privacy & Security > Bluetooth)."
        );
        let _ = writeln!(
            out,
            "Then run `prins printer scan` again, or add `--seconds 15`."
        );
        return out;
    }

    let _ = writeln!(
        out,
        "Heard {} Bluetooth device{} in {seconds} s. {} look{} like the label printer.",
        found.len(),
        if found.len() == 1 { "" } else { "s" },
        if likely.is_empty() {
            "None".to_string()
        } else {
            likely.len().to_string()
        },
        if likely.len() == 1 { "s" } else { "" },
    );
    if likely.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "No device looks like an M110. Check that the printer is on and not connected to the phone app, then scan again.");
    } else {
        let _ = writeln!(out);
        let _ = writeln!(out, "Likely printers:");
        for f in &likely {
            let name = f.name.as_deref().unwrap_or("(no name)");
            let mark = if is_saved(f, saved) { "  [saved]" } else { "" };
            let _ = writeln!(out, "  * {name}{mark}");
            let _ = writeln!(out, "      id:     {}", f.id);
            let _ = writeln!(out, "      signal: {}", signal_word(f.rssi));
            let _ = writeln!(out, "      why:    {}", why_likely(f));
            let _ = writeln!(out, "      save:   prins printer save {}", f.id);
        }
    }
    // Most devices around have no name; listing them only hides the ones that matter.
    let named: Vec<&Found> = others
        .iter()
        .copied()
        .filter(|f| f.name.is_some())
        .collect();
    let nameless = others.len() - named.len();
    if !named.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Other devices with a name ({}), strongest first:",
            named.len()
        );
        for f in named.iter().take(OTHER_DEVICES_SHOWN) {
            let name = f.name.as_deref().unwrap_or("(no name)");
            let mark = if is_saved(f, saved) { "  [saved]" } else { "" };
            let _ = writeln!(
                out,
                "    {name}{mark}   id {}   {}",
                f.id,
                signal_word(f.rssi)
            );
        }
        if named.len() > OTHER_DEVICES_SHOWN {
            let _ = writeln!(
                out,
                "    ... and {} more (see `--json`).",
                named.len() - OTHER_DEVICES_SHOWN
            );
        }
    }
    if nameless > 0 {
        let _ = writeln!(
            out,
            "{}{nameless} more device{} no name (see `--json`).",
            if named.is_empty() { "\n" } else { "" },
            if nameless == 1 { " has" } else { "s have" }
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Nothing was saved. Ask the user which device is their printer, then run `prins printer save <id>`.");
    out
}

// ---------------------------------------------------------------------------------------------
// save

/// Choose the device to save from a scan and what the user typed. Refuses an empty argument, an
/// unknown or ambiguous one, and (without `yes`) a device that does not look like an M110.
fn choose_to_save<'a>(found: &'a [Found], target: &str, yes: bool) -> Result<&'a Found> {
    let device = match ble::pick(found, target) {
        Ok(d) => d,
        Err(PickError::NotFound(wanted)) if found.is_empty() => bail!(
            "The scan heard no devices, so {wanted:?} cannot be checked. Check that the printer is on and not connected to the phone app, \
             and that this terminal may use Bluetooth, then try again. Or save the id without checking: `prins printer save {wanted} --no-verify`."
        ),
        Err(e) => bail!("{e}"),
    };
    if !device.likely_printer && !yes {
        bail!(
            "{} does not look like an M110 (no printer service, and the name is not like Q199E63C4390578). Nothing was saved. \
             If the user is sure, run `prins printer save {} --yes`.",
            device.label(),
            device.id
        );
    }
    Ok(device)
}

fn saved_message(printer: &PrinterRef, file: &Path, verified: bool) -> String {
    let mut out = format!(
        "Saved printer {}.\n",
        printer.name.as_deref().unwrap_or(&printer.id)
    );
    out.push_str(&format!("  id:   {}\n", printer.id));
    out.push_str(&format!(
        "  name: {}\n",
        printer.name.as_deref().unwrap_or("(none saved)")
    ));
    out.push_str(&format!("  file: {}\n", file.display()));
    if !verified {
        out.push_str("It was not checked against a scan. `prins printer scan` shows whether it is in range.\n");
    }
    out.push_str(
        "Next: `prins printer test --dry-run`, or `prins printer test` to print one label.\n",
    );
    out
}

fn save(target: &str, yes: bool, no_verify: bool) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        bail!("{}", PickError::Empty);
    }
    let printer = if no_verify {
        PrinterRef {
            id: target.to_string(),
            name: None,
        }
    } else {
        let found = ble::scan(Duration::from_secs(SAVE_SCAN_SECONDS))?;
        let device = choose_to_save(&found, target, yes)?;
        PrinterRef {
            id: device.id.clone(),
            name: device.name.clone(),
        }
    };
    let mut config = Config::load()?;
    config.printer = Some(printer.clone());
    config.save()?;
    print!("{}", saved_message(&printer, &config::path(), !no_verify));
    if let Some(env) = config::env_printer() {
        println!(
            "Note: PRINS_PRINTER is set to {:?} in this shell and overrides the saved printer.",
            env.id
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// show, forget

fn show_text(
    effective: Option<(PrinterRef, Source)>,
    file: &Path,
    file_printer: Option<&PrinterRef>,
) -> String {
    let mut out = String::new();
    match effective {
        None => {
            out.push_str(NO_PRINTER);
            out.push('\n');
        }
        Some((printer, source)) => {
            out.push_str(&format!("Printer: {}\n", printer.describe()));
            match source {
                Source::File => out.push_str(&format!("Source:  config file {}\n", file.display())),
                Source::Env => {
                    out.push_str(
                        "Source:  environment variable PRINS_PRINTER (for this run only)\n",
                    );
                    if let Some(f) = file_printer {
                        out.push_str(&format!("The config file has: {}\n", f.describe()));
                    }
                }
            }
        }
    }
    out.push_str(&format!(
        "Config file: {} ({})\n",
        file.display(),
        if file.exists() {
            "exists"
        } else {
            "does not exist"
        }
    ));
    out
}

fn show() -> Result<()> {
    let file = config::path();
    let file_printer = Config::load()?.printer;
    let effective = config::printer_with_source();
    print!("{}", show_text(effective, &file, file_printer.as_ref()));
    Ok(())
}

fn forget() -> Result<()> {
    let file = config::path();
    let mut config = Config::load()?;
    match config.printer.take() {
        None => println!(
            "No printer was saved in {}. Nothing changed.",
            file.display()
        ),
        Some(old) => {
            config.save()?;
            println!(
                "Removed the saved printer {} from {}.",
                old.describe(),
                file.display()
            );
        }
    }
    if let Some(env) = config::env_printer() {
        println!(
            "Note: PRINS_PRINTER is set to {:?} in this shell and still selects a printer.",
            env.id
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// test, calibrate

fn dry_run_text(what: &str, image: &image::GrayImage) -> String {
    let dry = m110::dry_run(image);
    format!(
        "Dry run: nothing was sent and no printer was touched.\n\
         The {what} image is {}x{}. It would go out as {} rows of {} px ({} bytes), {} black pixels.\n\
         Without --dry-run this prints ONE label.\n",
        image.width(),
        image.height(),
        dry.rows,
        dry.prepared.width(),
        dry.bytes,
        m110::black_pixels(&dry.prepared),
    )
}

fn seconds(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

fn print_report_text(report: &PrintReport) -> String {
    let replies = if report.replies.is_empty() {
        "none".to_string()
    } else {
        report.replies.join(" | ")
    };
    format!(
        "Sent {} bytes ({} rows) to {} in {} writes. It took {}.\n\
         Printer replies: {replies}\n\
         Done reply: {}\n\
         This only shows the data was sent. Look at the printer to see whether the label came out.\n",
        report.bytes,
        report.rows,
        report.printer_name.as_deref().unwrap_or(&report.printer_id),
        report.writes,
        seconds(report.timings.total_ms),
        if report.done {
            "yes"
        } else {
            "no (the printer may still have printed; this reply is only inferred)"
        },
    )
}

/// Turn a print failure into a message that says what to do next.
fn explain(err: anyhow::Error) -> anyhow::Error {
    if let Some(M110Error::NotFound(name)) = err.downcast_ref::<M110Error>() {
        return anyhow!(
            "Printer {name:?} was not found. Check that it is on and not connected to the phone app, and that it is near. \
             `prins printer scan` shows what is in range."
        );
    }
    err
}

fn print_image(image: image::GrayImage, what: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        print!("{}", dry_run_text(what, &image));
        return Ok(());
    }
    let printer = config::printer().ok_or_else(|| anyhow!(NO_PRINTER))?;
    println!(
        "Printing ONE {what} label on {}. This takes about 10 seconds.",
        printer.describe()
    );
    let report = m110::print(&image, &printer, PrintOptions::default()).map_err(explain)?;
    print!("{}", print_report_text(&report));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        cmd: PrinterCmd,
    }

    fn parse(args: &[&str]) -> Result<PrinterCmd, clap::Error> {
        let mut argv = vec!["prins-printer"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).map(|c| c.cmd)
    }

    fn dev(id: &str, name: Option<&str>, rssi: Option<i16>) -> Found {
        Found {
            id: id.into(),
            name: name.map(String::from),
            rssi,
            services: vec![],
            likely_printer: ble::is_likely_printer(name, &[]),
        }
    }

    #[test]
    fn save_needs_an_explicit_identifier() {
        assert!(parse(&["save"]).is_err()); // no argument: refused by the parser
        let err = parse(&["save"]).unwrap_err().to_string();
        assert!(err.contains("<TARGET>"), "{err}");
        assert_eq!(
            parse(&["save", "ABC"]).unwrap(),
            PrinterCmd::Save {
                target: "ABC".into(),
                yes: false,
                no_verify: false
            }
        );
        assert_eq!(
            parse(&["save", "ABC", "--no-verify", "--yes"]).unwrap(),
            PrinterCmd::Save {
                target: "ABC".into(),
                yes: true,
                no_verify: true
            }
        );
    }

    #[test]
    fn scan_never_takes_something_to_save() {
        assert!(parse(&["scan", "--save"]).is_err());
        assert!(parse(&["scan", "ABC"]).is_err());
        assert_eq!(
            parse(&["scan"]).unwrap(),
            PrinterCmd::Scan {
                seconds: 6,
                json: false
            }
        );
        assert_eq!(
            parse(&["scan", "--seconds", "10", "--json"]).unwrap(),
            PrinterCmd::Scan {
                seconds: 10,
                json: true
            }
        );
    }

    #[test]
    fn the_other_subcommands_parse() {
        assert_eq!(parse(&["show"]).unwrap(), PrinterCmd::Show);
        assert_eq!(parse(&["forget"]).unwrap(), PrinterCmd::Forget);
        assert_eq!(
            parse(&["test", "--dry-run"]).unwrap(),
            PrinterCmd::Test { dry_run: true }
        );
        assert_eq!(
            parse(&["test"]).unwrap(),
            PrinterCmd::Test { dry_run: false }
        );
        assert_eq!(
            parse(&["calibrate"]).unwrap(),
            PrinterCmd::Calibrate { dry_run: false }
        );
    }

    #[test]
    fn real_print_commands_warn_on_their_help() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        for name in ["test", "calibrate"] {
            let sub = cmd.find_subcommand(name).unwrap();
            let about = sub.get_about().unwrap().to_string();
            assert!(about.contains("ONE"), "{name}: {about}");
        }
    }

    #[test]
    fn choose_refuses_ambiguous_arguments_with_the_candidates() {
        let found = vec![
            dev("1", Some("Q1AAAAAA01"), None),
            dev("2", Some("Q1AAAAAA02"), None),
        ];
        let err = choose_to_save(&found, "Q1AAAAAA", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("more than one")
                && err.contains("Q1AAAAAA01")
                && err.contains("Q1AAAAAA02"),
            "{err}"
        );
        assert_eq!(choose_to_save(&found, "2", false).unwrap().id, "2");
    }

    #[test]
    fn choose_refuses_empty_and_unknown_arguments() {
        let found = vec![dev("1", Some("Q1AAAAAA"), None)];
        assert!(choose_to_save(&found, " ", false).is_err());
        let err = choose_to_save(&found, "zzz", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("prins printer scan"), "{err}");
        let err = choose_to_save(&[], "zzz", false).unwrap_err().to_string();
        assert!(
            err.contains("--no-verify") && err.contains("Bluetooth"),
            "{err}"
        );
    }

    #[test]
    fn a_device_that_is_not_a_printer_needs_yes() {
        let found = vec![
            dev("1", Some("Magic Keyboard"), Some(-40)),
            dev("2", Some("Q1AAAAAA"), None),
        ];
        let err = choose_to_save(&found, "1", false).unwrap_err().to_string();
        assert!(
            err.contains("does not look like an M110")
                && err.contains("--yes")
                && err.contains("Nothing was saved"),
            "{err}"
        );
        assert_eq!(choose_to_save(&found, "1", true).unwrap().id, "1");
        assert_eq!(choose_to_save(&found, "Q1AAAAAA", false).unwrap().id, "2");
    }

    #[test]
    fn the_scan_list_points_at_the_printer() {
        let mut found = vec![
            dev("ID-PRINTER", Some("Q199E63C4390578"), Some(-52)),
            dev("ID-KB", Some("Magic Keyboard"), Some(-40)),
            dev("ID-NONAME", None, None),
        ];
        ble::sort_found(&mut found);
        let text = scan_text(&found, None, 6);
        let printer_at = text.find("Q199E63C4390578").unwrap();
        let kb_at = text.find("Magic Keyboard").unwrap();
        assert!(printer_at < kb_at);
        assert!(
            text.contains("Likely printers:") && text.contains("1 looks like the label printer")
        );
        assert!(text.contains("prins printer save ID-PRINTER") && text.contains("-52 dBm, strong"));
        assert!(text.contains("1 more device has no name"), "{text}");
        assert!(text.contains("why:") && !text.contains("(no name)"));
        assert!(text.contains("Nothing was saved. Ask the user"));
    }

    #[test]
    fn the_saved_printer_is_marked_in_the_list() {
        let found = vec![dev("ID-1", Some("Q199E63C4390578"), Some(-60))];
        let saved = PrinterRef {
            id: "OLD-ID".into(),
            name: Some("Q199E63C4390578".into()),
        };
        assert!(scan_text(&found, Some(&saved), 6).contains("[saved]"));
        assert!(!scan_text(&found, None, 6).contains("[saved]"));
    }

    #[test]
    fn an_empty_scan_says_what_to_check() {
        let text = scan_text(&[], None, 6);
        for needle in [
            "No Bluetooth devices",
            "switched on",
            "phone app",
            "Privacy & Security",
        ] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
    }

    #[test]
    fn a_scan_without_a_printer_says_so() {
        let found = vec![dev("ID-KB", Some("Magic Keyboard"), Some(-40))];
        let text = scan_text(&found, None, 6);
        assert!(
            text.contains("None look like") && text.contains("No device looks like an M110"),
            "{text}"
        );
        assert!(!text.contains("Likely printers:"));
    }

    #[test]
    fn a_long_list_of_other_devices_is_cut() {
        let found: Vec<Found> = (0..40)
            .map(|i| dev(&format!("ID-{i}"), Some("Thing"), Some(-70)))
            .collect();
        let text = scan_text(&found, None, 6);
        assert!(text.contains("and 25 more"), "{text}");
    }

    #[test]
    fn scan_json_is_machine_readable() {
        let found = vec![dev("ID-1", Some("Q199E63C4390578"), Some(-52))];
        let v: serde_json::Value = serde_json::from_str(&scan_json(&found, None, 6)).unwrap();
        assert_eq!(v["devices"][0]["id"], "ID-1");
        assert_eq!(v["devices"][0]["likely_printer"], true);
        assert_eq!(v["devices"][0]["rssi"], -52);
        assert!(v["saved_printer"].is_null());
    }

    #[test]
    fn the_saved_message_says_what_and_where() {
        let p = PrinterRef {
            id: "ID-1".into(),
            name: Some("Q199E63C4390578".into()),
        };
        let text = saved_message(&p, Path::new("/home/x/.prins/config.json"), true);
        assert!(
            text.contains("ID-1")
                && text.contains("Q199E63C4390578")
                && text.contains("/home/x/.prins/config.json")
        );
        assert!(!text.contains("not checked"));
        let q = PrinterRef {
            id: "ID-2".into(),
            name: None,
        };
        assert!(saved_message(&q, Path::new("/c.json"), false).contains("not checked"));
    }

    #[test]
    fn show_says_where_the_printer_came_from() {
        let p = PrinterRef {
            id: "ID-1".into(),
            name: Some("Q1".into()),
        };
        let file = Path::new("/definitely/not/here/config.json");
        let text = show_text(Some((p.clone(), Source::File)), file, Some(&p));
        assert!(
            text.contains("config file") && text.contains("does not exist"),
            "{text}"
        );
        let env = PrinterRef {
            id: "Q1ZZZZZZ".into(),
            name: None,
        };
        let text = show_text(Some((env, Source::Env)), file, Some(&p));
        assert!(
            text.contains("PRINS_PRINTER") && text.contains("The config file has"),
            "{text}"
        );
        assert!(show_text(None, file, None).contains("No printer saved. Run `prins printer scan`"));
    }

    #[test]
    fn the_dry_run_says_nothing_was_sent() {
        let text = dry_run_text("test", &m110::test_pattern());
        assert!(
            text.contains("nothing was sent")
                && text.contains("236 rows")
                && text.contains("ONE label"),
            "{text}"
        );
    }

    #[test]
    fn a_missing_printer_gets_a_next_step() {
        let e = explain(anyhow::Error::new(M110Error::NotFound("Q1".into())));
        let text = e.to_string();
        assert!(
            text.contains("not found") && text.contains("prins printer scan"),
            "{text}"
        );
        assert_eq!(explain(anyhow!("other")).to_string(), "other");
    }
}
