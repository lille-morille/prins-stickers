//! Runs the real `prins` binary against a temporary `PRINS_HOME`. Nothing here touches Bluetooth
//! (printing is always `--dry-run`, and the printer tests only edit the config file) or the real
//! `~/.prins`.
//!
//! Assertions are about structure (exit codes, what is on stdout and stderr, files and sizes),
//! not about what a particular tool draws, so they hold for any design engine behind the tools.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn ok(self) -> String {
        assert_eq!(self.code, 0, "expected success, stderr: {}", self.stderr);
        assert!(self.stderr.is_empty(), "unexpected stderr: {}", self.stderr);
        self.stdout
    }

    /// The failure's one-line message, after checking the shape of a failure.
    fn err(self) -> String {
        assert_eq!(self.code, 1, "expected exit 1, stdout: {}", self.stdout);
        assert!(
            self.stderr.starts_with("prins: ") && self.stderr.trim_end().lines().count() == 1,
            "stderr should be one `prins: ...` line: {:?}",
            self.stderr
        );
        self.stderr
            .trim_end()
            .trim_start_matches("prins: ")
            .to_string()
    }
}

struct Env {
    root: PathBuf,
    home: PathBuf,
}

impl Env {
    fn new() -> Env {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "prins-cli-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("prins-home");
        fs::create_dir_all(&root).unwrap();
        Env { root, home }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_prins"));
        cmd.args(args)
            .env("PRINS_HOME", &self.home)
            .env("HOME", &self.root)
            .env("PRINS_PRINTER", "")
            .env_remove("PRINS_DRAWING")
            .env_remove("PRINS_NAME");
        cmd
    }

    fn run(&self, args: &[&str]) -> Out {
        self.finish(self.command(args), None)
    }

    fn run_with(&self, args: &[&str], envs: &[(&str, &str)]) -> Out {
        let mut cmd = self.command(args);
        cmd.envs(envs.iter().copied());
        self.finish(cmd, None)
    }

    fn run_stdin(&self, args: &[&str], stdin: &str) -> Out {
        self.finish(self.command(args), Some(stdin))
    }

