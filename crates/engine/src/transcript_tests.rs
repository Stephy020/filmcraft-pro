use std::sync::Arc;

use serde_json::json;

use crate::Session;
use epsicut_project::{ItemId, Transcript, Word};
use epsicut_speech::FixedTranscriber;
use epsicut_time::Tick;

/// The demo project with a fake transcriber whose transcript fits the first A1 clip's media:
/// "Hello um world." (Speaker 1), a 1.2 s pause, "Second speaker here." (Speaker 2).
fn session() -> (Session, ItemId, Tick) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    let a = &q.audio_tracks[0].items[0];
    let (item, sin) = (a.item, a.source_in);
    let sec = |x: f64| sin + Tick::from_seconds_f64(x);
    let mut t = Transcript { language: "en".into(), ..Default::default() };
    for (text, a, b, sp) in
        [("Hello", 0.2, 0.5, 0), ("um", 0.6, 0.9, 0), ("world.", 1.0, 1.4, 0), ("Second", 2.6, 3.0, 1), ("speaker", 3.0, 3.5, 1), ("here.", 3.5, 4.0, 1)]
    {
        let mut w = Word::new(text, sec(a), sec(b));
        w.speaker = Some(sp);
        t.words.push(w);
    }
    t.normalize();
    s.transcriber = Some(Arc::new(FixedTranscriber { transcript: t, id: "fixed".into() }));
    let start = a_start(&s);
    (s, item, start)
}

fn a_start(s: &Session) -> Tick {
    s.active_sequence().unwrap().audio_tracks[0].items[0].start
}

fn words(s: &mut Session) -> Vec<String> {
    let r = s.execute("transcript.inspect", json!({})).unwrap();
    r["words"].as_array().unwrap().iter().map(|w| w["text"].as_str().unwrap().to_string()).collect()
}

#[test]
fn generate_inspect_search_and_rename() {
    let (mut s, item, start) = session();
    assert!(s.execute("transcript.select", json!({"from": 0})).is_err(), "disabled before transcribing");
    let r = s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    assert_eq!(r["items"][0]["words"], 6, "{r}");
    assert_eq!(r["items"][0]["source"], "fixed");
    assert_eq!(s.project.transcripts[&item].words.len(), 6);

    let r = s.execute("transcript.inspect", json!({})).unwrap();
    assert_eq!(words(&mut s), ["Hello", "um", "world.", "Second", "speaker", "here."]);
    assert_eq!(r["words"][0]["start"].as_i64().unwrap(), (start + Tick::from_seconds_f64(0.2)).0, "mapped to sequence time");
    assert_eq!(r["paragraphs"].as_array().unwrap().len(), 2, "speaker change splits paragraphs: {}", r["paragraphs"]);
    assert_eq!(r["speakers"], json!(["Speaker 1", "Speaker 2"]));

    let r = s.execute("transcript.search", json!({"query": "second spea"})).unwrap();
    assert_eq!(r["matches"], json!([{"from": 3, "to": 4, "start": r["matches"][0]["start"], "end": r["matches"][0]["end"]}]));

    s.execute("transcript.renameSpeaker", json!({"speaker": "Speaker 2", "name": "Ann"})).unwrap();
    let r = s.execute("transcript.inspect", json!({})).unwrap();
    assert_eq!(r["words"][3]["speaker"], "Ann");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.transcripts[&item].speakers[1].name, "Speaker 2");
    s.execute("transcript.renameSpeaker", json!({"speaker": 0, "item": item.0, "name": "Bo"})).unwrap();
    assert_eq!(s.project.transcripts[&item].speakers[0].name, "Bo");
    assert!(s.execute("transcript.renameSpeaker", json!({"speaker": "Nobody", "name": "X"})).is_err());

    // transcribing is one undo step
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.transcripts.is_empty());
}

