//! The `prins` command line: argument types and what each command does.
//!
//! Output is plain on purpose, because agents parse it: what a command prints is what its help
//! says it prints, with no decoration. Errors are `anyhow` errors; `main` prints them as one line
//! `prins: <message>` on stderr and exits 1.

pub mod printer;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use image::GrayImage;
use serde_json::{json, Value};

use crate::config::{self, Source};
use crate::store::{self, Store};
use crate::tools::{self, Session};
use crate::{assets, canvas, fonts, m110};

/// The agent skill, embedded so `prins skill` works from a bare binary.
pub const SKILL: &str = include_str!("../../skills/prins-stickers/SKILL.md");

const NO_PRINTER: &str =
    "No printer saved. Run `prins printer scan`, then `prins printer save <id>`.";

const ABOUT: &str = "Make stickers and labels, and print them on an M110 over Bluetooth.";

const LONG_ABOUT: &str = "\
Make stickers and labels, and print them on an M110 over Bluetooth.

You work on a numbered drawing. A drawing is a list of design ops kept in ~/.prins/drawings,
on a 312x216 pixel, 1 bit grid (a 40 mm label at 203 dpi).

  prins new                                     create a drawing, prints its id (warm-fox)
  prins draw warm-fox poster say=\"Back at 3\"    design it with one tool call
  prins preview warm-fox                        render a PNG, prints its path: look at it
  prins print warm-fox                          print it (uses one label)

First time: `prins printer scan`, then `prins printer save <id>`.
Read the agent guide with `prins skill`. Tools are listed by `prins tools`.";

