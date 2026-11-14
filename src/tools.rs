//! The drawing tools. One definition, used by the MCP server and by the internal designer. Port of `tools.py`.
//!
//! Tokens are the budget. Tool definitions stay short because they sit in every request, the names of
//! figures and fonts live behind `catalog`, and draw_batch does a whole design in one call.
//!
//! Left out on purpose: `print_note`, `list_friends` and `send_to_friend`. Printing is `prins print` here.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use image::GrayImage;
use serde_json::{json, Map, Value};

use crate::canvas::{self, pyval, Canvas, DesignError};
use crate::poster;

pub const GUIDE: &str = concat!(
    "Prins prints notes on a 312x216 black and white pixel grid. Origin is top-left.\n",
    "Start with draw_poster: it gives the best design for the fewest words. Use the other draw_* tools for details it can't do, then print_note or send_to_friend. draw_batch is the cheapest way to do several steps.\n",
    "x,y,w,h default to the whole grid. ink is black, white, invert, or a pattern name (dots:6, gray:30).\n",
    "For a brand logo use draw_logo first. For anything else use draw_svg (path data), draw_image (URL, path, base64) or draw_pixels. Any design is possible.\n",
    "preview shows the note. Text auto-sizes to fit its box when size is 0."
);

/// What a tool hands back: a line of text and sometimes a PNG for the agent to look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub text: String,
    pub png: Option<Vec<u8>>,
}

impl ToolResult {
    fn text(text: impl Into<String>) -> ToolResult {
        ToolResult {
            text: text.into(),
            png: None,
        }
    }

    fn ok() -> ToolResult {
        ToolResult::text("ok")
    }
}

#[derive(Default)]
pub struct Session {
    pub canvas: Canvas,
    pub designs_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: &'static str,
    pub description: String,
    pub schema: Value,
    /// The internal designer may use it too.
    pub internal: bool,
}

// --- schema pieces ------------------------------------------------------------------------------

fn boxed() -> Value {
    json!({
        "x": {"type": "number"},
        "y": {"type": "number"},
        "w": {"type": "number"},
        "h": {"type": "number"},
    })
}

fn ink() -> Value {
    json!({"ink": {"type": "string"}, "rot": {"type": "number"}})
}

fn ink_only() -> Value {
    json!({"ink": {"type": "string"}})
}

fn s(description: &str) -> Value {
    if description.is_empty() {
        json!({"type": "string"})
    } else {
        json!({"type": "string", "description": description})
    }
}

