# prins-stickers

Design stickers, labels and sticky notes from the command line and print them on an M110 thermal label printer over Bluetooth. One Rust binary, `prins`, made for AI agents (Claude Code, Codex, Cursor, anything that can run a shell) and fine by hand. No server, no phone app.

A sticker is exactly 312 x 216 pixels, 1 bit: a 40 mm label at 203 dpi.

## One-shot setup with your agent

Paste this into your coding agent (Claude Code, Codex, Cursor, Gemini CLI, ...):

```text
Install the prins sticker printer CLI and its agent skill for me:

1. Make sure Rust is installed (`cargo --version`). If it is not, install it with rustup
   (https://rustup.rs) after asking me.
2. Run: cargo install --git https://github.com/lille-morille/prins-stickers
   and check that `prins --help` works (~/.cargo/bin must be on PATH).
3. Install the skill into the skills folder of the harness you are running in right now:
   - Claude Code: prins skill --install
   - Codex:       prins skill --install --dir ~/.codex/skills
   - Any other harness: find where it loads skills or agent instructions and run
     `prins skill --install --dir <that folder>`, or save the output of `prins skill`
     wherever your rules/instructions live (e.g. AGENTS.md, .cursor/rules).
4. Run `prins doctor` and show me the result.
5. Set up my printer: tell me to switch on the M110 and disconnect it from the phone app,
   run `prins printer scan`, show me the list and ask which device is mine. Do not guess.
   Save my choice with `prins printer save <id>`.
6. Make a test sticker that says "Hello" with `prins new`, `prins draw <id> poster say=Hello`,
   and `prins preview <id>`, and show me the image. Only print it if I say yes.
```

## Install by hand

```sh
cargo install --git https://github.com/lille-morille/prins-stickers   # puts `prins` in ~/.cargo/bin
prins skill --install                         # Claude Code: ~/.claude/skills/prins-stickers/SKILL.md
prins skill --install --dir ~/.codex/skills   # or another agent's skills folder
```

Or from a clone: `cargo build --release` and use `./target/release/prins`.

On macOS the terminal needs Bluetooth permission (System Settings > Privacy & Security > Bluetooth). On Linux, `btleplug` needs BlueZ and the D-Bus development headers (`libdbus-1-dev`) to build.

## The loop

```sh
prins printer scan                 # once: list nearby Bluetooth devices (saves nothing)
prins printer save <id>            # once: save the one the user says is the printer

prins new                          # prints an id such as warm-fox
prins draw warm-fox poster say="Back at 3" sub="Lunch" pic=🍔
prins preview warm-fox             # prints the PNG's path: open it, look at it
prins draw warm-fox undo           # change your mind
prins print warm-fox               # uses ONE label
```

Always preview and get a yes before `print`. A print uses one label per copy and thermal prints fade, so `--copies` is for when someone asked for copies.

## Commands

| Command | What it does |
|---|---|
| `prins new [--name NAME]` | Create an empty drawing and print its id. |
| `prins draw [ID] <tool> [JSON \| - \| key=value ...]` | Run one design tool on the drawing. Prints the tool's text result and `ops: <n>`. |
| `prins tools [NAME] [--json] [--guide]` | One line per tool, or one tool's description and JSON schema. `--guide` prints the short guide. |
| `prins catalog [KIND]` | Names of figures, patterns, frames, fonts, looks, dithers, ops, system_fonts, as JSON. |
| `prins preview [ID] [-o PATH] [--scale N]` | Render to a PNG (default `<tmp>/prins/<id>.png`, scale 2, sharp pixels). Prints only the absolute path. A blank drawing is an error. |
| `prins print [ID] [--copies N] [--dry-run]` | Print on the saved printer. `--dry-run` writes the exact 1-bit image that would be sent to `<tmp>/prins/<id>-print.png`, prints its path and sends nothing. |
| `prins drawings [--json] [--prune DAYS]` | List drawings, newest first. `--prune` deletes drawings not updated for DAYS days and prints what it removed. |
| `prins show [ID]` | The drawing's ops as JSON. |
| `prins printer scan\|save\|show\|forget\|test\|calibrate` | Find, choose and test the printer. `scan` only listens; `save` needs an explicit id; `test` and `calibrate` print one label (use `--dry-run` to look first). |
| `prins doctor` | Home folder, saved printer, sender, fonts, emoji font, cache. Does not scan Bluetooth. |
| `prins skill [--install] [--dir DIR]` | Print the agent skill, or install it. |