    fn finish(&self, mut cmd: Command, stdin: Option<&str>) -> Out {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = cmd.spawn().unwrap();
        if let Some(text) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// `prins new`, returning the id.
    fn new_drawing(&self) -> String {
        self.run(&["new"]).ok().trim().to_string()
    }

    /// Draw one text op (works with any engine) and return the op count.
    fn add_text(&self, id: &str, s: &str) -> usize {
        ops_line(&self.run(&["draw", id, "text", &format!("s={s}")]).ok())
    }

    fn ops_in(&self, id: &str) -> Vec<serde_json::Value> {
        let text = self.run(&["show", id]).ok();
        serde_json::from_str(text.trim()).unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The number in the `ops: N` line a `draw` ends with.
fn ops_line(stdout: &str) -> usize {
    let last = stdout.trim_end().lines().last().unwrap_or("");
    last.strip_prefix("ops: ")
        .unwrap_or_else(|| panic!("no `ops: N` line in {stdout:?}"))
        .parse()
        .unwrap()
}

fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn read_png(path: &str) -> image::GrayImage {
    let path = Path::new(path.trim());
    assert!(path.is_absolute(), "{path:?} is not absolute");
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    image::load_from_memory(&bytes).unwrap().to_luma8()
}

// ---------------------------------------------------------------------------------------------

#[test]
fn new_prints_only_an_id_and_creates_the_file() {
    let env = Env::new();
    let id = env.run(&["new"]).ok();
    assert_eq!(id.matches('\n').count(), 1, "{id:?}");
    let id = id.trim();
    assert!(is_id(id), "{id}");
    assert!(env
        .home
        .join("drawings")
        .join(format!("{id}.json"))
        .is_file());
    assert_ne!(env.new_drawing(), id);
    assert!(env.ops_in(id).is_empty());
}

#[test]
fn new_with_a_name_uses_it_once() {
    let env = Env::new();
    assert_eq!(env.run(&["new", "--name", "door-sign"]).ok(), "door-sign\n");
    let err = env.run(&["new", "--name", "door-sign"]).err();
    assert!(err.contains("already exists"), "{err}");
    for bad in ["../x", "Door", "a b", ""] {
        env.run(&["new", "--name", bad]).err();
    }
}

#[test]
fn draw_takes_json_stdin_and_key_value_arguments() {
    let env = Env::new();
    let id = env.new_drawing();

    let n = ops_line(
        &env.run(&["draw", &id, "text", r#"{"s":"from json","size":30}"#])
            .ok(),
    );
    assert_eq!(n, 1);
    let n = ops_line(
        &env.run_stdin(&["draw", &id, "text", "-"], r#"{"s":"from stdin"}"#)
            .ok(),
    );
    assert_eq!(n, 2);
    let n = ops_line(
        &env.run(&["draw", &id, "text", "s=Hello world", "size=40"])
            .ok(),
    );
    assert_eq!(n, 3);

    let ops = env.ops_in(&id);
    assert_eq!(ops.len(), 3);
    assert_eq!(ops[0]["s"], "from json");
    assert_eq!(ops[0]["size"], 30);
    assert_eq!(ops[1]["s"], "from stdin");
    // key=value: valid JSON is read as JSON (a number), anything else is a string.
    assert_eq!(ops[2]["s"], "Hello world");
    assert_eq!(ops[2]["size"], 40);
}

#[test]
fn draw_accepts_names_with_and_without_the_draw_prefix() {
    let env = Env::new();
    let id = env.new_drawing();
    assert_eq!(
        ops_line(&env.run(&["draw", &id, "draw_text", "s=a"]).ok()),
        1
    );
    assert_eq!(ops_line(&env.run(&["draw", &id, "text", "s=b"]).ok()), 2);
}

#[test]
fn draw_uses_prins_drawing_when_the_id_is_left_out() {
    let env = Env::new();
    let id = env.new_drawing();
    let out = env.run_with(&["draw", "text", "s=hi"], &[("PRINS_DRAWING", &id)]);
    assert_eq!(ops_line(&out.ok()), 1);
    assert_eq!(env.ops_in(&id).len(), 1);
    let out = env.run_with(&["show"], &[("PRINS_DRAWING", &id)]).ok();
    assert!(out.starts_with('['), "{out}");
    // Without the variable, the id is required.
    let err = env.run(&["draw", "text", "s=hi"]).err();
    assert!(err.contains("PRINS_DRAWING"), "{err}");
}

#[test]
fn a_failed_draw_leaves_the_drawing_untouched() {
    let env = Env::new();
    let id = env.new_drawing();
    env.add_text(&id, "keep me");
    let file = env.home.join("drawings").join(format!("{id}.json"));
    let before = fs::read(&file).unwrap();
    // A figure that does not exist is refused, as in the Python engine.
    let err = env.run(&["draw", &id, "figure", "name=no-such-figure"]).err();
    assert!(!err.is_empty());
    assert_eq!(fs::read(&file).unwrap(), before);
    // An unknown tool is refused before the drawing is touched too.
    let err = env.run(&["draw", &id, "no_such_tool"]).err();
    assert!(err.contains("Unknown tool"), "{err}");
    assert_eq!(fs::read(&file).unwrap(), before);
    // Broken arguments.
    env.run(&["draw", &id, "text", "{not json"]).err();
    env.run(&["draw", &id, "text", "justaword"]).err();
    env.run_stdin(&["draw", &id, "text", "-"], "[1,2]").err();
    assert_eq!(fs::read(&file).unwrap(), before);
}

#[test]
fn undo_and_clear_change_the_drawing() {
    let env = Env::new();
    let id = env.new_drawing();
    env.add_text(&id, "one");
    env.add_text(&id, "two");
    assert_eq!(ops_line(&env.run(&["draw", &id, "undo"]).ok()), 1);
    assert_eq!(env.ops_in(&id)[0]["s"], "one");
    assert_eq!(ops_line(&env.run(&["draw", &id, "clear"]).ok()), 0);
    assert!(env.ops_in(&id).is_empty());
    // A blank drawing cannot be previewed or printed.
    let err = env.run(&["preview", &id]).err();
    assert!(err.contains("blank"), "{err}");
    let err = env.run(&["print", &id, "--dry-run"]).err();
    assert!(err.contains("blank"), "{err}");
}

#[test]
fn tools_lists_describes_and_gives_the_guide() {
    let env = Env::new();
    let list = env.run(&["tools"]).ok();
    for name in [
        "draw_poster",
        "draw_text",
        "undo",
        "clear",
        "preview",
        "compare_looks",
    ] {
        assert!(
            list.lines()
                .any(|l| l.split_whitespace().next() == Some(name)),
            "{name} missing from:\n{list}"
        );
    }
    assert!(!list.contains("print_note") && !list.contains("send_to_friend"));
    // One line per tool, no schemas.
    assert!(!list.contains("\"properties\""));

    let one = env.run(&["tools", "poster"]).ok();
    assert!(one.starts_with("draw_poster\n"), "{one}");
    assert!(one.contains("\"type\""), "the schema is shown: {one}");

    let json: serde_json::Value =
        serde_json::from_str(&env.run(&["tools", "text", "--json"]).ok()).unwrap();
    assert_eq!(json["name"], "draw_text");
    assert!(json["description"].is_string() && json["schema"].is_object());

    let all: serde_json::Value = serde_json::from_str(&env.run(&["tools", "--json"]).ok()).unwrap();
    assert!(all
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "draw_poster"));

    assert!(!env.run(&["tools", "--guide"]).ok().trim().is_empty());
    let err = env.run(&["tools", "no_such_tool"]).err();
    assert!(err.contains("Unknown tool"), "{err}");
}

#[test]
fn catalog_prints_json() {
    let env = Env::new();
    let v: serde_json::Value = serde_json::from_str(&env.run(&["catalog"]).ok()).unwrap();
    assert!(v.is_object());
    env.run(&["catalog", "definitely-not-a-kind"]).err();
}

#[test]
fn preview_prints_only_the_path_of_a_sharp_png() {
    let env = Env::new();
    let id = env.new_drawing();
    env.add_text(&id, "hello");
    let out = env.run(&["preview", &id]).ok();
    assert_eq!(out.matches('\n').count(), 1, "only the path: {out:?}");
    assert!(out.trim().ends_with(&format!("{id}.png")), "{out}");
    let img = read_png(&out);
    assert_eq!(img.dimensions(), (624, 432), "default scale is 2");
    assert!(
        img.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255),
        "nearest-neighbour keeps it 1 bit"
    );
    assert!(img.pixels().any(|p| p.0[0] == 0), "something is drawn");

    let target = env.path("out").join("mine.png");
    let out = env
        .run(&[
            "preview",
            &id,
            "-o",
            target.to_str().unwrap(),
            "--scale",
            "1",
        ])
        .ok();
    assert_eq!(Path::new(out.trim()), target);
    assert_eq!(read_png(&out).dimensions(), (312, 216));
    env.run(&["preview", &id, "--scale", "0"]).err();
    let _ = fs::remove_file(std::env::temp_dir().join("prins").join(format!("{id}.png")));
}

#[test]
fn print_dry_run_writes_the_one_bit_image_and_sends_nothing() {
    let env = Env::new();
    let id = env.new_drawing();
    env.add_text(&id, "hello");
    let out = env.run(&["print", &id, "--dry-run"]).ok();
    assert_eq!(out.matches('\n').count(), 1, "only the path: {out:?}");
    assert!(out.trim().ends_with(&format!("{id}-print.png")), "{out}");
    let img = read_png(&out);
    assert!(img.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255));
    assert!(img.pixels().any(|p| p.0[0] == 0));
    assert!(img.width() >= 312);
    // Nothing was printed, so the counter is unchanged.
    let list: serde_json::Value =
        serde_json::from_str(&env.run(&["drawings", "--json"]).ok()).unwrap();
    assert_eq!(list[0]["printed"], 0);
    let _ = fs::remove_file(
        std::env::temp_dir()
            .join("prins")
            .join(format!("{id}-print.png")),
    );
}

#[test]
fn print_without_a_saved_printer_says_how_to_save_one() {
    let env = Env::new();
    let id = env.new_drawing();
    env.add_text(&id, "hello");
    let err = env.run(&["print", &id]).err();
    assert!(
        err.contains("No printer saved") && err.contains("prins printer scan"),
        "{err}"
    );
    let list: serde_json::Value =
        serde_json::from_str(&env.run(&["drawings", "--json"]).ok()).unwrap();
    assert_eq!(list[0]["printed"], 0);
    env.run(&["print", &id, "--copies", "0", "--dry-run"]).err();
    let help = env.run(&["print", "--help"]).ok();
    assert!(
        help.contains("one label"),
        "help warns about labels: {help}"
    );
}

#[test]
fn drawings_lists_newest_first_and_prunes_old_ones() {
    let env = Env::new();
    assert!(env.run(&["drawings"]).ok().contains("No drawings"));
    let old = env.new_drawing();
    let fresh = env.new_drawing();
    env.add_text(&fresh, "x");
    env.add_text(&fresh, "y");

    // Make `old` look a year old by editing the file the way the store writes it.
    let file = env.home.join("drawings").join(format!("{old}.json"));
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    v["updated"] = serde_json::json!(1_000_000);
    fs::write(&file, serde_json::to_string(&v).unwrap()).unwrap();

    let text = env.run(&["drawings"]).ok();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].starts_with(&fresh) && lines[0].contains("2 ops"),
        "{text}"
    );
    assert!(lines[1].starts_with(&old), "{text}");

    let json: serde_json::Value =
        serde_json::from_str(&env.run(&["drawings", "--json"]).ok()).unwrap();
    assert_eq!(json[0]["id"], fresh.as_str());
    assert_eq!(json[0]["ops"], 2);
    assert_eq!(json[0]["printed"], 0);
    assert!(json[0]["updated"].as_u64().unwrap() > 1_000_000_000);

    let out = env.run(&["drawings", "--prune", "30"]).ok();
    assert!(
        out.contains(&format!("removed {old}")) && !out.contains(&fresh),
        "{out}"
    );
    assert!(!file.exists());
    env.run(&["show", &fresh]).ok();
    assert!(env
        .run(&["drawings", "--prune", "30"])
        .ok()
        .contains("Nothing to prune"));
}

#[test]
fn unknown_and_invalid_ids_are_one_line_errors() {
    let env = Env::new();
    let a = env.new_drawing();
    let err = env.run(&["show", "no-such-one"]).err();
    assert!(
        err.contains("no-such-one") && err.contains(&a),
        "lists recent drawings: {err}"
    );
    env.add_text(&a, "x");
    let err = env.run(&["draw", "no-such-one", "text", "s=x"]).err();
    assert!(err.contains("no-such-one"), "{err}");
    env.run(&["preview", "no-such-one"]).err();
    env.run(&["print", "no-such-one", "--dry-run"]).err();

    for bad in ["../escape", "UPPER", "a/b", "dot.json", &"a".repeat(41)] {
        let err = env.run(&["show", bad]).err();
        assert!(err.contains("not a drawing id"), "{bad}: {err}");
        env.run(&["draw", bad, "text", "s=x"]).err();
    }
    assert!(!env.root.join("escape.json").exists());
}

#[test]
fn usage_mistakes_exit_one_and_help_exits_zero() {
    let env = Env::new();
    assert_eq!(env.run(&["bogus"]).code, 1);
    assert_eq!(env.run(&[]).code, 1);
    assert_eq!(env.run(&["draw"]).code, 1);
    let help = env.run(&["--help"]);
    assert_eq!(help.code, 0);
    for command in [
        "new", "draw", "tools", "preview", "print", "drawings", "doctor", "skill", "printer",
    ] {
        assert!(
            help.stdout.contains(command),
            "{command} missing from --help"
        );
    }
    assert_eq!(env.run(&["--version"]).code, 0);
}

#[test]
fn two_processes_drawing_at_once_lose_nothing() {
    let env = Env::new();
    let id = env.new_drawing();
    let handles: Vec<_> = (0..6)
        .map(|n| {
            let mut cmd = env.command(&["draw", &id, "text", &format!("s=job {n}")]);
            std::thread::spawn(move || {
                let out = cmd.output().unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(env.ops_in(&id).len(), 6);
    let leftovers: Vec<String> = fs::read_dir(env.home.join("drawings"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_tool_that_returns_an_image_prints_its_path() {
    let env = Env::new();
    let id = env.new_drawing();
    env.add_text(&id, "hello");
    let out = env.run(&["draw", &id, "preview"]).ok();
    let png_line = out
        .lines()
        .find(|l| l.ends_with(".png"))
        .unwrap_or_else(|| panic!("no PNG path in {out:?}"));
    let img = read_png(png_line);
    assert!(img.width() >= 312);
    assert_eq!(ops_line(&out), 1);
    let _ = fs::remove_file(png_line);
}

#[test]
fn printer_show_save_no_verify_and_forget_use_the_temp_home() {
    let env = Env::new();
    fs::create_dir_all(&env.home).unwrap();
    let config = env.home.join("config.json");
    // Other keys in the file, such as the sender's name, must survive.
    fs::write(&config, r#"{"name": "Anders", "theme": {"dark": true}}"#).unwrap();

    let out = env.run(&["printer", "show"]).ok();
    assert!(out.contains("No printer saved"), "{out}");

    let out = env
        .run(&["printer", "save", "TEST-PRINTER-ID", "--no-verify"])
        .ok();
    assert!(out.contains("TEST-PRINTER-ID"), "{out}");
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["printer"]["id"], "TEST-PRINTER-ID");
    assert_eq!(saved["name"], "Anders");
    assert_eq!(saved["theme"]["dark"], true);

    let out = env.run(&["printer", "show"]).ok();
    assert!(
        out.contains("TEST-PRINTER-ID") && out.contains(env.home.to_str().unwrap()),
        "{out}"
    );
    // PRINS_PRINTER wins for one run and says so.
    let out = env
        .run_with(&["printer", "show"], &[("PRINS_PRINTER", "OTHER-PRINTER")])
        .ok();
    assert!(
        out.contains("OTHER-PRINTER") && out.contains("PRINS_PRINTER"),
        "{out}"
    );

    let out = env.run(&["printer", "forget"]).ok();
    assert!(out.contains("TEST-PRINTER-ID"), "{out}");
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert!(after.get("printer").is_none());
    assert_eq!(after["name"], "Anders");
    assert!(env
        .run(&["printer", "show"])
        .ok()
        .contains("No printer saved"));
    // `save` needs an explicit target.
    env.run(&["printer", "save"]).err();
    // A dry-run test label never needs a printer.
    let out = env.run(&["printer", "test", "--dry-run"]).ok();
    assert!(out.contains("Dry run"), "{out}");
}

#[test]
fn skill_prints_and_installs() {
    let env = Env::new();
    let text = env.run(&["skill"]).ok();
    assert!(
        text.starts_with("---\nname: prins-stickers\n"),
        "{}",
        &text[..text.len().min(80)]
    );
    assert!(text.contains("description:"));
    for needle in [
        "prins preview",
        "prins print",
        "prins printer scan",
        "sticky note",
    ] {
        assert!(text.contains(needle), "skill lacks {needle:?}");
    }
    assert!(
        text.lines().count() <= 125,
        "keep the skill short: {} lines",
        text.lines().count()
    );

    let dir = env.path("skills");
    let out = env
        .run(&["skill", "--install", "--dir", dir.to_str().unwrap()])
        .ok();
    let file = dir.join("prins-stickers").join("SKILL.md");
    assert_eq!(Path::new(out.trim()), file);
    assert_eq!(fs::read_to_string(&file).unwrap(), text);
    // Installing again overwrites without complaint.
    env.run(&["skill", "--install", "--dir", dir.to_str().unwrap()])
        .ok();

    // The default folder is ~/.claude/skills (HOME is the temp folder here).
    let out = env.run(&["skill", "--install"]).ok();
    let default = env
        .root
        .join(".claude")
        .join("skills")
        .join("prins-stickers")
        .join("SKILL.md");
    assert_eq!(
        fs::canonicalize(out.trim()).unwrap(),
        fs::canonicalize(&default).unwrap()
    );
    env.run(&["skill", "--dir", dir.to_str().unwrap()]).err();
}

#[test]
fn doctor_reports_the_home_folder_and_printer() {
    let env = Env::new();
    let out = env.run(&["doctor"]).ok();
    assert!(out.contains(env.home.to_str().unwrap()), "{out}");
    for word in ["printer", "fonts", "emoji", "cache", "PRINS_NAME"] {
        assert!(out.contains(word), "doctor lacks {word:?}:\n{out}");
    }
    let out = env.run_with(&["doctor"], &[("PRINS_NAME", "Ada")]).ok();
    assert!(out.contains("Ada"), "{out}");
}