/// `_props`: an object schema from groups of properties.
fn props(groups: Vec<Value>, required: &[&str]) -> Value {
    let mut merged = Map::new();
    for group in groups {
        if let Value::Object(map) = group {
            merged.extend(map);
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(merged));
    if !required.is_empty() {
        schema.insert("required".into(), json!(required));
    }
    Value::Object(schema)
}

fn poster_fields() -> Value {
    json!({
        "say": s("fewest words that carry it, 1-4"),
        "sub": s("short detail line"),
        "big": s("number or time to feature"),
        "pic": s("subject: an emoji, or logo:brand"),
        "data": {"type": "array", "items": {"type": "number"}},
        "items": {"type": "array", "items": {"type": "string"}},
        "qr": s(""),
        "seed": {"type": "number"},
    })
}

fn tool(name: &'static str, description: &str, schema: Value, internal: bool) -> Tool {
    Tool {
        name,
        description: description.to_string(),
        schema,
        internal,
    }
}

/// The tools, in Python's order.
pub fn tools() -> Vec<Tool> {
    let look = format!("omit to auto-choose. {}", poster::LOOK_GUIDE);
    vec![
        tool(
            "draw_poster",
            "Designs a whole note from its meaning: layout, type and shading. Never invent facts.",
            props(vec![json!({"look": s(&look)}), poster_fields()], &[]),
            true,
        ),
        tool(
            "compare_looks",
            "See content in the looks that suit it, as one numbered image. Takes draw_poster's fields, plus n. Changes nothing.",
            props(vec![json!({"say": s(""), "n": {"type": "number"}})], &[]),
            false,
        ),
        tool(
            "draw_batch",
            "Run many ops in one call. Each op is {op:poster|text|figure|pattern|frame|shape|image|svg|logo|pixels|formula|qr|transform|repeat|scatter|clear, ...same args as its draw_ tool}. repeat: {n,dx,dy,ops}. scatter: {n,seed,x,y,w,h,ops}.",
            json!({"type": "object", "properties": {"ops": {"type": "array", "items": {"type": "object"}}}, "required": ["ops"]}),
            true,
        ),
        tool(
            "draw_text",
            "Text. size 0 fits the box. align l|c|r, valign t|m|b.",
            props(
                vec![
                    json!({"s": s(""), "font": s("see catalog fonts, default sans-bold"), "size": {"type": "number"}, "align": s(""), "valign": s(""), "wrap": {"type": "boolean"}}),
                    boxed(),
                    ink(),
                ],
                &["s"],
            ),
            true,
        ),
        tool(
            "draw_figure",
            "A figure (see catalog figures), or any emoji as name emoji:\u{1f382}.",
            props(vec![json!({"name": s("")}), boxed(), ink()], &["name"]),
            true,
        ),
        tool(
            "draw_pattern",
            "Fill a box with a pattern (see catalog patterns). name:size sets scale, gray:N a flat tone. shape ellipse for a round fill.",
            props(vec![json!({"name": s(""), "shape": s("")}), boxed(), ink()], &["name"]),
            true,
        ),
        tool(
            "draw_frame",
            "A border (see catalog frames). Any figure name also repeats around the edge.",
            props(
                vec![
                    json!({"name": s(""), "size": {"type": "number"}, "inset": {"type": "number"}}),
                    boxed(),
                    ink_only(),
                ],
                &[],
            ),
            true,
        ),
        tool(
            "draw_shape",
            "kind: rect, rrect (r), ellipse, line, curve, polygon (pts [[x,y],..]), arc, pie, chord (a0,a1 degrees). fill false for outline, lw line width.",
            props(
                vec![
                    json!({"kind": s(""), "pts": {"type": "array"}, "fill": {"type": "boolean"}, "lw": {"type": "number"}, "r": {"type": "number"}, "a0": {"type": "number"}, "a1": {"type": "number"}}),
                    boxed(),
                    ink_only(),
                ],
                &["kind"],
            ),
            true,
        ),
        tool(
            "draw_logo",
            "A brand logo by name (spotify, nike...), from Simple Icons.",
            props(vec![json!({"name": s("")}), boxed(), ink()], &["name"]),
            true,
        ),
        tool(
            "draw_svg",
            "Any vector. d: SVG path data with vb viewBox (default 0 0 24 24), or svg: full markup, or src: SVG URL or path.",
            props(
                vec![
                    json!({"d": s(""), "vb": s(""), "evenodd": {"type": "boolean"}, "svg": s(""), "src": s("")}),
                    boxed(),
                    ink(),
                ],
                &[],
            ),
            true,
        ),
        tool(
            "draw_image",
            "Any image, dithered to one bit. src: URL, file path, or base64. fit contain|cover|stretch. dither atkinson|floyd|bayer|halftone|threshold. dot 2 dithers on 2x2 dots so photos do not blur on thermal paper; contrast and bright scale tones (1 = unchanged).",
            props(
                vec![
                    json!({"src": s(""), "fit": s(""), "dither": s(""), "threshold": {"type": "number"}, "invert": {"type": "boolean"}, "dot": {"type": "number"}, "contrast": {"type": "number"}, "bright": {"type": "number"}}),
                    boxed(),
                    ink(),
                ],
                &["src"],
            ),
            false,
        ),
        tool(
            "draw_pixels",
            "Exact pixels. rows: strings where . is off and anything else on, with scale. Or b64: packed bits (1 = black) with w.",
            props(
                vec![
                    json!({"rows": {"type": "array", "items": s("")}, "b64": s(""), "x": {"type": "number"}, "y": {"type": "number"}, "w": {"type": "number"}, "scale": {"type": "number"}}),
                    ink(),
                ],
                &[],
            ),
            true,
        ),
        tool(
            "draw_formula",
            "Per-pixel math: true paints. Vars x,y,u,v(0-1),r,a(polar),W,H. Funcs sin,cos,tan,atan2,sqrt,abs,floor,hypot,min,max,noise,pow. Example: sin(r/6+a*3)>0",
            props(vec![json!({"f": s("")}), boxed(), ink()], &["f"]),
            true,
        ),
        tool(
            "draw_qr",
            "A QR code for s.",
            props(vec![json!({"s": s(""), "ecc": s("l|m|q|h")}), boxed(), ink()], &["s"]),
            true,
        ),
        tool(
            "transform",
            "Change a region. kind: invert, flip-h, flip-v, rotate (deg), mirror-h, mirror-v, shift (dx,dy), pixelate (size), outline.",
            props(
                vec![
                    json!({"kind": s(""), "deg": {"type": "number"}, "dx": {"type": "number"}, "dy": {"type": "number"}, "size": {"type": "number"}}),
                    boxed(),
                ],
                &["kind"],
            ),
            true,
        ),
        tool(
            "catalog",
            "Names. kind: looks, figures, patterns, frames, fonts, dithers, ops, system_fonts. Omit for most.",
            props(vec![json!({"kind": s("")})], &[]),
            true,
        ),
        tool("undo", "Undo n steps.", props(vec![json!({"n": {"type": "number"}})], &[]), true),
        tool("clear", "A blank note.", props(vec![], &[]), true),
        tool("preview", "The note as an image.", props(vec![], &[]), false),
        tool(
            "save_design",
            "Save the current note under a name.",
            props(vec![json!({"name": s("")})], &["name"]),
            false,
        ),
        tool(
            "load_design",
            "Draw a saved design.",
            props(vec![json!({"name": s("")})], &["name"]),
            false,
        ),
    ]
}

/// `TOOLS`
pub static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(tools);

// --- running ------------------------------------------------------------------------------------

/// Runs a tool by name. `args` is the JSON object the agent sent (null counts as none).
pub fn run_tool(
    session: &mut Session,
    name: &str,
    args: &Value,
) -> Result<ToolResult, DesignError> {
    let empty = Value::Object(Map::new());
    let args = if pyval::truthy(args) { args } else { &empty };
    if !args.is_object() {
        return Err(DesignError("Tool arguments must be an object.".into()));
    }
    match name {
        "draw_poster" => poster_tool(session, args),
        "compare_looks" => compare_tool(session, args),
        "draw_batch" => batch_tool(session, args),
        "draw_text" => draw(session, "text", args),
        "draw_figure" => draw(session, "figure", args),
        "draw_pattern" => draw(session, "pattern", args),
        "draw_frame" => draw(session, "frame", args),
        "draw_shape" => draw(session, "shape", args),
        "draw_logo" => draw(session, "logo", args),
        "draw_svg" => draw(session, "svg", args),
        "draw_image" => draw(session, "image", args),
        "draw_pixels" => draw(session, "pixels", args),
        "draw_formula" => draw(session, "formula", args),
        "draw_qr" => draw(session, "qr", args),
        "transform" => draw(session, "transform", args),
        "catalog" => catalog_tool(args),
        "undo" => undo_tool(session, args),
        "clear" => {
            session.canvas.clear();
            Ok(ToolResult::ok())
        }
        "preview" => preview_tool(session),
        "save_design" => save_tool(session, args),
        "load_design" => load_tool(session, args),
        _ => Err(DesignError(format!(
            "Unknown tool {}.",
            crate::assets::py_repr(name)
        ))),
    }
}

/// `_draw`: `{"op": op, **args}`, so an `op` in the arguments wins, as in Python.
fn draw(session: &mut Session, op: &str, args: &Value) -> Result<ToolResult, DesignError> {
    session.canvas.apply(with_op(op, args))?;
    Ok(ToolResult::ok())
}

fn with_op(op: &str, args: &Value) -> Value {
    let mut map = Map::new();
    map.insert("op".into(), Value::String(op.into()));
    if let Value::Object(extra) = args {
        map.extend(extra.clone());
    }
    Value::Object(map)
}

fn batch_tool(session: &mut Session, args: &Value) -> Result<ToolResult, DesignError> {
    let Some(Value::Array(ops)) = args.get("ops") else {
        return Err(DesignError("ops must be a list.".into()));
    };
    session.canvas.apply_all(ops)?;
    Ok(ToolResult::ok())
}

fn poster_tool(session: &mut Session, args: &Value) -> Result<ToolResult, DesignError> {
    session.canvas.apply(with_op("poster", args))?;
    Ok(ToolResult::text(match &session.canvas.report {
        Some(report) => report.summary(),
        None => "ok".to_string(),
    }))
}

fn compare_tool(session: &mut Session, args: &Value) -> Result<ToolResult, DesignError> {
    // int(args.get("n", 4) or 4)
    let count = match args.get("n").filter(|v| pyval::truthy(v)) {
        Some(n) => pyval::to_int(n).map_err(DesignError)?,
        None => 4,
    };
    let (png, names) = poster::compare(args, count.max(0) as usize, &session.canvas.sender)
        .map_err(DesignError)?;
    Ok(ToolResult {
        text: format!("{names}. Pick one and call draw_poster with its look."),
        png: Some(png),
    })
}

fn catalog_tool(args: &Value) -> Result<ToolResult, DesignError> {
    let found = match args.get("kind") {
        None | Some(Value::Null) => canvas::catalog(None)?,
        Some(Value::String(kind)) => canvas::catalog(Some(kind))?,
        Some(other) if !pyval::truthy(other) => canvas::catalog(None)?,
        Some(other) => {
            return Err(DesignError(format!(
                "Unknown catalog {}. Use one of figures, patterns, frames, fonts, looks, dithers, ops, system_fonts.",
                pyval::repr(other)
            )))
        }
    };
    // Python's dict order, whatever order the JSON map keeps.
    const ORDER: [&str; 8] = [
        "figures",
        "patterns",
        "frames",
        "fonts",
        "looks",
        "dithers",
        "ops",
        "system_fonts",
    ];
    let parts: Vec<String> = ORDER
        .iter()
        .filter_map(|key| found.get(key).map(|list| (key, list)))
        .map(|(key, list)| format!("\"{key}\":{}", compact(list)))
        .collect();
    Ok(ToolResult::text(format!("{{{}}}", parts.join(","))))
}

/// `json.dumps(value, ensure_ascii=False, separators=(",", ":"))`
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn undo_tool(session: &mut Session, args: &Value) -> Result<ToolResult, DesignError> {
    let count = match args.get("n") {
        Some(n) => pyval::to_int(n).map_err(DesignError)?,
        None => 1,
    };
    session.canvas.try_undo(count.max(0) as usize)?;
    Ok(ToolResult::ok())
}

/// An image this size costs far fewer tokens than describing the pixels in text.
fn preview_tool(session: &Session) -> Result<ToolResult, DesignError> {
    Ok(ToolResult {
        text: format!("{} ops", session.canvas.ops.len()),
        png: Some(png_bytes(&session.canvas.image)?),
    })
}

/// A grayscale PNG, packed as small as it will go.
pub fn png_bytes(image: &GrayImage) -> Result<Vec<u8>, DesignError> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ExtendedColorType, ImageEncoder};
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Best, FilterType::Adaptive)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::L8,
        )
        .map_err(|e| DesignError(format!("Couldn't make the preview: {e}")))?;
    Ok(out)
}

