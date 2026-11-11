//! Recipes (`workflow.*`): one call runs a whole edit through the existing commands (import, a
//! sequence, reframing, transcript cuts, captions, checks, export) as one undo step and answers a
//! short report, so an agent spends a few hundred tokens on an edit instead of many thousands.
//!
//! A built-in recipe is a Rust function with documented defaults. A user recipe is a JSON file
//! `<recipe dir>/<name>.json`, `{"base": "vertical-short", "description": str?, "params": {...}}`:
//! it changes a built-in recipe's defaults and never contains code. Call params override both;
//! an object param (`captions`, `removePauses`, `export`) is merged one level deep, and `false`
//! turns a step off.
//!
//! Stages: `prepare` builds the sequence and transcribes it, then reports the transcript as
//! paragraphs so the agent can choose what to keep; `finish` cuts, captions, checks, saves and
//! exports the open sequence; `all` (default) does both.

use serde_json::{Map, Value, json};

use filmcraft_project::{ClipId, ItemId, ParamValue};

use crate::commands::{CommandSpec, always, bad, bool_p, secs_ms, str_p};
use crate::{EngineError, Result, Session};

const RUN: &str = "workflow.run";
/// A user recipe file larger than this is refused.
const MAX_RECIPE_BYTES: u64 = 64 * 1024;
/// User recipe files listed at most.
const MAX_RECIPES: usize = 200;
/// Media one recipe call takes at most.
const MAX_MEDIA: usize = 500;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Prepare,
    Finish,
    All,
}

struct Builtin {
    name: &'static str,
    description: &'static str,
    /// The params and their defaults (what `workflow.list` shows and user recipes override).
    defaults: fn() -> Value,
    run: fn(&mut Session, Stage, &Value) -> Result<Value>,
}

const BUILTINS: &[Builtin] = &[Builtin {
    name: "vertical-short",
    description: "9:16 short (TikTok, Reels, Shorts) from talking footage: 1080x1920, fill-frame reframe, transcript cuts (keep, filler words, pauses), big captions burned in, quality check, H.264 export at -14 LUFS",
    defaults: vertical_short_defaults,
    run: vertical_short,
}];

fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

// ------------------------------------------------------------------------------- user recipes

struct UserRecipe {
    name: String,
    base: &'static Builtin,
    description: String,
    params: Value,
}

/// A recipe name is its file name: letters, digits, `-` and `_` (never a path).
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Read and check the user recipe `name` (None: there is no such file).
fn user_recipe(s: &Session, name: &str) -> Result<Option<UserRecipe>> {
    let Some(dir) = &s.recipe_dir else { return Ok(None) };
    if !valid_name(name) {
        return Ok(None);
    }
    let path = dir.join(format!("{name}.json")).to_string_lossy().into_owned();
    let Ok(size) = s.services.file_size(&path) else { return Ok(None) };
    let fail = |why: String| bad(RUN, format!("recipe file {path}: {why}"));
    if size > MAX_RECIPE_BYTES {
        return Err(fail(format!("{size} bytes; at most {MAX_RECIPE_BYTES}")));
    }
    let bytes = s.services.read_file(&path).map_err(|e| fail(e.to_string()))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| fail(e.to_string()))?;
    let base = str_p(&v, "base").ok_or_else(|| fail("`base` (a built-in recipe) is required".into()))?;
    let base = builtin(base).ok_or_else(|| fail(format!("unknown base recipe `{base}`")))?;
    let params = match v.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => return Err(fail("`params` must be an object".into())),
    };
    let description = str_p(&v, "description").unwrap_or(base.description).chars().take(500).collect();
    Ok(Some(UserRecipe { name: name.to_string(), base, description, params }))
}

/// The user recipes in the recipe directory (unreadable or invalid files are skipped).
fn user_recipes(s: &Session) -> Vec<UserRecipe> {
    let Some(dir) = &s.recipe_dir else { return Vec::new() };
    let Some(Ok(mut names)) = s.services.list_dir(&dir.to_string_lossy()) else { return Vec::new() };
    names.sort();
    names
        .iter()
        .filter_map(|n| n.strip_suffix(".json"))
        .filter(|n| valid_name(n) && builtin(n).is_none())
        .take(MAX_RECIPES)
        .filter_map(|n| user_recipe(s, n).ok().flatten())
        .collect()
}