#[derive(Parser, Debug)]
#[command(name = "prins", version, about = ABOUT, long_about = LONG_ABOUT)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create an empty drawing and print its id.
    ///
    /// The id is two words such as warm-fox. Use it with `draw`, `preview` and `print`, or put
    /// it in PRINS_DRAWING so it can be left out.
    New {
        /// Use this id instead of a generated one (a-z, 0-9 and -, at most 40 characters).
        #[arg(long)]
        name: Option<String>,
    },
    /// Run one design tool on a drawing.
    ///
    /// Usage: prins draw [ID] <TOOL> [JSON | - | key=value ...]
    ///
    /// ID may be left out when PRINS_DRAWING is set. Tool names work with or without the
    /// `draw_` prefix (`text` is `draw_text`, `poster` is `draw_poster`). Arguments are a JSON
    /// object, or `-` to read the JSON object from stdin, or key=value pairs. A value is read as
    /// JSON when it is valid JSON (`size=40`, `data=[1,2,3]`, `fill=false`), otherwise it is a
    /// string (`say="Back at 3"`). To force a string that looks like a number, quote it as JSON:
    /// `big='"15"'`.
    ///
    /// Prints the tool's text result (for a poster: the look and any warnings) and then
    /// `ops: <n>`. Tools that return an image (`preview`, `compare_looks`) write a PNG to the
    /// temp folder and print its path. A tool error goes to stderr, exits 1, and leaves the
    /// drawing unchanged.
    ///
    /// Examples:
    ///   prins draw warm-fox poster say="Back at 3" sub="Lunch" pic=🍔
    ///   prins draw warm-fox text s="Hello world" size=40
    ///   prins draw warm-fox shape '{"kind":"rrect","x":4,"y":4,"w":304,"h":208,"fill":false}'
    ///   echo '{"say":"Fragile"}' | prins draw warm-fox poster -
    ///   prins draw warm-fox undo
    ///   prins draw warm-fox compare_looks say="Back at 3"
    Draw {
        /// [ID] <TOOL> [JSON | - | key=value ...]
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ID TOOL ARGS"
        )]
        args: Vec<String>,
    },
    /// List the design tools, or show one tool's description and JSON schema.
    ///
    /// With no name: one line per tool. With a name: its full description and schema, so an
    /// agent only pays for what it reads. `--guide` prints the short guide to the tools.
    Tools {
        /// A tool, with or without `draw_` (for example `poster`).
        name: Option<String>,
        /// Print JSON: the list (name and description), or {name, description, schema}.
        #[arg(long)]
        json: bool,
        /// Print the guide to the tools instead of the list.
        #[arg(long, conflicts_with = "name")]
        guide: bool,
    },
    /// Print the names of figures, patterns, frames, fonts, looks, dithers, ops, system_fonts.
    ///
    /// The same JSON as the `catalog` tool. Give a KIND for one list, or none for most of them.
    Catalog {
        /// looks, figures, patterns, frames, fonts, dithers, ops or system_fonts.
        kind: Option<String>,
    },
    /// Render a drawing to a PNG and print only its absolute path.
    ///
    /// The image is scaled with nearest-neighbour so pixels stay sharp. Open it and show it to
    /// the user before printing. A blank drawing is an error.
    Preview {
        /// The drawing (default: PRINS_DRAWING).
        id: Option<String>,
        /// Write the PNG here (default: <temp>/prins/<id>.png).
        #[arg(short = 'o', long, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Pixel scale, 1 to 16.
        #[arg(long, default_value_t = 2)]
        scale: u32,
    },
    /// Print a drawing on the saved printer. EVERY copy uses one label.
    ///
    /// Needs a printer from `prins printer save`. Refuses a blank drawing. Thermal prints fade
    /// with time. With --dry-run nothing is sent: the exact 1-bit image that would go to the
    /// printer is written to <temp>/prins/<id>-print.png and its path is printed.
    Print {
        /// The drawing (default: PRINS_DRAWING).
        id: Option<String>,
        /// How many copies. Each one uses a label.
        #[arg(long, default_value_t = 1)]
        copies: u32,
        /// Do not touch the printer: write the image that would be sent and print its path.
        #[arg(long)]
        dry_run: bool,
    },
    /// List drawings, newest first: id, ops, last update, how often printed.
    Drawings {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Delete drawings not updated for DAYS days, and print what was removed.
        #[arg(long, value_name = "DAYS")]
        prune: Option<u64>,
    },
    /// Print a drawing's ops as JSON.
    Show {
        /// The drawing (default: PRINS_DRAWING).
        id: Option<String>,
    },
    /// Show where things are: home folder, printer, fonts, emoji, cache. No Bluetooth.
    Doctor,
    /// Print the agent skill (SKILL.md), or install it with --install.
    ///
    /// --install writes <DIR>/prins-stickers/SKILL.md, by default
    /// ~/.claude/skills/prins-stickers/SKILL.md. For another agent use --dir, for example
    /// `--dir ~/.codex/skills`.
    Skill {
        /// Write the skill to disk instead of printing it.
        #[arg(long)]
        install: bool,
        /// The skills folder to install into (default ~/.claude/skills).
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Find, save and test the label printer.
    Printer {
        #[command(subcommand)]
        cmd: printer::PrinterCmd,
    },
}

/// Run the parsed command line.
pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::New { name } => new(name.as_deref()),
        Command::Draw { args } => draw(&args),
        Command::Tools { name, json, guide } => tools_cmd(name.as_deref(), json, guide),
        Command::Catalog { kind } => catalog_cmd(kind.as_deref()),
        Command::Preview { id, output, scale } => preview(id, output, scale),
        Command::Print {
            id,
            copies,
            dry_run,
        } => print(id, copies, dry_run),
        Command::Drawings { json, prune } => drawings(json, prune),
        Command::Show { id } => show(id),
        Command::Doctor => doctor(),
        Command::Skill { install, dir } => skill(install, dir),
        Command::Printer { cmd } => printer::run(cmd),
    }
}

// ---------------------------------------------------------------------------------------------
// helpers

/// The drawing id: the argument, else `PRINS_DRAWING`.
fn resolve_id(arg: Option<String>) -> Result<String> {
    if let Some(id) = arg {
        return Ok(id);
    }
    match std::env::var(store::DRAWING_ENV) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => bail!(
            "Which drawing? Pass its id, or set {}. `prins drawings` lists them, `prins new` makes one.",
            store::DRAWING_ENV
        ),
    }
}

/// `<temp>/prins`, where previews and dry runs go.
fn temp_folder() -> PathBuf {
    std::env::temp_dir().join("prins")
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn write_png_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("Cannot write {}", path.display()))
}