/// `transcript.inspect {words:false}`: paragraph rows without the word list, so an agent reads a
/// talk in a few tokens; the default reply is unchanged.
#[test]
fn compact_inspect_lists_paragraph_rows() {
    let (mut s, item, start) = session();
    s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    let full = s.execute("transcript.inspect", json!({})).unwrap();
    assert_eq!(full["words"].as_array().map(Vec::len), Some(6));
    let r = s.execute("transcript.inspect", json!({"words": false})).unwrap();
    assert_eq!(r["words"], 6, "{r}");
    let rows = r["paragraphs"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{r}");
    assert_eq!((rows[0][0].as_u64(), rows[0][1].as_u64(), rows[0][2].as_u64()), (Some(0), Some(0), Some(2)), "{r}");
    assert_eq!(rows[0][3], "Speaker 1");
    assert_eq!(rows[0][6], "Hello um world.");
    assert_eq!((rows[1][0].as_u64(), rows[1][6].as_str()), (Some(1), Some("Second speaker here.")));
    let t0 = rows[0][4].as_f64().unwrap();
    assert!((t0 - (start + Tick::from_seconds_f64(0.2)).seconds()).abs() < 0.002, "{t0}");
    assert_eq!(r["speakers"], json!(["Speaker 1", "Speaker 2"]));
    assert!(r.to_string().len() * 2 < full.to_string().len(), "compact {r}");
    // hostile paragraph gaps fall back to the default
    for gap in [json!(-5.0), json!(1e300), json!("x")] {
        let r = s.execute("transcript.inspect", json!({"words": false, "paragraphGapSeconds": gap})).unwrap();
        assert_eq!(r["paragraphs"].as_array().unwrap().len(), 2, "{gap}");
    }
}

/// `transcript.keep`: keep paragraphs, word spans or times; everything else is ripple-deleted in
/// one undo step, and a bad list is an error that changes nothing.
#[test]
fn keep_only_selected_text() {
    let (mut s, item, _) = session();
    // paragraphs need a transcript
    assert!(s.execute("transcript.keep", json!({"paragraphs": [0]})).is_err());
    s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    let before = s.active_sequence().unwrap().duration();

    let r = s.execute("transcript.keep", json!({"paragraphs": [1], "padSeconds": 0})).unwrap();
    assert_eq!(r["kept"], 1, "{r}");
    assert_eq!(words(&mut s), ["Second", "speaker", "here."], "only paragraph 1 is left");
    let after = s.active_sequence().unwrap().duration();
    assert!(after <= Tick::from_seconds_f64(1.5), "1.4 s of speech, frame-aligned: {after:?} of {before:?}");
    assert!((r["seconds"].as_f64().unwrap() - after.seconds()).abs() < 0.001, "{r}");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.active_sequence().unwrap().duration(), before, "one undo step");
    assert_eq!(words(&mut s).len(), 6);

    // overlapping word spans merge
    s.execute("transcript.keep", json!({"words": [[0, 1], [1, 2]]})).unwrap();
    assert_eq!(words(&mut s), ["Hello", "um", "world."]);
    s.execute("edit.undo", json!({})).unwrap();

    // times, as transcript.inspect {words:false} reports them
    let rows = s.execute("transcript.inspect", json!({"words": false})).unwrap()["paragraphs"].clone();
    let (a, b) = (rows[1][4].as_f64().unwrap(), rows[1][5].as_f64().unwrap());
    s.execute("transcript.keep", json!({"seconds": [[a, b]]})).unwrap();
    assert_eq!(words(&mut s), ["Second", "speaker", "here."]);
    s.execute("edit.undo", json!({})).unwrap();

    // keeping everything removes nothing
    let total = before.seconds();
    let r = s.execute("transcript.keep", json!({"seconds": [[0, total + 10.0]]})).unwrap();
    assert_eq!(r["removed"], 0, "{r}");
    assert_eq!(s.active_sequence().unwrap().duration(), before);

    for bad in [
        json!({}),
        json!({"paragraphs": [2]}),
        json!({"paragraphs": "0"}),
        json!({"paragraphs": [-1]}),
        json!({"words": [[0]]}),
        json!({"words": [[0, 99]]}),
        json!({"seconds": [[2.0, 1.0]]}),
        json!({"seconds": [[-1.0, 1.0]]}),
        json!({"seconds": [[1e9, 2e9]]}),
        json!({"seconds": [["a", 1.0]]}),
        json!({"paragraphs": [0], "padSeconds": -1}),
        json!({"paragraphs": [0], "padSeconds": 99}),
        json!({"paragraphs": vec![0; 10_001]}),
    ] {
        assert!(s.execute("transcript.keep", bad.clone()).is_err(), "{bad}");
        assert_eq!(s.active_sequence().unwrap().duration(), before, "{bad} changed nothing");
    }
}

/// A caption's minimum duration never carries it past the last clip: that would lengthen the
/// sequence, and an export would end on black (found by the vertical-short recipe's check).
#[test]
fn captions_stop_at_the_end_of_the_clips() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    let r = s.execute("transcript.inspect", json!({"words": false})).unwrap();
    let last_word_end = r["paragraphs"][1][5].as_f64().unwrap();
    // keep only the speech, so the last word ends with the clips
    s.execute("transcript.keep", json!({"seconds": [[0, last_word_end]], "padSeconds": 0})).unwrap();
    let before = s.active_sequence().unwrap().duration();
    s.execute("transcript.createCaptions", json!({"minSeconds": 600})).unwrap();
    let q = s.active_sequence().unwrap();
    assert_eq!(q.duration(), before, "captions do not lengthen the sequence");
    assert!(q.caption_tracks[0].captions.iter().all(|c| c.start + c.duration <= before), "every caption ends with the clips");
}

