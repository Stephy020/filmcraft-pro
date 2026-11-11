//! `sequence.qa`: a text report of what would spoil a finished cut (gaps with no picture, flash
//! frames, offline media, loudness and peaks, long silences, black pictures, caption problems,
//! length), so an agent can check an edit without looking at pictures.

use std::collections::HashMap;

use serde_json::{Value, json};

use epsicut_audio_dsp::LoudnessMeter;
use epsicut_project::{ItemId, Sequence};
use epsicut_time::{Tick, TimeRange};

use crate::commands::{CommandSpec, bad, bool_p, has_seq, secs_ms};
use crate::{EngineError, Result, Session};

const CMD: &str = "sequence.qa";
/// Issues listed in full; the rest are only counted.
const MAX_ISSUES: usize = 50;
/// Black-frame samples at most (the step grows on long sequences).
const MAX_SAMPLES: i64 = 600;
/// Audio analysed at most: the first 3 hours.
const MAX_AUDIO_SECONDS: i64 = 3 * 3600;
/// A true peak above this may clip after delivery encoding (dBTP).
const PEAK_LIMIT_DBTP: f64 = -1.0;
/// A stretch quieter than this for at least [`SILENCE_SECONDS`] is reported.
const SILENCE_DBFS: f64 = -50.0;
const SILENCE_SECONDS: f64 = 2.0;
/// Mean picture level (8-bit sRGB luma) below which a frame counts as black.
const BLACK_LUMA: f64 = 8.0;
/// Captions read faster than this (characters per second) are hard to follow.
const MAX_CPS: f64 = 20.0;

struct Issue {
    kind: &'static str,
    at: Option<Tick>,
    len: Option<Tick>,
    detail: String,
}

impl Issue {
    fn new(kind: &'static str, at: Tick, len: Option<Tick>, detail: String) -> Self {
        Self { kind, at: Some(at), len, detail }
    }
    fn json(&self) -> Value {
        let mut v = json!({"kind": self.kind, "detail": self.detail});
        if let Some(t) = self.at {
            v["at"] = json!(secs_ms(t));
        }
        if let Some(l) = self.len {
            v["seconds"] = json!(secs_ms(l));
        }
        v
    }
}

struct Opts {
    target_lufs: Option<f64>,
    max_seconds: Option<f64>,
    min_clip_frames: i64,
    audio: bool,
    video: bool,
    sample: Tick,
}

/// A number param in `lo..=hi` (absent or null: `None`); anything else is an error.
fn number(p: &Value, k: &str, lo: f64, hi: f64) -> Result<Option<f64>> {
    match p.get(k).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && (lo..=hi).contains(x))
            .map(Some)
            .ok_or_else(|| bad(CMD, format!("`{k}` must be a number from {lo} to {hi}, got {v}"))),
    }
}

fn opts(p: &Value) -> Result<Opts> {
    Ok(Opts {
        target_lufs: number(p, "targetLufs", -70.0, 0.0)?,
        max_seconds: number(p, "maxSeconds", 0.001, 86_400.0)?,
        min_clip_frames: number(p, "minClipFrames", 0.0, 100.0)?.map_or(3, |v| v as i64),
        audio: bool_p(p, "audio").unwrap_or(true),
        video: bool_p(p, "video").unwrap_or(true),
        sample: Tick::from_seconds_f64(number(p, "sampleSeconds", 0.04, 60.0)?.unwrap_or(1.0)),
    })
}

/// Where each media item (and the media behind a subclip) is first used in the sequence.
fn first_uses(s: &Session, q: &Sequence) -> HashMap<ItemId, Tick> {
    let mut uses: HashMap<ItemId, Tick> = HashMap::new();
    for i in q.video_tracks.iter().chain(&q.audio_tracks).flat_map(|t| t.items.iter()) {
        let root = s.project.resolve_media(i.item).map(|(r, _, _)| r);
        for id in std::iter::once(i.item).chain(root) {
            let at = uses.entry(id).or_insert(i.start);
            *at = (*at).min(i.start);
        }
    }
    uses
}

/// Media the sequence uses that is offline, missing or unreadable (it renders as a placeholder).
fn offline(s: &Session, q: &Sequence, out: &mut Vec<Issue>) {
    let uses = first_uses(s, q);
    let mut used: Vec<(ItemId, Tick)> = uses.into_iter().collect();
    used.sort_by_key(|(id, at)| (*at, id.0));
    for (id, at) in used {
        let Some(st) = crate::relink::status_json(s, id) else { continue };
        let status = st["status"].as_str().unwrap_or("online");
        if !matches!(status, "online" | "generated") {
            out.push(Issue::new("offline", at, None, format!("{} is {status}", st["name"].as_str().unwrap_or("media"))));
        }
    }
}