fn write_png(path: &Path, image: &GrayImage) -> Result<()> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir.display()))?;
    }
    image
        .save_with_format(path, image::ImageFormat::Png)
        .with_context(|| format!("Cannot write {}", path.display()))
}

fn scaled(image: &GrayImage, scale: u32) -> GrayImage {
    if scale <= 1 {
        return image.clone();
    }
    image::imageops::resize(
        image,
        image.width() * scale,
        image.height() * scale,
        image::imageops::FilterType::Nearest,
    )
}

// ---------------------------------------------------------------------------------------------
// new, drawings, show

fn new(name: Option<&str>) -> Result<()> {
    let id = Store::from_env().create(name)?;
    println!("{id}");
    Ok(())
}

fn age(updated: u64, now: u64) -> String {
    let secs = now.saturating_sub(updated);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn drawings(json: bool, prune: Option<u64>) -> Result<()> {
    let store = Store::from_env();
    if let Some(days) = prune {
        let removed = store.prune(days)?;
        if removed.is_empty() {
            println!("Nothing to prune: no drawing is older than {days} days.");
        }
        for id in removed {
            println!("removed {id}");
        }
        return Ok(());
    }
    let list = store.list()?;
    if json {
        println!("{}", serde_json::to_string(&list)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("No drawings yet. Run `prins new` to start one.");
    }
    let now = store::now();
    for s in list {
        println!(
            "{:<24} {:>4} ops  updated {:<9}  printed {}",
            s.id,
            s.ops,
            age(s.updated, now),
            s.printed
        );
    }
    Ok(())
}

fn show(id: Option<String>) -> Result<()> {
    let drawing = Store::from_env().read(&resolve_id(id)?)?;
    println!("{}", serde_json::to_string(&drawing.ops)?);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// draw

/// The tool a name stands for: exact, or with `draw_` in front. Dashes count as underscores.
fn resolve_tool(name: &str) -> Option<&'static str> {
    let name = name.replace('-', "_");
    let all = tools::tools();
    if let Some(t) = all.iter().find(|t| t.name == name) {
        return Some(t.name);
    }
    let prefixed = format!("draw_{name}");
    all.iter().find(|t| t.name == prefixed).map(|t| t.name)
}

fn looks_like_arguments(arg: &str) -> bool {
    arg == "-" || arg.starts_with('{') || arg.contains('=')
}

/// Split `[ID] <TOOL> [ARGS...]`. The id is optional when `PRINS_DRAWING` is set.
fn split_draw_args(args: &[String]) -> Result<(String, &'static str, &[String])> {
    if args.len() >= 2 {
        if let Some(tool) = resolve_tool(&args[1]) {
            return Ok((args[0].clone(), tool, &args[2..]));
        }
    }
    if let Some(tool) = resolve_tool(&args[0]) {
        let id = resolve_id(None).map_err(|_| {
            anyhow!(
                "Which drawing? Use `prins draw <id> {} ...`, or set {}.",
                args[0],
                store::DRAWING_ENV
            )
        })?;
        return Ok((id, tool, &args[1..]));
    }
    // Neither is a tool. Report the word that was most likely meant as one.
    let guess = if args.len() >= 2 && !looks_like_arguments(&args[1]) {
        &args[1]
    } else {
        &args[0]
    };
    if args.len() == 1 && store::valid_id(guess) {
        bail!("Which tool? `prins draw {guess} <tool> ...`. `prins tools` lists them.");
    }
    bail!("Unknown tool {guess:?}. `prins tools` lists them.")
}

/// A key=value argument. The value is JSON when it parses, else a string.
fn parse_pair(arg: &str) -> Result<(String, Value)> {
    let Some((key, value)) = arg.split_once('=') else {
        bail!("Expected key=value, a JSON object, or `-` for stdin, but got {arg:?}.");
    };
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        bail!("Expected key=value, but {arg:?} has no usable key.");
    }
    let value =
        serde_json::from_str::<Value>(value).unwrap_or_else(|_| Value::String(value.into()));
    Ok((key.to_string(), value))
}

fn parse_object(text: &str, from: &str) -> Result<Value> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => bail!("The arguments from {from} must be a JSON object like {{\"say\":\"Hi\"}}."),
        Err(e) => bail!("The arguments from {from} are not valid JSON: {e}."),
    }
}