#[test]
fn select_extract_and_lift_by_words() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    let rate = s.sequence_rate();
    let before = s.active_sequence().unwrap().duration();

    let r = s.execute("transcript.select", json!({"from": 3, "to": 5})).unwrap();
    let (a, b) = (Tick(r["start"].as_i64().unwrap()), Tick(r["end"].as_i64().unwrap()));
    let q = s.active_sequence().unwrap();
    assert_eq!(q.mark_in, Some(a));
    assert_eq!(q.mark_out, Some(b - rate.frame_duration()), "Out is the last frame inside");
    assert_eq!(rate.snap(a), a, "frame aligned");
    assert_eq!(s.playhead(), a);

    // Extract "um": the sequence gets shorter by the word's frames and the word is gone
    let r = s.execute("transcript.extract", json!({"from": 1})).unwrap();
    let cut = Tick(r["end"].as_i64().unwrap()) - Tick(r["start"].as_i64().unwrap());
    assert!(cut > Tick::ZERO);
    assert_eq!(s.active_sequence().unwrap().duration(), before - cut);
    assert!(!words(&mut s).contains(&"um".to_string()), "{:?}", words(&mut s));
    s.active_sequence().unwrap().check().unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.active_sequence().unwrap().duration(), before);

    // Lift leaves a gap: same duration, word gone
    s.execute("transcript.lift", json!({"from": 1, "to": 1})).unwrap();
    assert_eq!(s.active_sequence().unwrap().duration(), before);
    assert_eq!(words(&mut s), ["Hello", "world.", "Second", "speaker", "here."]);
    assert!(s.execute("transcript.extract", json!({"from": 99})).is_err());
}

#[test]
fn remove_fillers_pauses_and_create_captions() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    let before = s.active_sequence().unwrap().duration();
    let r = s.execute("transcript.removeFillers", json!({})).unwrap();
    assert_eq!(r["removed"], 1, "{r}");
    assert!(!words(&mut s).contains(&"um".to_string()));
    let r = s.execute("transcript.removePauses", json!({"minSeconds": 1.0, "keepSeconds": 0.1})).unwrap();
    assert_eq!(r["removed"], 1, "{r}");
    let after = s.active_sequence().unwrap().duration();
    assert!(after < before - Tick::from_seconds_f64(1.0), "the pause and the filler are gone");
    assert_eq!(words(&mut s).len(), 5);
    s.active_sequence().unwrap().check().unwrap();

    let r = s.execute("transcript.createCaptions", json!({"maxChars": 32})).unwrap();
    assert_eq!(r["captions"], 2, "one caption per speaker: {r}");
    let q = s.active_sequence().unwrap();
    let tr = &q.caption_tracks[0];
    assert_eq!(tr.captions[0].text, "Hello world.");
    assert_eq!(tr.captions[1].speaker.as_deref(), Some("Speaker 2"));
    q.check().unwrap();
}

#[test]
fn set_delete_and_models() {
    let (mut s, item, _) = session();
    let t = json!({"language": "en", "words": [
        {"text": "b", "start": 2000, "end": 3000, "speaker": 1},
        {"text": "a", "start": 0, "end": 1000},
    ]});
    let r = s.execute("transcript.set", json!({"item": item.0, "transcript": t})).unwrap();
    assert_eq!(r["words"], 2);
    let tr = &s.project.transcripts[&item];
    assert_eq!(tr.words[0].text, "a", "normalized: sorted");
    assert_eq!(tr.speakers.len(), 2, "missing speaker labels added");
    assert_eq!(tr.source, "imported");
    assert!(s.execute("transcript.set", json!({"item": 999_999, "transcript": {}})).is_err());
    assert!(s.execute("transcript.set", json!({"item": item.0, "transcript": {"words": 3}})).is_err());

    // survives save/load
    let bytes = epsicut_format::encode(&s.project, true);
    let back = epsicut_format::decode(&bytes).unwrap();
    assert_eq!(back.project.transcripts[&item].words.len(), 2);

    s.execute("transcript.delete", json!({"items": [item.0]})).unwrap();
    assert!(s.project.transcripts.is_empty());
    assert!(s.execute("transcript.delete", json!({})).is_err());

    let m = s.execute("transcript.models", json!({})).unwrap();
    assert_eq!(m["available"], epsicut_speech::available());
    assert!(m["models"].as_array().unwrap().iter().any(|x| x["id"] == "whisper-base"));
}

#[test]
fn generate_without_a_transcriber() {
    let (mut s, item, _) = session();
    s.transcriber = None;
    let e = s.execute("transcript.generate", json!({"items": [item.0], "model": "nope"})).unwrap_err().to_string();
    // a build without speech-to-text says that first: the command is disabled (#97)
    let why = if epsicut_speech::available() { "unknown speech model" } else { "not available in this build" };
    assert!(e.contains(why), "{e}");
    if !epsicut_speech::available() {
        assert!(!s.is_enabled("sequence.transcribe"), "Transcribe Sequence follows transcript.generate");
        let e = s.execute("transcript.generate", json!({"items": [item.0]})).unwrap_err().to_string();
        assert!(e.contains("whisper") && e.contains("not available"), "{e}");
        #[cfg(not(feature = "speech-download"))]
        assert!(s.execute("transcript.downloadModel", json!({})).unwrap_err().to_string().contains("not available"));
    }
    // defaults to the media of the open sequence's audio clips
    s.transcriber = Some(Arc::new(FixedTranscriber { transcript: Transcript::default(), id: "empty".into() }));
    assert!(s.is_enabled("sequence.transcribe"), "an installed recogniser enables Transcribe Sequence");
    let r = s.execute("transcript.generate", json!({})).unwrap();
    assert!(r["items"].as_array().unwrap().len() >= 2, "{r}");
}