/// `[\w -]{1,60}`, the whole name.
fn valid_design_name(name: &str) -> bool {
    let count = name.chars().count();
    (1..=60).contains(&count)
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ' ' || c == '-')
}

fn design_path(session: &Session, name: &str) -> Result<PathBuf, DesignError> {
    let Some(dir) = &session.designs_dir else {
        return Err(DesignError("Saved designs aren't available here.".into()));
    };
    if !valid_design_name(name) {
        return Err(DesignError(
            "A design name is letters, numbers, spaces, - and _.".into(),
        ));
    }
    Ok(dir.join(format!("{name}.json")))
}

fn name_arg(args: &Value) -> String {
    args.get("name").map(pyval::to_str).unwrap_or_default()
}

fn save_tool(session: &mut Session, args: &Value) -> Result<ToolResult, DesignError> {
    let name = name_arg(args);
    let path = design_path(session, &name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| DesignError(format!("Couldn't save the design: {e}")))?;
    }
    let text = compact(&Value::Array(session.canvas.ops.clone()));
    std::fs::write(&path, text)
        .map_err(|e| DesignError(format!("Couldn't save the design: {e}")))?;
    Ok(ToolResult::text(format!("saved {name}")))
}

fn saved_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        path.file_stem()
                            .and_then(|s| s.to_str())
                            .map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn load_tool(session: &mut Session, args: &Value) -> Result<ToolResult, DesignError> {
    let name = name_arg(args);
    let path = design_path(session, &name)?;
    if !path.exists() {
        let saved = path.parent().map(saved_names).unwrap_or_default();
        let listing = if saved.is_empty() {
            "none".to_string()
        } else {
            saved.join(", ")
        };
        return Err(DesignError(format!(
            "No design named {}. Saved: {listing}.",
            crate::assets::py_repr(&name)
        )));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| {
        DesignError(format!(
            "Couldn't read the design {}: {e}",
            crate::assets::py_repr(&name)
        ))
    })?;
    let ops: Value = serde_json::from_str(&text).map_err(|e| {
        DesignError(format!(
            "The design {} is damaged: {e}",
            crate::assets::py_repr(&name)
        ))
    })?;
    let Value::Array(ops) = ops else {
        return Err(DesignError(format!(
            "The design {} is damaged: it isn't a list of ops.",
            crate::assets::py_repr(&name)
        )));
    };
    session.canvas.apply_all(&ops)?;
    Ok(ToolResult::ok())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const DEFINITIONS: &str = include_str!("../tests/fixtures/tool_definitions.json");

    const NOT_HERE: [&str; 3] = ["print_note", "list_friends", "send_to_friend"];

    /// Tools with options the Python server does not have.
    const RUST_ONLY: [(&str, &[&str]); 1] = [("draw_image", &["dot", "contrast", "bright"])];

    fn run(session: &mut Session, name: &str, args: Value) -> ToolResult {
        run_tool(session, name, &args).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn fail(session: &mut Session, name: &str, args: Value) -> String {
        run_tool(session, name, &args).expect_err("should fail").0
    }

    fn black(session: &Session) -> usize {
        session
            .canvas
            .image
            .pixels()
            .filter(|p| p.0[0] < 128)
            .count()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prins-tools-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn definitions_are_byte_for_byte_pythons() {
        let python: Value = serde_json::from_str(DEFINITIONS).unwrap();
        assert_eq!(python["guide"].as_str().unwrap(), GUIDE);
        assert_eq!(python["look_guide"].as_str().unwrap(), poster::LOOK_GUIDE);
        let expected = python["tools"].as_array().unwrap();
        let ours = tools();
        let names: Vec<&str> = ours.iter().map(|t| t.name).collect();
        let wanted: Vec<&str> = expected
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .filter(|name| !NOT_HERE.contains(name))
            .collect();
        assert_eq!(names, wanted, "same tools in the same order");
        for tool in &ours {
            let theirs = expected.iter().find(|t| t["name"] == tool.name).unwrap();
            let description = theirs["description"].as_str().unwrap();
            if let Some(extra) = RUST_ONLY.iter().find(|(name, _)| *name == tool.name) {
                // Options the Rust CLI added: Python's text and properties, plus these.
                assert!(tool.description.starts_with(description), "{}", tool.name);
                let mut schema = tool.schema.clone();
                let props = schema["properties"].as_object_mut().unwrap();
                for key in extra.1 {
                    assert!(props.remove(*key).is_some(), "{} {key}", tool.name);
                }
                assert_eq!(schema, theirs["input_schema"], "{}", tool.name);
            } else {
                assert_eq!(tool.description, description, "{}", tool.name);
                assert_eq!(tool.schema, theirs["input_schema"], "{}", tool.name);
            }
            assert_eq!(
                tool.internal,
                theirs["internal"].as_bool().unwrap(),
                "{}",
                tool.name
            );
        }
        assert_eq!(TOOLS.len(), ours.len());
    }

    fn python_dumps(value: &Value) -> String {
        match value {
            Value::Array(items) => format!(
                "[{}]",
                items
                    .iter()
                    .map(python_dumps)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Object(map) => format!(
                "{{{}}}",
                map.iter()
                    .map(|(k, v)| format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap(),
                        python_dumps(v)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            other => serde_json::to_string(other).unwrap(),
        }
    }

    #[test]
    fn every_tool_has_a_schema_and_short_definitions() {
        for tool in tools() {
            assert_eq!(tool.schema["type"], "object", "{}", tool.name);
            assert!(tool.description.len() < 350, "{}", tool.name);
        }
        let defs: Vec<Value> = tools()
            .iter()
            .map(
                |t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}),
            )
            .collect();
        let size = python_dumps(&Value::Array(defs)).chars().count();
        assert!(size < 9000, "{size} characters of tool definitions");
    }

    #[test]
    fn the_internal_designer_gets_no_files_or_previews() {
        let internal: BTreeSet<&str> = tools()
            .iter()
            .filter(|t| t.internal)
            .map(|t| t.name)
            .collect();
        for name in [
            "draw_image",
            "save_design",
            "load_design",
            "preview",
            "compare_looks",
        ] {
            assert!(!internal.contains(name), "{name}");
        }
        for name in ["draw_batch", "draw_logo", "draw_svg", "catalog"] {
            assert!(internal.contains(name), "{name}");
        }
    }

    #[test]
    fn the_guide_describes_the_grid() {
        assert!(GUIDE.contains("312x216"));
    }

    #[test]
    fn batch_and_single_tools_draw_the_same() {
        let mut one = Session::default();
        run(
            &mut one,
            "draw_figure",
            json!({"name": "heart", "w": 80, "h": 80}),
        );
        run(
            &mut one,
            "draw_text",
            json!({"s": "Hi", "x": 90, "w": 100, "h": 50}),
        );
        let mut many = Session::default();
        run(
            &mut many,
            "draw_batch",
            json!({"ops": [
                {"op": "figure", "name": "heart", "w": 80, "h": 80},
                {"op": "text", "s": "Hi", "x": 90, "w": 100, "h": 50}
            ]}),
        );
        assert_eq!(one.canvas.image.as_raw(), many.canvas.image.as_raw());
        assert_eq!(one.canvas.ops.len(), 2);
    }

    #[test]
    fn preview_returns_a_small_grayscale_png() {
        let mut session = Session::default();
        run(&mut session, "draw_figure", json!({"name": "star"}));
        let result = run(&mut session, "preview", json!({}));
        let png = result.png.expect("a picture");
        assert!(png.starts_with(b"\x89PNG"));
        assert!(png.len() < 8000, "{}", png.len());
        assert_eq!(result.text, "1 ops");
        let decoded = image::load_from_memory(&png).unwrap();
        assert!(matches!(decoded, image::DynamicImage::ImageLuma8(_)));
        assert_eq!(decoded.to_luma8().as_raw(), session.canvas.image.as_raw());
    }

    #[test]
    fn catalog_lists_names_compactly() {
        let mut session = Session::default();
        let text = run(&mut session, "catalog", json!({"kind": "figures"})).text;
        assert!(text.contains("heart") && text.len() < 800, "{}", text.len());
        assert!(text.starts_with("{\"figures\":[\""));
        let everything = run(&mut session, "catalog", json!({})).text;
        assert!(everything.starts_with("{\"figures\":[") && everything.contains("\"patterns\":["));
        assert!(!everything.contains("system_fonts"));
        assert!(fail(&mut session, "catalog", json!({"kind": "nope"}))
            .starts_with("Unknown catalog 'nope'."));
        assert!(fail(&mut session, "catalog", json!({"kind": 5})).starts_with("Unknown catalog 5."));
    }

    #[test]
    fn saved_designs_reuse_for_free() {
        let dir = temp_dir("designs");
        let mut session = Session {
            designs_dir: Some(dir.clone()),
            ..Session::default()
        };
        run(&mut session, "draw_figure", json!({"name": "star"}));
        assert_eq!(
            run(&mut session, "save_design", json!({"name": "my star"})).text,
            "saved my star"
        );
        let picture = session.canvas.image.clone();
        run(&mut session, "clear", json!({}));
        assert!(session.canvas.blank());
        run(&mut session, "load_design", json!({"name": "my star"}));
        assert_eq!(session.canvas.image.as_raw(), picture.as_raw());
        assert_eq!(
            fail(
                &mut session,
                "load_design",
                json!({"name": "../../etc/passwd"})
            ),
            "A design name is letters, numbers, spaces, - and _."
        );
        assert_eq!(
            fail(&mut session, "load_design", json!({"name": "missing"})),
            "No design named 'missing'. Saved: my star."
        );
        // A bare ops list, compact (key order is the JSON map's).
        let saved = std::fs::read_to_string(dir.join("my star.json")).unwrap();
        assert!(!saved.contains(' ') && !saved.contains('\n'), "{saved}");
        assert_eq!(
            serde_json::from_str::<Value>(&saved).unwrap(),
            json!([{"op": "figure", "name": "star"}])
        );
        let mut nowhere = Session::default();
        assert_eq!(
            fail(&mut nowhere, "save_design", json!({"name": "x"})),
            "Saved designs aren't available here."
        );
        let mut empty = Session {
            designs_dir: Some(temp_dir("none")),
            ..Session::default()
        };
        assert_eq!(
            fail(&mut empty, "load_design", json!({"name": "x"})),
            "No design named 'x'. Saved: none."
        );
        assert!(fail(&mut empty, "save_design", json!({"name": ""})).starts_with("A design name"));
        assert!(
            fail(&mut empty, "save_design", json!({"name": "a".repeat(61)}))
                .starts_with("A design name")
        );
        assert!(fail(&mut empty, "save_design", json!({})).starts_with("A design name"));
    }

    #[test]
    fn errors_reach_the_agent_with_what_to_do() {
        let mut session = Session::default();
        let bad = fail(&mut session, "draw_figure", json!({"name": "nope"}));
        assert!(bad.contains("catalog"), "{bad}");
        run(&mut session, "draw_figure", json!({"name": "star"}));
        assert_eq!(
            fail(&mut session, "nope", json!({})),
            "Unknown tool 'nope'."
        );
        assert_eq!(
            fail(&mut session, "draw_batch", json!({"ops": 5})),
            "ops must be a list."
        );
        assert_eq!(
            fail(&mut session, "draw_batch", json!({})),
            "ops must be a list."
        );
    }

    #[test]
    fn a_batch_keeps_what_drew_before_it_failed() {
        let mut session = Session::default();
        let message = fail(
            &mut session,
            "draw_batch",
            json!({"ops": [{"op": "figure", "name": "star"}, {"op": "figure", "name": "nope"}]}),
        );
        assert!(message.starts_with("Unknown figure 'nope'"));
        assert_eq!(session.canvas.ops.len(), 1);
    }

    #[test]
    fn arguments_may_override_the_op_as_in_python() {
        let mut session = Session::default();
        run(
            &mut session,
            "draw_text",
            json!({"op": "figure", "name": "star"}),
        );
        assert_eq!(session.canvas.ops[0]["op"], "figure");
        assert!(black(&session) > 100);
    }

    #[test]
    fn undo_and_clear() {
        let mut session = Session::default();
        run(&mut session, "draw_figure", json!({"name": "star"}));
        run(
            &mut session,
            "draw_figure",
            json!({"name": "heart", "x": 200, "w": 50, "h": 50}),
        );
        let both = black(&session);
        run(&mut session, "undo", json!({}));
        assert!(black(&session) < both);
        run(&mut session, "undo", json!({"n": 5}));
        assert!(session.canvas.blank());
        assert!(
            fail(&mut session, "undo", json!({"n": "x"})).starts_with("invalid literal for int()")
        );
        run(&mut session, "draw_figure", json!({"name": "star"}));
        run(&mut session, "clear", json!({}));
        assert_eq!(black(&session), 0);
        assert_eq!(
            run_tool(&mut session, "clear", &Value::Null).unwrap().text,
            "ok"
        );
        assert!(run_tool(&mut session, "clear", &json!([1])).is_err());
    }

    #[test]
    fn draw_tools_answer_ok_and_poster_reports_its_look() {
        let mut session = Session::default();
        assert_eq!(
            run(
                &mut session,
                "draw_shape",
                json!({"kind": "rect", "w": 10, "h": 10})
            )
            .text,
            "ok"
        );
        let summary = run(&mut session, "draw_poster", json!({"say": "Hi"})).text;
        assert!(summary.starts_with("Drew look "), "{summary}");
        let compared = run(&mut session, "compare_looks", json!({"say": "Hi", "n": 2}));
        assert!(
            compared
                .text
                .ends_with(". Pick one and call draw_poster with its look."),
            "{}",
            compared.text
        );
        assert!(compared.png.unwrap().starts_with(b"\x89PNG"));
    }

    #[test]
    fn every_draw_tool_reaches_its_op() {
        let table = [
            ("draw_text", "text"),
            ("draw_figure", "figure"),
            ("draw_pattern", "pattern"),
            ("draw_frame", "frame"),
            ("draw_shape", "shape"),
            ("draw_logo", "logo"),
            ("draw_svg", "svg"),
            ("draw_image", "image"),
            ("draw_pixels", "pixels"),
            ("draw_formula", "formula"),
            ("draw_qr", "qr"),
            ("transform", "transform"),
        ];
        for (tool, op) in table {
            assert_eq!(with_op(op, &json!({"x": 1}))["op"], op, "{tool}");
        }
        for (tool, _) in table {
            let mut session = Session::default();
            if let Err(e) = run_tool(&mut session, tool, &json!({})) {
                assert!(!e.0.starts_with("Unknown tool"), "{tool}: {e}");
            }
        }
    }
}