/// `over` on top of `base`: keys of `over` replace those of `base`, and two objects are merged one
/// level deep (`{"captions": {"size": 70}}` keeps the other caption defaults).
fn merge(base: &Value, over: &Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    for (k, v) in over.as_object().into_iter().flatten() {
        let merged = match (out.get(k), v) {
            (Some(Value::Object(a)), Value::Object(b)) => {
                let mut m = a.clone();
                m.extend(b.iter().map(|(k, v)| (k.clone(), v.clone())));
                Value::Object(m)
            }
            _ => v.clone(),
        };
        out.insert(k.clone(), merged);
    }
    Value::Object(out)
}

fn list(s: &mut Session, _: &Value) -> Result<Value> {
    let mut recipes: Vec<Value> = BUILTINS
        .iter()
        .map(|b| json!({"name": b.name, "description": b.description, "params": (b.defaults)(), "stages": ["prepare", "finish", "all"]}))
        .collect();
    for u in user_recipes(s) {
        recipes.push(json!({"name": u.name, "base": u.base.name, "description": u.description, "params": merge(&(u.base.defaults)(), &u.params)}));
    }
    Ok(json!({"recipes": recipes, "dir": s.recipe_dir.as_ref().map(|d| d.to_string_lossy().into_owned())}))
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_p(p, "recipe").ok_or_else(|| bad(RUN, "`recipe` is required (workflow.list names them)"))?;
    let stage = match str_p(p, "stage") {
        None | Some("all") => Stage::All,
        Some("prepare") => Stage::Prepare,
        Some("finish") => Stage::Finish,
        Some(o) => return Err(bad(RUN, format!("unknown stage `{o}` (prepare, finish or all)"))),
    };
    let (base, defaults) = match builtin(name) {
        Some(b) => (b, (b.defaults)()),
        None => match user_recipe(s, name)? {
            Some(u) => (u.base, merge(&(u.base.defaults)(), &u.params)),
            None => return Err(bad(RUN, format!("no recipe named `{name}` (workflow.list names them)"))),
        },
    };
    let mut params = merge(&defaults, p);
    if let Some(m) = params.as_object_mut() {
        m.insert("recipe".into(), json!(name));
    }
    (base.run)(s, stage, &params)
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec { id: "workflow.list", label: "List Recipes", menu: &[], shortcut: None, params: "{}", enabled: always, run: list, journal: false },
        CommandSpec {
            id: "workflow.run",
            label: "Run Recipe",
            menu: &[],
            shortcut: None,
            params: r#"{"recipe":str,"stage":"prepare|finish|all"="all","wait":bool=false,...recipe params (workflow.list)}"#,
            enabled: always,
            run,
            journal: true,
        },
    ]
}

// ------------------------------------------------------------------------------ shared steps

/// The short report a recipe answers, with what went wrong but did not stop it.
#[derive(Default)]
struct Report {
    out: Map<String, Value>,
    warnings: Vec<String>,
}

impl Report {
    fn set(&mut self, k: &str, v: Value) {
        self.out.insert(k.to_string(), v);
    }
    fn finish(mut self) -> Value {
        self.out.insert("warnings".into(), json!(self.warnings));
        Value::Object(self.out)
    }
}

/// A number param of the merged recipe params in `lo..=hi`; null or absent is `None`.
fn number(p: &Value, k: &str, lo: f64, hi: f64) -> Result<Option<f64>> {
    match p.get(k).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && (lo..=hi).contains(x))
            .map(Some)
            .ok_or_else(|| bad(RUN, format!("`{k}` must be a number from {lo} to {hi}, got {v}"))),
    }
}

/// A step's options: the object, or None when the step is off (`false`, null or absent).
fn step<'a>(p: &'a Value, k: &str) -> Option<&'a Value> {
    p.get(k).filter(|v| v.is_object())
}

fn on(p: &Value, k: &str) -> bool {
    !matches!(p.get(k), None | Some(Value::Null | Value::Bool(false)))
}

