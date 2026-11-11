//! Recipes (`workflow.*`): one call runs a whole edit as one undo step.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::Session;
use crate::media_test_util::{make_movie, tmp_dir};
use epsicut_media::DemoScene;
use epsicut_project::{Transcript, Word};
use epsicut_speech::FixedTranscriber;
use epsicut_time::Tick;

/// A 2 s 320x180 movie with sound.
fn movie(name: &str) -> PathBuf {
    let p = tmp_dir(name).join("talk.mov");
    make_movie(&p, DemoScene::Aurora, 320, 180, 48);
    p
}

/// "Hello um world." (Speaker 1), a 0.5 s pause, "Bye" (Speaker 2), in media time.
fn talk() -> Transcript {
    let mut t = Transcript { language: "en".into(), ..Default::default() };
    for (text, a, b, sp) in [("Hello", 0.1, 0.4, 0), ("um", 0.5, 0.7, 0), ("world.", 0.8, 1.1, 0), ("Bye", 1.6, 1.9, 1)] {
        let mut w = Word::new(text, Tick::from_seconds_f64(a), Tick::from_seconds_f64(b));
        w.speaker = Some(sp);
        t.words.push(w);
    }
    t.normalize();
    t
}

fn session() -> Session {
    Session { transcriber: Some(Arc::new(FixedTranscriber { transcript: talk(), id: "fixed".into() })), ..Default::default() }
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn words(s: &Session) -> Vec<String> {
    crate::transcript::sequence_words(s).into_iter().map(|w| w.text).collect()
}

fn undo_steps(s: &mut Session) -> usize {
    s.execute("history.list", json!({})).unwrap()["undo"].as_array().unwrap().len()
}

/// One call: a 1080x1920 sequence, the clip filling the frame with the source point `focusX`
/// centred, filler words and pauses cut, captions styled, a quality check; one undo step back.
#[test]
fn vertical_short_in_one_call_is_one_undo_step() {
    let a = movie("recipe-all");
    let mut s = session();
    let (steps, sequences) = (undo_steps(&mut s), s.project.sequences().count());
    let r = s.execute("workflow.run", json!({"recipe": "vertical-short", "media": [path(&a)], "focusX": 0.3})).unwrap();
    assert_eq!(r["warnings"], json!([]), "{r}");
    assert_eq!((r["reframed"].as_u64(), r["removed"]["fillers"].as_u64()), (Some(1), Some(1)), "{r}");
    assert!(r["removed"]["pauses"].as_u64().unwrap() >= 1, "{r}");
    assert!(r["captions"]["count"].as_u64().unwrap() >= 2, "{r}");
    assert!(r["seconds"].as_f64().unwrap() < 2.0, "{r}");
    assert_eq!(r["qa"]["ok"], true, "a clean cut passes its own check: {r}");
    assert_eq!(words(&s), ["Hello", "world.", "Bye"]);

    let q = s.active_sequence().unwrap();
    assert_eq!((q.settings.width, q.settings.height), (1080, 1920));
    let m = q.video_tracks[0].items[0].effect("motion").unwrap();
    let scale = m.param("scale").unwrap().value.as_f64().unwrap();
    assert!((scale - 1920.0 / 180.0 * 100.0).abs() < 0.5, "fills the height: {scale}");
    let x = m.param("position").unwrap().value.as_vec2().unwrap().x;
    let expected = 540.0 + 0.2 * 320.0 * scale / 100.0;
    assert!((x - expected).abs() < 1.0, "source point 0.3 centred: x {x}, expected {expected}");
    let style = &q.caption_tracks[0].style;
    assert_eq!((style.size, style.outline, style.background), (58.0, 4.0, false));

    assert_eq!(undo_steps(&mut s), steps + 1, "one undo step");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.sequences().count(), sequences, "undo takes the whole recipe back");
}

/// `prepare` reports the transcript as paragraphs; `finish` keeps the chosen ones and cuts the
/// rest; `false` turns a step off.
#[test]
fn prepare_then_finish_keeps_the_chosen_paragraphs() {
    let a = movie("recipe-stages");
    let mut s = session();
    let r = s.execute("workflow.run", json!({"recipe": "vertical-short", "media": [path(&a)], "stage": "prepare"})).unwrap();
    let paras = r["transcript"]["paragraphs"].as_array().unwrap();
    assert_eq!(paras.len(), 2, "{r}");
    assert_eq!(paras[0][6], "Hello um world.");
    assert!(r.get("removed").is_none() && r.get("qa").is_none(), "prepare only prepares: {r}");
    assert_eq!(words(&s).len(), 4);

    let r = s.execute("workflow.run", json!({"recipe": "vertical-short", "stage": "finish", "keep": {"paragraphs": [0]}, "captions": false})).unwrap();
    assert_eq!(words(&s), ["Hello", "world."], "{r}");
    assert!(r.get("captions").is_none() && s.active_sequence().unwrap().caption_tracks.is_empty(), "{r}");
    assert!(r["qa"].is_object(), "{r}");
}