/// The tool's arguments as one JSON object.
fn parse_call(rest: &[String]) -> Result<Value> {
    match rest {
        [] => Ok(json!({})),
        [one] if one == "-" => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("Cannot read stdin")?;
            if text.trim().is_empty() {
                bail!("Nothing came in on stdin. Pipe a JSON object, or use key=value.");
            }
            parse_object(&text, "stdin")
        }
        [one] if one.trim_start().starts_with('{') => parse_object(one, "the command line"),
        _ => {
            if rest.iter().any(|a| a == "-") {
                bail!("`-` reads all arguments from stdin, so it cannot be mixed with others.");
            }
            let mut map = serde_json::Map::new();
            for arg in rest {
                let (key, value) = parse_pair(arg)?;
                map.insert(key, value);
            }
            Ok(Value::Object(map))
        }
    }
}

fn draw(args: &[String]) -> Result<()> {
    let store = Store::from_env();
    let (id, tool, rest) = split_draw_args(args)?;
    if !store.exists(&id) {
        // Same error as a read: lists the recent drawings, rejects odd ids.
        store.read(&id)?;
    }
    let call = parse_call(rest)?;
    let (result, ops) = store.update(&id, |drawing| {
        let canvas = store.canvas(drawing)?;
        let mut session = Session {
            canvas,
            designs_dir: Some(store.designs_dir()),
        };
        let result = tools::run_tool(&mut session, tool, &call).map_err(|e| anyhow!("{e}"))?;
        drawing.ops = session.canvas.ops.clone();
        Ok((result, drawing.ops.len()))
    })?;
    if !result.text.is_empty() {
        println!("{}", result.text);
    }
    if let Some(png) = &result.png {
        let path = absolute(&temp_folder().join(format!("{id}-{tool}.png")));
        write_png_bytes(&path, png)?;
        println!("{}", path.display());
    }
    println!("ops: {ops}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// tools, catalog

/// The first sentence of a description, cut to fit on a line.
fn one_line(description: &str) -> String {
    let first = description
        .split_once(". ")
        .map(|(a, _)| a)
        .unwrap_or(description)
        .trim_end_matches('.');
    let first = first.lines().next().unwrap_or("").trim();
    if first.chars().count() > 110 {
        let cut: String = first.chars().take(107).collect();
        format!("{cut}...")
    } else {
        first.to_string()
    }
}

fn tools_cmd(name: Option<&str>, json: bool, guide: bool) -> Result<()> {
    if guide {
        println!("{}", tools::GUIDE);
        return Ok(());
    }
    let all = tools::tools();
    match name {
        None if json => {
            let list: Vec<Value> = all
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description}))
                .collect();
            println!("{}", serde_json::to_string(&list)?);
        }
        None => {
            let width = all.iter().map(|t| t.name.len()).max().unwrap_or(0);
            for t in &all {
                println!("{:<width$}  {}", t.name, one_line(&t.description));
            }
        }
        Some(name) => {
            let resolved = resolve_tool(name)
                .ok_or_else(|| anyhow!("Unknown tool {name:?}. `prins tools` lists them."))?;
            let t = all
                .iter()
                .find(|t| t.name == resolved)
                .ok_or_else(|| anyhow!("Unknown tool {name:?}."))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(
                        &json!({"name": t.name, "description": t.description, "schema": t.schema})
                    )?
                );
            } else {
                println!("{}", t.name);
                println!("{}", t.description);
                println!();
                println!("{}", serde_json::to_string_pretty(&t.schema)?);
            }
        }
    }
    Ok(())
}