fn has_words(s: &Session) -> bool {
    !crate::transcript::sequence_words(s).is_empty()
}

/// Import `media` (absolute paths) and add `items`; at least one clip is needed.
fn gather_items(s: &mut Session, p: &Value, r: &mut Report) -> Result<Vec<u64>> {
    let list = |k: &str| -> Result<Vec<Value>> {
        match p.get(k) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(a)) if a.len() <= MAX_MEDIA => Ok(a.clone()),
            Some(_) => Err(bad(RUN, format!("`{k}` must be a list of at most {MAX_MEDIA} entries"))),
        }
    };
    let mut items: Vec<u64> =
        list("items")?.iter().map(|v| v.as_u64().ok_or_else(|| bad(RUN, format!("`items` holds item ids, got {v}")))).collect::<Result<_>>()?;
    let paths: Vec<String> = list("media")?
        .iter()
        .map(|v| v.as_str().map(str::to_string).ok_or_else(|| bad(RUN, format!("`media` holds file paths, got {v}"))))
        .collect::<Result<_>>()?;
    if !paths.is_empty() {
        let res = s.execute("file.import", json!({"paths": paths}))?;
        items.extend(res["items"].as_array().into_iter().flatten().filter_map(Value::as_u64));
        r.warnings.extend(res["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|e| format!("import: {e}")));
    }
    if items.is_empty() {
        return Err(bad(RUN, "nothing to edit: pass `media` (file paths) or `items` (project item ids)"));
    }
    Ok(items)
}

/// Every video clip of the active sequence.
fn video_clips(s: &Session) -> Vec<ClipId> {
    s.active_sequence().map(|q| q.video_tracks.iter().flat_map(|t| t.items.iter()).map(|i| i.id).collect()).unwrap_or_default()
}

/// Fill the frame with every video clip, then move each sideways so the point `focus_x` of the
/// source (0 left, 1 right) is as near the frame centre as the frame stays covered.
fn reframe(s: &mut Session, focus_x: f64) -> Result<usize> {
    let clips = video_clips(s);
    if clips.is_empty() {
        return Ok(0);
    }
    s.execute("clip.fillFrame", json!({"clips": clips.iter().map(|c| c.0).collect::<Vec<_>>()}))?;
    if (focus_x - 0.5).abs() < 1e-6 {
        return Ok(clips.len());
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let (fw, fh, par) = (f64::from(q.settings.width), f64::from(q.settings.height), q.settings.par);
    let moves: Vec<(ClipId, f64)> = clips
        .iter()
        .filter_map(|c| {
            let (_, it) = q.find_item(*c)?;
            let (dw, _) = s.project.conformed_source_size(it.item, par)?;
            let scale = it.effect("motion")?.param("scale")?.value.as_f64()? / 100.0;
            let scaled = dw * scale;
            let room = ((scaled - fw) / 2.0).max(0.0);
            let x = (fw / 2.0 - (focus_x - 0.5) * scaled).max(fw / 2.0 - room).min(fw / 2.0 + room);
            x.is_finite().then_some((*c, x))
        })
        .collect();
    s.edit_sequence("Reframe", |q, _, _| {
        for (c, x) in &moves {
            if let Some((_, it)) = q.find_item_mut(*c)
                && let Some(prm) = it.effect_mut("motion").and_then(|m| m.param_mut("position"))
            {
                prm.keyframes.clear();
                prm.value = ParamValue::Vec2(filmcraft_geom::Vec2::new(*x, fh / 2.0));
            }
        }
        Ok(())
    })?;
    Ok(clips.len())
}

/// Transcribe the sequence's media unless it already has a transcript. Without speech-to-text
/// the recipe goes on without the transcript steps and says why.
fn transcribe(s: &mut Session, p: &Value, items: &[u64], r: &mut Report) {
    if has_words(s) || !on(p, "transcribe") {
        return;
    }
    if let Err(why) = crate::transcript::can_transcribe(s) {
        r.warnings.push(format!("no transcript, so no transcript cuts or captions: {why}"));
        return;
    }
    if let Err(e) = s.execute("transcript.generate", json!({"items": items})) {
        r.warnings.push(format!("transcription failed, so no transcript cuts or captions: {e} (a missing model downloads with transcript.downloadModel)"));
    }
}

/// The caption track recipes make (a later run replaces it).
const CAPTION_TRACK: &str = "Recipe Captions";

/// Captions from the transcript, styled with the recipe's `captions` options. Captions an earlier
/// run made are replaced, so running `finish` again does not stack caption tracks.
fn add_captions(s: &mut Session, c: &Value) -> Result<Value> {
    let old: Vec<u64> = s.active_sequence().map(|q| q.caption_tracks.iter().filter(|t| t.name == CAPTION_TRACK).map(|t| t.id.0).collect()).unwrap_or_default();
    for t in old {
        s.execute("captions.deleteTrack", json!({"track": t}))?;
    }
    let mut make = json!({"name": CAPTION_TRACK});
    for k in ["maxChars", "lines", "minSeconds", "maxSeconds", "gapFrames"] {
        if let Some(v) = c.get(k) {
            make[k] = v.clone();
        }
    }
    let made = s.execute("transcript.createCaptions", make)?;
    let mut style = json!({"track": made["track"]});
    for k in ["font", "fontStyle", "size", "color", "background", "backgroundColor", "align", "anchor", "margin", "lineSpacing", "outline", "outlineColor"] {
        if let Some(v) = c.get(k) {
            style[k] = v.clone();
        }
    }
    if style.as_object().is_some_and(|m| m.len() > 1) {
        s.execute("captions.setStyle", style)?;
    }
    Ok(json!({"track": made["track"], "count": made["captions"]}))
}

/// The transcript cuts and captions of `finish`, in order: keep, filler words, pauses, captions.
fn cut_and_caption(s: &mut Session, p: &Value, r: &mut Report) -> Result<()> {
    let mut removed = Map::new();
    if let Some(keep) = step(p, "keep") {
        let k = s.execute("transcript.keep", keep.clone())?;
        removed.insert("keptSeconds".into(), k["seconds"].clone());
    }
    if on(p, "removeFillers") && has_words(s) {
        let fillers = p.get("fillers").filter(|v| v.is_array()).map_or_else(|| json!({}), |f| json!({"fillers": f}));
        removed.insert("fillers".into(), s.execute("transcript.removeFillers", fillers)?["removed"].clone());
    }
    if let Some(o) = step(p, "removePauses")
        && has_words(s)
    {
        removed.insert("pauses".into(), s.execute("transcript.removePauses", o.clone())?["removed"].clone());
    }
    r.set("removed", Value::Object(removed));
    if let Some(c) = step(p, "captions")
        && has_words(s)
    {
        r.set("captions", add_captions(s, c)?);
    }
    Ok(())
}

/// Save the project when `project` names a file; a failure is a warning (the edit is done).
fn save(s: &mut Session, p: &Value, r: &mut Report) {
    let Some(path) = str_p(p, "project") else { return };
    match s.execute("file.saveAs", json!({"path": path})) {
        Ok(_) => r.set("project", json!(path)),
        Err(e) => r.warnings.push(format!("saving {path} failed: {e}")),
    }
}

/// Export when `export.path` is set: H.264 at the sequence size, captions burned in, loudness
/// normalized to `targetLufs`. Without `wait` the export runs as a job, whose `job` and `path` the
/// report carries at the top (agents poll `jobs.list`; MCP streams its progress).
fn export(s: &mut Session, p: &Value, r: &mut Report) {
    let Some(e) = step(p, "export") else { return };
    let Some(path) = str_p(e, "path") else {
        r.warnings.push("`export` needs `path`: nothing was exported".into());
        return;
    };
    let captions = s.active_sequence().is_some_and(|q| q.caption_tracks.iter().any(|t| t.enabled && !t.captions.is_empty()));
    let mut x = json!({"path": path, "format": "h264", "burnCaptions": captions, "wait": bool_p(p, "wait").unwrap_or(false)});
    if let Ok(Some(t)) = number(p, "targetLufs", -70.0, 0.0) {
        x["loudnessLufs"] = json!(t);
    }
    for k in ["format", "crf", "bitrateKbps", "bitrateMode", "preset", "hardwareEncoding", "captionSidecar", "scale", "width", "height", "fps"] {
        if let Some(v) = e.get(k) {
            x[k] = v.clone();
        }
    }
    match s.execute("file.exportMedia", x) {
        Ok(v) => {
            for k in ["job", "path", "poll", "cancel"] {
                if let Some(x) = v.get(k).filter(|_| v.get("result").is_none()) {
                    r.set(k, x.clone());
                }
            }
            r.set("export", v);
        }
        Err(e) => r.warnings.push(format!("export failed: {e}")),
    }
}

/// The open sequence in a few numbers.
fn describe(s: &Session, r: &mut Report) {
    if let Some(q) = s.active_sequence() {
        r.set("sequence", json!(s.state.active_sequence.map(|i| i.0)));
        r.set("seconds", json!(secs_ms(q.duration())));
        r.set("clips", json!(q.video_tracks.iter().chain(&q.audio_tracks).map(|t| t.items.len()).sum::<usize>()));
    }
}

// --------------------------------------------------------------------------- vertical-short

fn vertical_short_defaults() -> Value {
    json!({
        "media": [],
        "items": [],
        "name": "Short",
        "fps": null,
        "focusX": 0.5,
        "transcribe": true,
        "keep": null,
        "removeFillers": true,
        "removePauses": {"minSeconds": 0.4, "keepSeconds": 0.1},
        "captions": {"maxChars": 14, "lines": 1, "size": 58, "fontStyle": "Bold", "color": "#ffffff", "background": false, "outline": 4, "outlineColor": "#000000", "anchor": "bottom", "margin": 0.22},
        "targetLufs": -14,
        "maxSeconds": 60,
        "project": null,
        "export": null,
    })
}

/// `prepare` of vertical-short: import, a 1080x1920 sequence of the clips back to back, fill-frame
/// reframe at `focusX`, transcript.
fn vertical_prepare(s: &mut Session, p: &Value, r: &mut Report) -> Result<()> {
    let focus_x = number(p, "focusX", 0.0, 1.0)?.unwrap_or(0.5);
    let items = gather_items(s, p, r)?;
    let mut sq = json!({"name": str_p(p, "name").unwrap_or("Short"), "fromItems": items, "width": 1080, "height": 1920});
    if let Some(fps) = p.get("fps").filter(|v| !v.is_null()) {
        sq["fps"] = fps.clone();
    }
    s.execute("file.newSequence", sq)?;
    r.set("reframed", json!(reframe(s, focus_x)?));
    let media: Vec<u64> = items.iter().filter_map(|i| s.project.resolve_media(ItemId(*i)).map(|(m, _, _)| m.0)).collect();
    transcribe(s, p, &media, r);
    Ok(())
}

fn vertical_short(s: &mut Session, stage: Stage, p: &Value) -> Result<Value> {
    // checked before the first change
    let max = number(p, "maxSeconds", 0.001, 86_400.0)?;
    number(p, "targetLufs", -70.0, 0.0)?;
    number(p, "focusX", 0.0, 1.0)?;
    if stage == Stage::Finish && s.active_sequence().is_none() {
        return Err(bad(RUN, "no sequence is open: run stage `prepare` first"));
    }
    let mut r = Report::default();
    r.set("recipe", p["recipe"].clone());
    s.edit_group("Recipe: vertical short", |s| {
        if stage != Stage::Finish {
            vertical_prepare(s, p, &mut r)?;
        }
        if stage != Stage::Prepare {
            cut_and_caption(s, p, &mut r)?;
        }
        Ok(())
    })?;
    describe(s, &mut r);
    if stage == Stage::Prepare {
        let t = s.execute("transcript.inspect", json!({"words": false}))?;
        r.set("transcript", json!({"words": t["words"], "paragraphs": t["paragraphs"]}));
        return Ok(r.finish());
    }
    let mut qa = json!({});
    if let Some(m) = max {
        qa["maxSeconds"] = json!(m);
    }
    match s.execute("sequence.qa", qa) {
        Ok(v) => r.set("qa", v),
        Err(e) => r.warnings.push(format!("quality check failed: {e}")),
    }
    save(s, p, &mut r);
    export(s, p, &mut r);
    Ok(r.finish())
}