/// `export.path` exports the short (captions burned in); with `wait` the file is written when the
/// call returns, without it the report names the job and file at the top (as MCP follows them).
#[test]
fn exports_the_finished_short() {
    let a = movie("recipe-export");
    let out = tmp_dir("recipe-out").join("short.mp4");
    let mut s = session();
    let r = s
        .execute("workflow.run", json!({"recipe": "vertical-short", "media": [path(&a)], "export": {"path": path(&out), "scale": 0.1}, "wait": true}))
        .unwrap();
    assert!(out.exists(), "{r}");
    assert!(r["export"]["result"].is_object() && r.get("job").is_none(), "a written export is not a job to follow: {r}");

    let later = out.with_file_name("later.mp4");
    let r = s
        .execute(
            "workflow.run",
            json!({"recipe": "vertical-short", "stage": "finish", "removeFillers": false, "removePauses": false, "captions": false, "export": {"path": path(&later), "scale": 0.1}}),
        )
        .unwrap();
    assert!(r["job"].is_u64() && r["path"].is_string() && r["poll"] == "jobs.list", "{r}");
    for _ in 0..1200 {
        if s.execute("jobs.list", json!({})).unwrap().as_array().unwrap().iter().all(|j| j["finished"] == true) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(later.exists());

    let r = s.execute("workflow.run", json!({"recipe": "vertical-short", "stage": "finish", "export": {"crf": 20}})).unwrap();
    assert!(r["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("needs `path`")), "{r}");
}

/// A user recipe file changes a built-in recipe's defaults; broken, huge or path-like recipes are
/// refused.
#[test]
fn user_recipes_override_defaults() {
    let dir = tmp_dir("recipes");
    std::fs::write(dir.join("my-shorts.json"), r#"{"base":"vertical-short","description":"mine","params":{"captions":{"size":70},"removeFillers":false}}"#)
        .unwrap();
    std::fs::write(dir.join("broken.json"), "{not json").unwrap();
    std::fs::write(dir.join("huge.json"), vec![b' '; 70_000]).unwrap();
    std::fs::write(dir.join("loop.json"), r#"{"base":"my-shorts"}"#).unwrap();
    let mut s = session();
    s.recipe_dir = Some(dir.clone());
    let l = s.execute("workflow.list", json!({})).unwrap();
    let names: Vec<&str> = l["recipes"].as_array().unwrap().iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, ["vertical-short", "my-shorts"], "{l}");
    let mine = &l["recipes"][1];
    assert_eq!((mine["base"].as_str(), mine["description"].as_str()), (Some("vertical-short"), Some("mine")));
    assert_eq!((&mine["params"]["captions"]["size"], &mine["params"]["captions"]["maxChars"]), (&json!(70), &json!(14)), "merged one level deep");

    let a = movie("recipe-user");
    let r = s.execute("workflow.run", json!({"recipe": "my-shorts", "media": [path(&a)]})).unwrap();
    assert!(r["removed"].get("fillers").is_none(), "{r}");
    assert_eq!(s.active_sequence().unwrap().caption_tracks[0].style.size, 70.0);
    // a call param overrides the file
    s.execute("workflow.run", json!({"recipe": "my-shorts", "media": [path(&a)], "captions": {"size": 64}})).unwrap();
    assert_eq!(s.active_sequence().unwrap().caption_tracks[0].style.size, 64.0);

    for (name, why) in [("broken", "broken.json"), ("huge", "bytes"), ("loop", "unknown base"), ("../huge", "no recipe"), ("nope", "no recipe")] {
        let e = s.execute("workflow.run", json!({"recipe": name, "media": [path(&a)]})).unwrap_err().to_string();
        assert!(e.contains(why), "{name}: {e}");
    }
}

/// Without a transcript the recipe still edits; turning transcription off is not a warning.
#[test]
fn no_transcript_still_edits() {
    let a = movie("recipe-nospeech");
    let mut s = session();
    let r = s.execute("workflow.run", json!({"recipe": "vertical-short", "media": [path(&a)], "transcribe": false})).unwrap();
    assert_eq!(r["warnings"], json!([]), "{r}");
    assert!(r.get("captions").is_none() && s.active_sequence().unwrap().caption_tracks.is_empty());
    assert_eq!(r["reframed"], 1);

    s.transcriber = None;
    if !epsicut_speech::available() {
        let r = s.execute("workflow.run", json!({"recipe": "vertical-short", "media": [path(&a)]})).unwrap();
        assert!(r["warnings"][0].as_str().unwrap().contains("no transcript"), "{r}");
        assert_eq!(r["reframed"], 1);
    }
}

/// Bad params are errors that leave the project and its history as they were.
#[test]
fn bad_params_change_nothing() {
    let a = path(&movie("recipe-bad"));
    let mut s = session();
    let (items, steps) = (s.project.items.len(), undo_steps(&mut s));
    for bad in [
        json!({}),
        json!({"recipe": "nope"}),
        json!({"recipe": "vertical-short"}),
        json!({"recipe": "vertical-short", "media": [a], "stage": "later"}),
        json!({"recipe": "vertical-short", "media": [a], "focusX": 2}),
        json!({"recipe": "vertical-short", "media": [a], "targetLufs": 10}),
        json!({"recipe": "vertical-short", "media": [a], "maxSeconds": -1}),
        json!({"recipe": "vertical-short", "media": a}),
        json!({"recipe": "vertical-short", "items": [-1]}),
        json!({"recipe": "vertical-short", "stage": "finish"}),
        json!({"recipe": "vertical-short", "media": ["/no/such/file.mov"]}),
        json!({"recipe": "vertical-short", "media": [a], "keep": {"paragraphs": [99]}}),
        json!({"recipe": "vertical-short", "media": [a], "fps": -5}),
        json!({"recipe": "vertical-short", "media": vec![Value::from("x"); 501]}),
    ] {
        assert!(s.execute("workflow.run", bad.clone()).is_err(), "{bad}");
        assert_eq!((s.project.items.len(), undo_steps(&mut s)), (items, steps), "{bad} changed nothing");
    }
}
