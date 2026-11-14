---
name: prins-stickers
description: Design and print stickers, labels, sticky notes, badges and QR code labels on an M110 thermal label printer with the `prins` command line. Use when the user asks to print a sticker, label, sticky note, badge or QR code label, to make a label, or to design a sticker.
---

# Making and printing stickers with `prins`

`prins` is one command line tool. You design on a numbered drawing, preview it as a PNG, and print it on the user's M110 over Bluetooth. No server, no board. Check `prins --help` if the binary is missing (install: `cargo install --git https://github.com/lille-morille/prins-stickers`).

## The loop

1. **Setup, once.** Run `prins printer show`.
   - A printer is shown: go on.
   - "No printer saved": run `prins printer scan`, SHOW the list to the user, and ASK which device is their printer. Only after they confirm, run `prins printer save <id>`. Never save a printer the user did not pick.
2. **Start a drawing.** `prins new` prints an id such as `warm-fox`. Use it everywhere below. (`export PRINS_DRAWING=warm-fox` lets you leave the id out.)
3. **Design.** Start with the poster tool, which lays out a whole sticker from its meaning:
   `prins draw warm-fox poster say="Back at 3" sub="Lunch" pic=🍔`
4. **Preview, always.** `prins preview warm-fox` prints the path of a PNG. Open that image and show it to the user in the chat. Ask: approve, or what to change? Iterate with `prins draw` (add or fix things) or `prins draw warm-fox undo`. Preview again after every change.
5. **Print, only after the user approves.** `prins print warm-fox`. One print uses ONE label, and thermal prints fade over months. Do not pass `--copies` unless the user asks for copies. To check without printing: `prins print warm-fox --dry-run`.

A successful print only means the data was sent. Ask the user whether the label came out.

## The poster tool

`prins draw <id> poster key=value ...` (or a JSON object, or `-` to read JSON from stdin). Fields, all optional except that something must be said:

- `say`: the main words, 1 to 4 of them.
- `sub`: a short detail line.
- `big`: a number or time to feature, like `big=7:30` or `big=15%`.
- `pic`: the picture: an emoji (`pic=🎂`) or a brand logo (`pic=logo:spotify`).
- `data`: numbers for a small chart: `data=[3,5,2]`.
- `items`: a list, like a checklist: `items='["milk","eggs"]'`.
- `qr`: text or a URL to encode as a QR code.
- `look`: the style. Leave it out to auto-choose; the result names the look it used and others that fit.
- `seed`: a number that changes the random parts. Same input and seed always give the same sticker.

To pick a style visually: `prins draw warm-fox compare_looks say="Back at 3"` writes one numbered PNG of several looks and prints its path. Show it to the user, then re-run `poster` with their `look=`.

A value that is valid JSON is read as JSON, so `big=15` is a number. To force text, quote it for JSON: `big='"15"'`.

## Other tools, only when needed

`prins tools` lists them in one line each. `prins tools <name>` shows one tool's arguments, so read only what you need. `prins catalog` lists figures, patterns, frames, fonts and looks. Common ones: `text`, `figure`, `pattern`, `frame`, `shape`, `logo`, `qr`, `image`, `svg`, `transform`, `batch`. Tool names work with or without `draw_` (`text` is `draw_text`). Draw several things in one call with `batch` to save steps. `save_design` and `load_design` keep a design under a name.

## The grid and what prints well

- A sticker is exactly 312 x 216 pixels, 1 bit, every pixel on or off: a 40 mm label at 203 dpi. There is no gray, only dots.
- Text must be at least 16 px tall to stay readable. The poster tool warns when something is too small.
- Keep black under about 60 percent of the label. More is hard on the print head and prints streaky.
- Leave a small margin; the label edge can clip.
- **Photos, engravings and other detailed pictures** (`image` op): thermal dots spread into their neighbours, so a full-resolution dither prints as merged blobs even when the preview looks fine. Dither on a coarser grid and push the tones apart: `{"op":"image","src":"pic.png","fit":"cover","dot":2,"contrast":1.8,"bright":1.35}`. `dot` 2 makes every dot 2x2 pixels (3 is coarser still); `contrast` and `bright` default to 1. Leave `dot` out for logos, SVGs and line art, which are already crisp.
- A detailed picture needs a tight crop at this size: crop the source to the face or subject first, or the subject drowns in texture.

## Rules for content

- Never invent facts for a sticker: names, dates, times, numbers. Use what the user gave you, or ask.
- The "from" line on a poster comes from `PRINS_NAME` (or `name` in `~/.prins/config.json`), never from you. Do not put a sender in `say` or `sub` yourself.
- In anything customer-facing, say "sticky note", not "Post-it": Post-it is a 3M trademark.

## Where things live

- Drawings: `~/.prins/drawings/<id>.json` (a list of ops). `prins drawings` lists them, `prins show <id>` prints the ops, `prins drawings --prune 30` deletes old ones. `PRINS_HOME` moves the folder.
- `PRINS_DRAWING`: default drawing id. `PRINS_PRINTER`: printer id or name for one run, over the saved one.
- Errors print one line on stderr (`prins: ...`) and exit 1. A failed `draw` leaves the drawing unchanged.
- `prins doctor` shows the home folder, printer, fonts and emoji font in one screen.

## Troubleshooting

- **Printer not found or scan shows nothing:** switch it on, make sure it is NOT connected to the phone app (it takes one connection), stay near it, and check Bluetooth is on. On macOS the terminal needs permission: System Settings > Privacy & Security > Bluetooth.
- **Another process holds the printer:** another app or process (the phone app, another `prins print`) keeps the connection. Close it, then try again.
- **"No printer saved":** do step 1 above.
- **"emoji font missing" in `prins doctor`:** use a named figure (`prins catalog figures`) or `logo:` instead of an emoji.
- **Blank drawing refused:** draw something first.
- **Test the printer itself:** `prins printer test --dry-run` shows what would be sent; `prins printer test` prints one label.