fn catalog_cmd(kind: Option<&str>) -> Result<()> {
    let value = canvas::catalog(kind).map_err(|e| anyhow!("{e}"))?;
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// preview, print

/// Load a drawing as a canvas, refusing a blank one.
fn load_nonblank(store: &Store, id: &str, then: &str) -> Result<canvas::Canvas> {
    let drawing = store.read(id)?;
    let canvas = store.canvas(&drawing)?;
    if canvas.blank() {
        bail!("Drawing {id} is blank, so there is nothing to {then}. Draw something first: `prins draw {id} poster say=\"...\"`.");
    }
    Ok(canvas)
}

fn preview(id: Option<String>, output: Option<PathBuf>, scale: u32) -> Result<()> {
    if !(1..=16).contains(&scale) {
        bail!("--scale is 1 to 16, not {scale}.");
    }
    let store = Store::from_env();
    let id = resolve_id(id)?;
    let canvas = load_nonblank(&store, &id, "preview")?;
    let path = absolute(&output.unwrap_or_else(|| temp_folder().join(format!("{id}.png"))));
    write_png(&path, &scaled(&canvas.image, scale))?;
    println!("{}", path.display());
    Ok(())
}

fn print(id: Option<String>, copies: u32, dry_run: bool) -> Result<()> {
    if !(1..=50).contains(&copies) {
        bail!("--copies is 1 to 50, not {copies}. Every copy uses one label.");
    }
    let store = Store::from_env();
    let id = resolve_id(id)?;
    let canvas = load_nonblank(&store, &id, "print")?;
    if dry_run {
        let dry = m110::dry_run(&canvas.image);
        let path = absolute(&temp_folder().join(format!("{id}-print.png")));
        write_png(&path, &dry.prepared)?;
        println!("{}", path.display());
        return Ok(());
    }
    let printer = config::printer().ok_or_else(|| anyhow!(NO_PRINTER))?;
    for copy in 1..=copies {
        let report = m110::print(&canvas.image, &printer, m110::PrintOptions::default())
            .map_err(explain_print_error)?;
        store.update(&id, |d| {
            d.printed += 1;
            Ok(())
        })?;
        println!(
            "Sent {id} to {} (copy {copy} of {copies}, {} bytes). That is only the data: look at the printer to see the label.",
            report.printer_name.as_deref().unwrap_or(&report.printer_id),
            report.bytes,
        );
    }
    Ok(())
}

fn explain_print_error(err: anyhow::Error) -> anyhow::Error {
    if let Some(m110::M110Error::NotFound(name)) = err.downcast_ref::<m110::M110Error>() {
        return anyhow!(
            "Printer {name:?} was not found. Check that it is on and not connected to the phone app, and that it is near. \
             `prins printer scan` shows what is in range."
        );
    }
    err
}

// ---------------------------------------------------------------------------------------------
// doctor

fn doctor() -> Result<()> {
    let store = Store::from_env();
    let home = store.home();
    let state = if home.is_dir() {
        "exists"
    } else {
        "does not exist yet (created on first use)"
    };
    println!("home:      {} ({state})", home.display());
    match std::env::var_os(config::HOME_ENV) {
        Some(v) if !v.is_empty() => println!("           set by {}", config::HOME_ENV),
        _ => {}
    }
    let drawings = store.list().map(|l| l.len()).unwrap_or(0);
    println!(
        "drawings:  {drawings} in {}",
        store.drawings_dir().display()
    );
    match config::printer_with_source() {
        Some((p, Source::File)) => println!(
            "printer:   {} (saved in {})",
            p.describe(),
            config::path().display()
        ),
        Some((p, Source::Env)) => println!(
            "printer:   {} (from {}, this shell only)",
            p.describe(),
            config::PRINTER_ENV
        ),
        None => println!(
            "printer:   none saved. Run `prins printer scan`, then `prins printer save <id>`."
        ),
    }
    let name = std::env::var(store::NAME_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    match (name, store::config_name(home)) {
        (Some(n), _) => println!("sender:    {n} (from {})", store::NAME_ENV),
        (None, Some(n)) => println!("sender:    {n} (from the name in config.json)"),
        (None, None) => println!(
            "sender:    not set. Posters get no \"from\" line. Set {} or add \"name\" to config.json.",
            store::NAME_ENV
        ),
    }
    match std::env::var(store::DRAWING_ENV) {
        Ok(v) if !v.trim().is_empty() => {
            println!("drawing:   {} (from {})", v.trim(), store::DRAWING_ENV)
        }
        _ => println!("drawing:   {} not set", store::DRAWING_ENV),
    }
    let fonts = fonts::all_system_fonts();
    if fonts.is_empty() {
        println!("fonts:     no system fonts found. Text falls back to the built-in pixel font.");
    } else {
        println!("fonts:     {} system fonts found", fonts.len());
    }
    let emoji = if assets::emoji_rgba("\u{1F642}", 32).is_some() {
        "present"
    } else {
        "missing (emoji: figures will not draw; use a named figure or an image)"
    };
    println!("emoji:     {emoji}");
    let cache = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".prins")
        .join("cache");
    let files = std::fs::read_dir(&cache).map(|d| d.count()).ok();
    match files {
        Some(n) => println!("cache:     {} ({n} files)", cache.display()),
        None => println!("cache:     {} (not created yet)", cache.display()),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// skill

fn skill(install: bool, dir: Option<PathBuf>) -> Result<()> {
    if !install {
        if dir.is_some() {
            bail!("--dir only goes with --install.");
        }
        print!("{SKILL}");
        return Ok(());
    }
    let skills = match dir {
        Some(d) => d,
        None => dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot find your home folder. Pass --dir."))?
            .join(".claude")
            .join("skills"),
    };
    let folder = skills.join("prins-stickers");
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("Cannot create {}", folder.display()))?;
    let file = folder.join("SKILL.md");
    std::fs::write(&file, SKILL).with_context(|| format!("Cannot write {}", file.display()))?;
    println!("{}", absolute(&file).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn key_value_pairs_parse_as_json_or_string() {
        let v = parse_call(&args(&[
            "size=40",
            "s=Hello world",
            "data=[1,2]",
            "fill=false",
            "big=\"15\"",
        ]))
        .unwrap();
        assert_eq!(v["size"], 40);
        assert_eq!(v["s"], "Hello world");
        assert_eq!(v["data"], json!([1, 2]));
        assert_eq!(v["fill"], false);
        assert_eq!(v["big"], "15");
        assert_eq!(parse_call(&args(&["s=a=b"])).unwrap()["s"], "a=b");
        assert!(parse_call(&args(&["nokey"])).is_err());
        assert!(parse_call(&args(&["=x"])).is_err());
    }

    #[test]
    fn a_json_object_is_one_argument() {
        let v = parse_call(&args(&[r#"{"s":"hi","size":3}"#])).unwrap();
        assert_eq!(v["size"], 3);
        assert!(parse_call(&args(&["{broken"])).is_err());
        assert_eq!(parse_call(&[]).unwrap(), json!({}));
        assert!(parse_call(&args(&["-", "s=x"])).is_err());
    }

    #[test]
    fn tool_names_work_with_and_without_the_prefix() {
        assert_eq!(resolve_tool("draw_text"), Some("draw_text"));
        assert_eq!(resolve_tool("text"), Some("draw_text"));
        assert_eq!(resolve_tool("poster"), Some("draw_poster"));
        assert_eq!(resolve_tool("undo"), Some("undo"));
        assert_eq!(resolve_tool("nope"), None);
    }

    #[test]
    fn draw_arguments_split_into_id_tool_and_rest() {
        let a = args(&["warm-fox", "text", "s=hi"]);
        let (id, tool, rest) = split_draw_args(&a).unwrap();
        assert_eq!(
            (id.as_str(), tool, rest.len()),
            ("warm-fox", "draw_text", 1)
        );
        let a = args(&["warm-fox", "bogus"]);
        let err = split_draw_args(&a).unwrap_err().to_string();
        assert!(err.contains("Unknown tool \"bogus\""), "{err}");
        let a = args(&["warm-fox"]);
        assert!(split_draw_args(&a)
            .unwrap_err()
            .to_string()
            .contains("Which tool"));
    }

    #[test]
    fn descriptions_shrink_to_one_line() {
        assert_eq!(one_line("Text. size 0 fits the box."), "Text");
        assert!(one_line(&"word ".repeat(60)).chars().count() <= 110);
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(age(100, 130), "just now");
        assert_eq!(age(0, 300), "5m ago");
        assert_eq!(age(0, 7200), "2h ago");
        assert_eq!(age(0, 3 * 86400), "3d ago");
    }

    #[test]
    fn the_embedded_skill_is_the_prins_skill() {
        assert!(SKILL.starts_with("---\nname: prins-stickers\n"));
    }
}
