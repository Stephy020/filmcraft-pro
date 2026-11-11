//! Compact replies for agents: `sequence.inspect {detail:"summary"}` and the `command.list` options.

use serde_json::{Value, json};

use crate::Session;

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

/// The summary names every clip the full reply has, with the same ids, in seconds, at a fraction
/// of the size; the full reply stays the default.
#[test]
fn sequence_summary_is_small_and_matches_the_full_inspect() {
    let mut s = demo();
    let full = s.execute("sequence.inspect", json!({})).unwrap();
    assert_eq!(s.execute("sequence.inspect", json!({"detail": "full"})).unwrap(), full, "full is the default");
    let sum = s.execute("sequence.inspect", json!({"detail": "summary"})).unwrap();
    assert_eq!(sum["id"], full["id"]);
    assert_eq!(sum["width"], full["settings"]["width"]);
    assert_eq!(sum["frames"], full["durationFrames"]);
    for kind in ["video", "audio"] {
        let rows: Vec<Value> = sum[kind].as_array().unwrap().iter().flat_map(|t| t["clips"].as_array().unwrap().clone()).collect();
        let clips: Vec<&Value> = full[kind].as_array().unwrap().iter().flat_map(|t| t["items"].as_array().unwrap()).collect();
        assert_eq!(rows.len(), clips.len(), "{kind}");
        for (r, c) in rows.iter().zip(&clips) {
            assert_eq!((&r[0], &r[1]), (&c["clip"], &c["item"]), "{r} {c}");
            let start = c["start"].as_i64().unwrap() as f64 / filmcraft_time::TICKS_PER_SECOND as f64;
            let end = c["end"].as_i64().unwrap() as f64 / filmcraft_time::TICKS_PER_SECOND as f64;
            assert!((r[3].as_f64().unwrap() - start).abs() < 0.001 && (r[4].as_f64().unwrap() - end).abs() < 0.001, "{r} {c}");
        }
    }
    let (small, big) = (sum.to_string().len(), full.to_string().len());
    assert!(small * 3 < big, "summary {small} bytes vs full {big}");
    let e = s.execute("sequence.inspect", json!({"detail": "everything"})).unwrap_err().to_string();
    assert!(e.contains("summary"), "{e}");
}

/// Without params `command.list` is every command in registry order, as before; `filter`,
/// `enabledOnly`, `compact` and `limit` narrow it.
#[test]
fn command_list_compact_filter_and_limit() {
    let mut s = demo();
    let all = s.execute("command.list", json!({})).unwrap();
    let n = all.as_array().unwrap().len();
    assert_eq!(n, crate::command_specs().len());
    assert_eq!(all[0]["id"], crate::command_specs()[0].id);
    assert!(all[0]["menu"].is_array() && all[0]["shortcuts"].is_array());

    let r = s.execute("command.list", json!({"filter": "RAZOR", "compact": true, "limit": 3})).unwrap();
    let rows = r.as_array().unwrap();
    assert!(!rows.is_empty() && rows.len() <= 3, "{r}");
    assert!(rows.iter().all(|row| row.as_array().is_some_and(|a| a.len() == 4)), "{r}");

    // ids starting with the filter come first
    let r = s.execute("command.list", json!({"filter": "playhead", "compact": true})).unwrap();
    let ids: Vec<&str> = r.as_array().unwrap().iter().filter_map(|row| row[0].as_str()).collect();
    let other = ids.iter().position(|id| !id.starts_with("playhead")).unwrap_or(ids.len());
    assert!(other > 0 && ids[other..].iter().all(|id| !id.starts_with("playhead")), "{ids:?}");

    let r = s.execute("command.list", json!({"enabledOnly": true})).unwrap();
    assert!(r.as_array().unwrap().iter().all(|c| c["enabled"] == true));
    assert!(r.as_array().unwrap().len() < n);

    let compact = s.execute("command.list", json!({"compact": true})).unwrap();
    assert_eq!(compact.as_array().unwrap().len(), n);
    assert!(compact.to_string().len() * 5 < all.to_string().len() * 4, "compact is smaller");

    assert_eq!(s.execute("command.list", json!({"limit": 0})).unwrap(), json!([]));
    for limit in [json!(1e30), json!("x"), json!(null)] {
        assert_eq!(s.execute("command.list", json!({"limit": limit})).unwrap().as_array().unwrap().len(), n, "{limit}");
    }
}