Run any command with `--help` for details.

### Drawing arguments

`draw` takes the tool name with or without `draw_` (`text` is `draw_text`, `poster` is `draw_poster`). Arguments are one of:

- a JSON object: `prins draw warm-fox shape '{"kind":"rrect","x":4,"y":4,"w":304,"h":208,"fill":false}'`
- `-`, to read the JSON object from stdin: `echo '{"say":"Fragile"}' | prins draw warm-fox poster -`
- `key=value` pairs. A value that is valid JSON is read as JSON, anything else is a string: `size=40 s="Hello world" data=[1,2,3]`. To force text that looks like a number, quote it as JSON: `big='"15"'`.

Photos print best with `image`'s `dot` option: `dot=2` dithers on 2x2 dots so they do not run together on thermal paper, and `contrast` and `bright` (1 = unchanged) push the tones apart first. For example `prins draw warm-fox image src=pic.jpg fit=cover dot=2 contrast=1.8 bright=1.35`.

`undo`, `clear`, `catalog`, `save_design`, `load_design` and `compare_looks` are tools too. `preview` and `compare_looks` return an image: it is written to `<tmp>/prins/<id>-<tool>.png` and the path is printed.

When an agent works on one drawing for a while, `export PRINS_DRAWING=warm-fox` lets it leave the id out.

### Output and errors

Success prints only what is described above, with no decoration, so a script can read it. Failure prints one line `prins: <message>` on stderr and exits 1; usage mistakes do too. A tool error never changes the drawing: it is only written when the whole call worked. Two agents drawing on the same id queue up behind a lock; different ids never wait for each other.

## Environment variables

| Variable | Meaning |
|---|---|
| `PRINS_HOME` | The `.prins` folder. Default `~/.prins`. Tests use it to stay away from yours. |
| `PRINS_NAME` | The sender that posters may print ("from Sam"). Else the `name` in `config.json`. The agent never chooses it. |
| `PRINS_DRAWING` | The drawing id used when a command is given none. |
| `PRINS_PRINTER` | A printer id or name that overrides the saved one for this run. |

## Files under `~/.prins`

| Path | What |
|---|---|
| `drawings/<id>.json` | A drawing: `{"version":1,"created":..,"updated":..,"printed":n,"ops":[...]}`. Replaying the ops redraws it. `<id>.lock` guards it. |
| `designs/<name>.json` | Designs saved with `save_design` (a bare list of ops). |
| `config.json` | The saved printer and the optional `name`. Unknown keys are kept. |
| `cache/` | Downloaded logos and pictures. |

## Printer setup

1. Switch the printer on. Make sure the phone app is not connected to it: it only takes one connection.
2. `prins printer scan` lists devices, likely printers first. Nothing is saved.
3. Ask the user which one is theirs (an agent must not guess), then `prins printer save <id>`. It checks the id against a fresh scan; `--no-verify` saves it without scanning.
4. `prins printer test --dry-run`, then `prins printer test` to print one label and look at it.

On macOS the terminal needs Bluetooth permission (System Settings > Privacy & Security > Bluetooth). On macOS the printer id is a CoreBluetooth UUID, not a MAC address.

## Development

```sh
cargo build && cargo test
```

`tests/cli.rs` runs the real binary against a temporary `PRINS_HOME` and never prints (always `--dry-run`). The agent skill lives in [skills/prins-stickers/SKILL.md](skills/prins-stickers/SKILL.md) and is compiled into the binary, so `prins skill` always matches the version you installed.

Not affiliated with the maker of the M110. Say "sticky note" rather than "Post-it", which is a 3M trademark.

## License

MIT, see [LICENSE](LICENSE). Do whatever you want with it.