/// The time an enabled clip on a shown video track covers.
fn picture(q: &Sequence) -> Vec<TimeRange> {
    q.video_tracks.iter().filter(|t| t.enabled).flat_map(|t| t.items.iter()).filter(|i| i.enabled).map(|i| TimeRange::new(i.start, i.duration)).collect()
}

/// Stretches with no clip on any shown video track: black on export.
fn picture_gaps(q: &Sequence, out: &mut Vec<Issue>) {
    let covered = picture(q);
    if covered.is_empty() {
        return;
    }
    let half_frame = Tick(q.settings.frame_rate.sane().frame_duration().0 / 2);
    for g in crate::transcript::complement(covered, q.duration()) {
        if g.duration > half_frame {
            out.push(Issue::new("gap", g.start, Some(g.duration), "no picture".into()));
        }
    }
}

/// Clips shorter than `min_frames` frames (flash frames, slivers left by cuts).
fn short_clips(q: &Sequence, min_frames: i64, out: &mut Vec<Issue>) {
    if min_frames <= 0 {
        return;
    }
    let rate = q.settings.frame_rate.sane();
    let min = rate.frame_duration().mul_ratio(min_frames, 1);
    for t in q.video_tracks.iter().chain(&q.audio_tracks).filter(|t| t.enabled) {
        for i in t.items.iter().filter(|i| i.enabled && i.duration < min) {
            let frames = (i.duration.seconds() * rate.as_f64()).round().max(0.0);
            out.push(Issue::new("flash", i.start, Some(i.duration), format!("{} on {} is {frames} frame(s) long", i.name, t.name)));
        }
    }
}

/// Overlapping captions, captions past the end, and captions too fast to read.
fn captions(q: &Sequence, out: &mut Vec<Issue>) {
    let end = q.duration();
    for t in q.caption_tracks.iter().filter(|t| t.enabled) {
        let mut cs: Vec<_> = t.captions.iter().collect();
        cs.sort_by_key(|c| c.start);
        for (n, c) in cs.iter().enumerate() {
            let c_end = c.start + c.duration;
            if let Some(next) = cs.get(n + 1)
                && next.start < c_end
            {
                out.push(Issue::new("caption", next.start, Some(c_end - next.start), format!("captions overlap on {}", t.name)));
            }
            if c_end > end {
                out.push(Issue::new("caption", c.start, None, format!("a caption on {} runs past the end", t.name)));
            }
            let secs = c.duration.seconds();
            let chars = c.text.chars().filter(|ch| *ch != '\n').count();
            if secs > 0.0 && chars as f64 / secs > MAX_CPS {
                out.push(Issue::new("caption", c.start, Some(c.duration), format!("{:.0} characters per second is hard to read", chars as f64 / secs)));
            }
        }
    }
}

fn silence(from: i64, to: i64, sr: i64, out: &mut Vec<Issue>) {
    let (a, b) = (Tick::from_units(from, sr), Tick::from_units(to, sr));
    if (b - a).seconds() >= SILENCE_SECONDS {
        out.push(Issue::new("silence", a, Some(b - a), format!("quieter than {SILENCE_DBFS} dBFS")));
    }
}

