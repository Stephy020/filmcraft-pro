//! `sequence.qa`: the text report agents check an edit with.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::Session;
use crate::media_test_util::{make_movie, session_with, tmp_dir};
use epsicut_media::DemoScene;
use epsicut_project::ClipId;
use epsicut_time::Tick;

fn kinds(r: &Value) -> Vec<String> {
    r["issues"].as_array().unwrap().iter().map(|i| i["kind"].as_str().unwrap().to_string()).collect()
}

fn issue<'a>(r: &'a Value, kind: &str) -> &'a Value {
    r["issues"].as_array().unwrap().iter().find(|i| i["kind"] == kind).unwrap_or_else(|| panic!("no {kind} issue in {r}"))
}

/// A gap with no picture, a one-frame clip, overlapping and over-fast captions, a missing file
/// and an over-long sequence are each reported, in time order.
#[test]
fn reports_gaps_flash_frames_captions_offline_media_and_length() {
    let dir = tmp_dir("qa");
    let (a, b) = (dir.join("a.mov"), dir.join("b.mov"));
    make_movie(&a, DemoScene::Aurora, 160, 90, 36);
    make_movie(&b, DemoScene::Aurora, 160, 90, 36);
    let (mut s, _, seq) = session_with(&[&a, &b]);
    let clean = s.execute("sequence.qa", json!({"video": false})).unwrap();
    assert_eq!(clean["ok"], true, "{clean}");

    std::fs::remove_file(&b).unwrap();
    s.execute("captions.add", json!({"seconds": 0, "durationSeconds": 1, "text": "Hello"})).unwrap();
    s.execute("captions.add", json!({"seconds": 2, "durationSeconds": 0.5, "text": "far too many characters to read in half a second"})).unwrap();
    let rate = s.sequence_rate();
    let mut p = (*s.project).clone();
    let id = ClipId(p.alloc_id());
    let q = p.sequence_mut(seq).unwrap();
    q.video_tracks[0].items[1].start += Tick::from_seconds_f64(1.0);
    let mut flash = q.video_tracks[0].items[0].clone();
    flash.id = id;
    flash.link = None;
    flash.start = Tick::from_seconds_f64(5.0);
    flash.duration = rate.frame_duration();
    q.video_tracks[0].items.push(flash);
    q.caption_tracks[0].captions[1].start = Tick::from_seconds_f64(0.5);
    s.project = Arc::new(p);

    let r = s.execute("sequence.qa", json!({"maxSeconds": 3, "video": false})).unwrap();
    assert_eq!(r["ok"], false, "{r}");
    for k in ["gap", "flash", "caption", "offline", "length"] {
        assert!(kinds(&r).iter().any(|x| x == k), "{k} missing from {r}");
    }
    let gap = issue(&r, "gap");
    assert!((gap["at"].as_f64().unwrap() - 1.5).abs() < 0.01 && (gap["seconds"].as_f64().unwrap() - 1.0).abs() < 0.01, "{gap}");
    assert!(issue(&r, "offline")["detail"].as_str().unwrap().contains("missing"), "{r}");
    assert!(issue(&r, "flash")["detail"].as_str().unwrap().contains("1 frame"), "{r}");
    let captions: Vec<&str> = r["issues"].as_array().unwrap().iter().filter(|i| i["kind"] == "caption").filter_map(|i| i["detail"].as_str()).collect();
    assert!(captions.iter().any(|d| d.contains("overlap")) && captions.iter().any(|d| d.contains("per second")), "{captions:?}");
    let ats: Vec<f64> = r["issues"].as_array().unwrap().iter().filter_map(|i| i["at"].as_f64()).collect();
    assert!(ats.windows(2).all(|w| w[0] <= w[1]), "in time order: {ats:?}");
    assert_eq!(r["more"], 0);
}

/// Black pictures are found by sampling frames; the mix is measured (integrated loudness, true peak).
#[test]
fn finds_black_pictures_and_measures_the_mix() {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"width": 160, "height": 90, "fps": 24})).unwrap();
    let item = s.execute("file.newBlackVideo", json!({"seconds": 3})).unwrap()["item"].as_u64().unwrap();
    s.execute("timeline.place", json!({"item": item, "seconds": 0})).unwrap();
    let r = s.execute("sequence.qa", json!({"audio": false})).unwrap();
    let black = issue(&r, "black");
    assert!(black["at"].as_f64().unwrap() < 0.01 && black["seconds"].as_f64().unwrap() > 2.0, "{r}");
    assert!(r["loudness"].is_null(), "audio was not asked for: {r}");

    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let r = s.execute("sequence.qa", json!({"video": false})).unwrap();
    assert!(r["loudness"].is_object() && r["seconds"].as_f64().unwrap() > 0.0, "{r}");
    assert!(r["issues"].is_array() && r["more"].is_u64(), "{r}");
}

/// Out-of-range options are errors; without a sequence the command is disabled.
#[test]
fn rejects_hostile_options() {
    let mut s = Session::default();
    assert!(s.execute("sequence.qa", json!({})).is_err(), "no sequence");
    s.execute("file.openDemoProject", json!({})).unwrap();
    for bad in [
        json!({"targetLufs": 5}),
        json!({"targetLufs": -1000}),
        json!({"targetLufs": "loud"}),
        json!({"maxSeconds": 0}),
        json!({"minClipFrames": 1000}),
        json!({"sampleSeconds": 0}),
        json!({"sampleSeconds": 1e9}),
    ] {
        let e = s.execute("sequence.qa", bad.clone()).unwrap_err().to_string();
        assert!(e.contains("must be a number"), "{bad}: {e}");
    }
}