/// Integrated loudness and true peak of the sequence mix, and long silences.
fn audio(s: &Session, q: &Sequence, target: Option<f64>, out: &mut Vec<Issue>) -> Value {
    let sr = i64::from(q.settings.sample_rate.clamp(8_000, 192_000));
    let total = q.duration().to_units_floor(sr).clamp(0, MAX_AUDIO_SECONDS * sr);
    if total == 0 {
        return Value::Null;
    }
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    let mut meter = LoudnessMeter::new(sr as f64, 2);
    let win = usize::try_from(sr / 10).unwrap_or(4_800).max(1);
    let quiet = 10f64.powf(SILENCE_DBFS / 20.0) as f32;
    let mut silent_from: Option<i64> = None;
    let mut pos = 0i64;
    while pos < total {
        let n = (total - pos).min(sr);
        let buf = epsicut_render::audio::mix_sequence(&s.project, q, pos, usize::try_from(n).unwrap_or(0), &provider);
        let (Some(l), Some(r)) = (buf.channels.first(), buf.channels.get(1)) else { break };
        meter.process(&[l.as_slice(), r.as_slice()]);
        for (k, (wl, wr)) in l.chunks(win).zip(r.chunks(win)).enumerate() {
            let e = wl.iter().chain(wr).map(|x| x * x).sum::<f32>() / (wl.len() + wr.len()).max(1) as f32;
            let at = pos.saturating_add(i64::try_from(k.saturating_mul(win)).unwrap_or(i64::MAX));
            if e.sqrt() < quiet {
                silent_from = silent_from.or(Some(at));
            } else if let Some(a) = silent_from.take() {
                silence(a, at, sr, out);
            }
        }
        pos += n;
    }
    if let Some(a) = silent_from {
        silence(a, pos, sr, out);
    }
    let (lufs, peak) = (meter.integrated(), meter.true_peak_dbtp());
    if peak.is_finite() && peak > PEAK_LIMIT_DBTP {
        out.push(Issue { kind: "peak", at: None, len: None, detail: format!("true peak {peak:.1} dBTP is above {PEAK_LIMIT_DBTP} dBTP") });
    }
    if let Some(t) = target
        && lufs.is_finite()
        && (lufs - t).abs() > 1.0
    {
        out.push(Issue { kind: "loudness", at: None, len: None, detail: format!("{lufs:.1} LUFS, target {t} LUFS") });
    }
    let round1 = |v: f64| v.is_finite().then(|| (v * 10.0).round() / 10.0);
    json!({"integratedLufs": round1(lufs), "truePeakDbtp": round1(peak), "seconds": secs_ms(Tick::from_units(pos, sr))})
}

/// Mean 8-bit sRGB luma of a rendered frame.
fn mean_luma(img: &epsicut_render::Image) -> f64 {
    let px = img.over_black_rgba8();
    let pixels = px.as_chunks::<4>().0;
    let sum: f64 = pixels.iter().map(|[r, g, b, _]| 0.2126 * f64::from(*r) + 0.7152 * f64::from(*g) + 0.0722 * f64::from(*b)).sum();
    if pixels.is_empty() { 0.0 } else { sum / pixels.len() as f64 }
}

/// Black pictures where a clip is shown (a gap is reported as a gap), sampled every `step`;
/// a run of black samples is one issue.
fn black(s: &Session, q: &Sequence, step: Tick, out: &mut Vec<Issue>) {
    let total = q.duration();
    let shown = picture(q);
    if total <= Tick::ZERO || shown.is_empty() {
        return;
    }
    let step = Tick(step.0.max(total.0 / MAX_SAMPLES).max(1));
    let scale = (32.0 / q.settings.width.max(q.settings.height).max(1) as f32).min(1.0);
    let mut run: Option<Tick> = None;
    let mut t = Tick::ZERO;
    while t < total {
        let dark = shown.iter().any(|r| r.contains(t)) && s.try_render_program_at(scale, t).is_ok_and(|img| mean_luma(&img) < BLACK_LUMA);
        match (dark, run) {
            (true, None) => run = Some(t),
            (false, Some(a)) => {
                out.push(Issue::new("black", a, Some(t - a), "black picture".into()));
                run = None;
            }
            _ => {}
        }
        t += step;
    }
    if let Some(a) = run {
        out.push(Issue::new("black", a, Some(total - a), "black picture".into()));
    }
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let o = opts(p)?;
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut issues = Vec::new();
    offline(s, q, &mut issues);
    picture_gaps(q, &mut issues);
    short_clips(q, o.min_clip_frames, &mut issues);
    captions(q, &mut issues);
    if let Some(max) = o.max_seconds
        && q.duration().seconds() > max
    {
        issues.push(Issue { kind: "length", at: None, len: Some(q.duration()), detail: format!("longer than {max} s") });
    }
    let loudness = if o.audio { audio(s, q, o.target_lufs, &mut issues) } else { Value::Null };
    if o.video {
        black(s, q, o.sample, &mut issues);
    }
    issues.sort_by_key(|i| i.at.unwrap_or(Tick::MIN));
    let more = issues.len().saturating_sub(MAX_ISSUES);
    Ok(json!({
        "ok": issues.is_empty(),
        "seconds": secs_ms(q.duration()),
        "loudness": loudness,
        "issues": issues.iter().take(MAX_ISSUES).map(Issue::json).collect::<Vec<_>>(),
        "more": more,
    }))
}

pub fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "sequence.qa",
        label: "Check Sequence",
        menu: &[],
        shortcut: None,
        params: r#"{"targetLufs":-70..0?,"maxSeconds":f?,"minClipFrames":0..100=3,"audio":bool=true,"video":bool=true,"sampleSeconds":0.04..60=1}"#,
        enabled: has_seq,
        run,
        journal: false,
    }]
}
