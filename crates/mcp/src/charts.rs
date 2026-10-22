//! What to draw: results that read better as a picture (a breakdown, a change
//! over time, a comparison) start with a `present` field, one chart (or two,
//! for folder_growth) with its type, title, why, unit and the data to plot:
//! a short flat list, values already in `unit`. Small or flat results get
//! none, and neither do actions (deleting, emptying the Recycle Bin).

use serde_json::{Map, Value, json};

/// Most items in a chart; the rest go into "Other".
const TOP: usize = 12;

/// The `present` value for `tool`'s `result` (called with `args`), if a
/// chart would help.
pub fn present(tool: &str, args: &Value, result: &Value) -> Option<Value> {
    match tool {
        "folder_sizes" => folder_sizes(result),
        "folder_growth" => folder_growth(result),
        "disk_overview" => disk_overview(result),
        "cleanup_candidates" => cleanup(result),
        "search_files" => search(args, result),
        _ => None,
    }
}

/// Puts `present` first in an object result, so it's read before the rest.
pub fn annotate(tool: &str, args: &Value, result: Value) -> Value {
    match (present(tool, args, &result), result) {
        (Some(p), Value::Object(rest)) => {
            let mut m = Map::with_capacity(rest.len() + 1);
            m.insert("present".into(), p);
            m.extend(rest);
            Value::Object(m)
        }
        (_, r) => r,
    }
}

/// A unit that makes `max` (bytes) read well, and its size.
/// TB only from 10 TB, so a drive's free space next to its size doesn't
/// round away (27 GB free on a 1.8 TB drive is 0.0 TB).
fn unit(max: u64) -> (&'static str, f64) {
    match max {
        m if m >= 10 << 40 => ("TB", (1u64 << 40) as f64),
        m if m >= 1 << 30 => ("GB", (1u64 << 30) as f64),
        m if m >= 1 << 20 => ("MB", (1u64 << 20) as f64),
        _ => ("KB", 1024.0),
    }
}

fn round1(bytes: f64, per: f64) -> f64 {
    (bytes / per * 10.0).round() / 10.0 + 0.0 // + 0.0: no "-0.0"
}

/// Like [`round1`], but a value that isn't zero doesn't round to zero (3 decimals then).
fn round_keep(bytes: f64, per: f64) -> f64 {
    match round1(bytes, per) {
        0.0 if bytes != 0.0 => (bytes / per * 1000.0).round() / 1000.0 + 0.0,
        v => v,
    }
}

fn chart(kind: &str, title: String, why: &str, unit: &str, mut data: Value) -> Value {
    // Items too small to see in this unit are noise in a list of bars.
    if let Value::Array(items) = &mut data {
        items.retain(|i| i["value"].as_f64() != Some(0.0));
    }
    json!({ "as": "chart", "type": kind, "title": title, "why": why, "unit": unit, "data": data })
}

/// Real entries of a `children` list (not the "(n more)" summary rows).
fn entries(v: &Value) -> Vec<&Value> {
    v.get("children")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|c| c.get("path").is_some()).collect())
        .unwrap_or_default()
}

fn size(v: &Value) -> u64 {
    v["size"].as_u64().unwrap_or(0)
}

/// `(label, bytes)` bars, the biggest `TOP`, the rest (of `total`) as "Other".
fn bars(items: Vec<(String, u64)>, total: u64) -> (&'static str, Value) {
    let shown: Vec<_> = items.into_iter().take(TOP).collect();
    let sum: u64 = shown.iter().map(|x| x.1).sum();
    let other = total.saturating_sub(sum);
    let (u, per) = unit(shown.iter().map(|x| x.1).max().unwrap_or(0).max(other));
    let mut data: Vec<Value> = shown
        .iter()
        .map(|(l, b)| json!({ "label": l, "value": round1(*b as f64, per) }))
        .collect();
    if round1(other as f64, per) > 0.0 {
        data.push(json!({ "label": "Other", "value": round1(other as f64, per) }));
    }
    (u, Value::Array(data))
}

fn folder_sizes(r: &Value) -> Option<Value> {
    let mut kids = entries(r);
    let total = size(r);
    if kids.len() < 3 || total == 0 {
        return None;
    }
    kids.sort_by_key(|c| std::cmp::Reverse(size(c)));
    let path = r["path"].as_str().unwrap_or("");
    let title = format!("What takes the space in {path}");
    if kids.iter().any(|c| !entries(c).is_empty()) {
        // Treemap: the top folders and the biggest few inside each.
        let top: Vec<&&Value> = kids.iter().take(TOP).collect();
        let (u, per) = unit(top.iter().map(|c| size(c)).max().unwrap_or(0));
        let mut data = Vec::new();
        for c in &top {
            let name = c["name"].as_str().unwrap_or("");
            data.push(json!({ "label": name, "value": round1(size(c) as f64, per), "parent": Value::Null }));
            let mut inner = entries(c);
            inner.sort_by_key(|x| std::cmp::Reverse(size(x)));
            for x in inner.iter().take(4) {
                data.push(json!({ "label": x["name"], "value": round1(size(x) as f64, per), "parent": name }));
            }
        }
        return Some(chart(
            "treemap",
            title,
            "Shows at a glance which folders, and what inside them, take the space.",
            u,
            Value::Array(data),
        ));
    }
    let items = kids
        .iter()
        .map(|c| (c["name"].as_str().unwrap_or("").to_string(), size(c)))
        .collect();
    let (u, data) = bars(items, total);
    Some(chart(
        "horizontal_bar",
        title,
        "Shows how the space splits, largest first.",
        u,
        data,
    ))
}

fn folder_growth(r: &Value) -> Option<Value> {
    let path = r["path"].as_str().unwrap_or("");
    let mut out = Vec::new();
    let tl = &r["timeline"];
    if let (Some(x), Some(series)) = (tl["times"].as_array(), tl["series"].as_array())
        && x.len() >= 3
        && !series.is_empty()
    {
        let max = series
            .iter()
            .flat_map(|s| s["sizes"].as_array().into_iter().flatten())
            .filter_map(Value::as_u64)
            .max()
            .unwrap_or(0);
        let (u, per) = unit(max);
        let series: Vec<Value> = series
            .iter()
            .map(|s| {
                let p = s["path"].as_str().unwrap_or("");
                // The folder's name (the title has the path); a drive root as is.
                let label = p
                    .trim_end_matches('\\')
                    .rsplit('\\')
                    .next()
                    .filter(|n| !n.ends_with(':'))
                    .unwrap_or(p);
                let values: Vec<Value> = s["sizes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| {
                        v.as_u64()
                            .map_or(Value::Null, |b| json!(round1(b as f64, per)))
                    })
                    .collect();
                json!({ "label": label, "values": values })
            })
            .collect();
        out.push(chart(
            "line",
            format!("Size over time: {path}"),
            "Shows when the size changed (null: not in that snapshot).",
            u,
            json!({ "x": x, "series": series }),
        ));
    }
    let kids = r["children"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let changed: Vec<&Value> = kids
        .iter()
        .filter(|c| c["change"].as_i64().unwrap_or(0) != 0)
        .collect();
    if changed.iter().filter(|c| c.get("path").is_some()).count() >= 3 {
        let (u, per) = unit(
            changed
                .iter()
                .map(|c| c["change"].as_i64().unwrap_or(0).unsigned_abs())
                .max()
                .unwrap_or(0),
        );
        let data: Vec<Value> = changed
            .iter()
            .take(TOP + 1)
            .map(|c| {
                // Summary rows ("(12 more changed)") keep their name, without the brackets.
                let name = c["name"].as_str().unwrap_or("");
                let label = if c.get("path").is_some() { name } else { name.trim_matches(['(', ')']) };
                json!({ "label": label, "value": round1(c["change"].as_i64().unwrap_or(0) as f64, per) })
            })
            .collect();
        let since = r["compared_with"]["time"].as_str().unwrap_or("then");
        let c = chart(
            "diverging_bar",
            format!("What grew and shrank in {path} since {since}"),
            "Growth one side, shrinking the other; biggest changes first.",
            u,
            Value::Array(data),
        );
        // One change dwarfing the rest is a sentence, not a chart.
        if c["data"].as_array().is_some_and(|d| d.len() >= 2) {
            out.push(c);
        }
    }
    match out.len() {
        0 => None,
        1 => out.pop(),
        _ => Some(Value::Array(out)),
    }
}

fn disk_overview(r: &Value) -> Option<Value> {
    let drives: Vec<&Value> = r["volumes"]
        .as_array()?
        .iter()
        .filter(|v| v["capacity"].as_u64().is_some_and(|c| c > 0))
        .collect();
    if drives.is_empty() {
        return None;
    }
    let (u, per) = unit(
        drives
            .iter()
            .map(|v| v["capacity"].as_u64().unwrap_or(0))
            .max()
            .unwrap_or(0),
    );
    let data: Vec<Value> = drives
        .iter()
        .map(|v| {
            let (cap, free) = (v["capacity"].as_u64().unwrap_or(0), v["free"].as_u64().unwrap_or(0));
            json!({ "label": v["volume"], "used": round_keep(cap.saturating_sub(free) as f64, per), "free": round_keep(free as f64, per) })
        })
        .collect();
    Some(chart(
        "stacked_bar",
        "How full each drive is".into(),
        "Used and free space of every drive in one chart.",
        u,
        Value::Array(data),
    ))
}

fn cleanup(r: &Value) -> Option<Value> {
    let mut cats: Vec<(String, u64)> = r["categories"]
        .as_array()?
        .iter()
        // Page and hibernation files are shown for information, not reclaimable.
        .filter(|c| c["id"] != "system_files")
        .map(|c| {
            (
                c["title"].as_str().unwrap_or("").to_string(),
                c["total_size"].as_u64().unwrap_or(0),
            )
        })
        .filter(|c| c.1 > 0)
        .collect();
    if cats.len() < 3 {
        return None;
    }
    cats.sort_by_key(|c| std::cmp::Reverse(c.1));
    let total = cats.iter().map(|c| c.1).sum();
    let (u, data) = bars(cats, total);
    Some(chart(
        "horizontal_bar",
        "Space that could be freed, by category".into(),
        "Shows where the reclaimable space is.",
        u,
        data,
    ))
}

fn search(a: &Value, r: &Value) -> Option<Value> {
    let rows = r["results"].as_array()?;
    let by_size = a["sort"].as_str() == Some("size")
        || (a.get("sort").is_none()
            && (a.get("min_size").is_some() || a.get("max_size").is_some()));
    if rows.len() < 20 || r.get("content").is_some() || !by_size {
        return None; // a list of files reads fine as a list
    }
    let items: Vec<(String, u64)> = rows
        .iter()
        .map(|x| {
            (
                x["path"]
                    .as_str()
                    .unwrap_or("")
                    .rsplit('\\')
                    .next()
                    .unwrap_or("")
                    .to_string(),
                size(x),
            )
        })
        .collect();
    let total = items.iter().take(TOP).map(|x| x.1).sum();
    let (u, data) = bars(items, total);
    Some(chart(
        "horizontal_bar",
        "Largest matching files".into(),
        "Shows how a few files compare in size.",
        u,
        data,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    fn kids(n: usize, nested: bool) -> Vec<Value> {
        (0..n)
            .map(|i| {
                let mut c = json!({ "name": format!("f{i}"), "path": format!(r"C:\f{i}"), "size": (20 - i as u64) * GB, "change": (i as i64 - 1) * GB as i64 });
                if nested {
                    c["children"] = json!([{ "name": "x", "path": r"C:\f\x", "size": GB }]);
                }
                c
            })
            .collect()
    }

    #[test]
    fn charts_only_where_they_help() {
        let sizes = json!({ "path": r"C:\", "size": 100 * GB, "children": kids(4, true) });
        let p = present("folder_sizes", &json!({}), &sizes).unwrap();
        assert_eq!(
            (p["as"].as_str(), p["type"].as_str(), p["unit"].as_str()),
            (Some("chart"), Some("treemap"), Some("GB"))
        );
        assert_eq!(
            p["data"][0],
            json!({ "label": "f0", "value": 20.0, "parent": null })
        );
        assert_eq!(p["data"][1]["parent"], json!("f0"));
        let flat = json!({ "path": r"C:\", "size": 100 * GB, "children": kids(4, false) });
        let p = present("folder_sizes", &json!({}), &flat).unwrap();
        assert_eq!(p["type"], json!("horizontal_bar"));
        assert_eq!(
            p["data"].as_array().unwrap().last().unwrap(),
            &json!({ "label": "Other", "value": 26.0 })
        );
        assert_eq!(
            present(
                "folder_sizes",
                &json!({}),
                &json!({ "size": 300, "children": kids(2, false) })
            ),
            None
        );

        let growth = json!({ "path": r"C:\", "children": kids(5, false), "timeline": { "times": ["a", "b", "now"], "series": [{ "path": r"C:\", "sizes": [2 * GB, null, 3 * GB] }] } });
        let p = present("folder_growth", &json!({}), &growth).unwrap();
        assert_eq!(
            (p[0]["type"].as_str(), p[1]["type"].as_str()),
            (Some("line"), Some("diverging_bar"))
        );
        assert_eq!(p[0]["data"]["series"][0]["values"], json!([2.0, null, 3.0]));
        assert_eq!(p[1]["data"][0], json!({ "label": "f0", "value": -1.0 }));
        let p = present(
            "folder_growth",
            &json!({}),
            &json!({ "path": "x", "children": kids(5, false) }),
        )
        .unwrap();
        assert_eq!(p["type"], json!("diverging_bar"), "one chart: not an array");
        assert_eq!(
            present(
                "folder_growth",
                &json!({}),
                &json!({ "children": kids(2, false) })
            ),
            None
        );

        let none = json!({ "volumes": [{ "volume": "\\\\nas\\x", "entries": 5 }] });
        assert_eq!(
            present("disk_overview", &json!({}), &none),
            None,
            "no drive with a capacity"
        );
        let one_drive =
            json!({ "volumes": [{ "volume": "C", "capacity": 10 * GB, "free": 2 * GB }] });
        assert_eq!(
            present("disk_overview", &json!({}), &one_drive).unwrap()["type"],
            json!("stacked_bar")
        );
        // 27 GB free on a 1.8 TB drive: GB, not 0.0 TB (which drew the drive as full).
        let tb = 1u64 << 40;
        let big = json!({ "volumes": [
            { "volume": "S", "capacity": tb * 18 / 10, "free": 27 * GB },
            { "volume": "C", "capacity": 930 * GB, "free": 0 },
            { "volume": "F", "capacity": 931 * GB, "free": GB / 50 },
        ] });
        let p = present("disk_overview", &json!({}), &big).unwrap();
        assert_eq!(p["unit"], json!("GB"));
        assert_eq!(
            p["data"][0],
            json!({ "label": "S", "used": 1816.2, "free": 27.0 })
        );
        assert_eq!(p["data"][1]["free"], json!(0.0), "really full");
        assert_eq!(p["data"][2]["free"], json!(0.02), "small, not rounded away");
        assert_eq!(unit(12 * tb).0, "TB");
        let two = json!({ "volumes": [{ "volume": "C", "capacity": 4 * GB, "free": GB }, { "volume": "D", "capacity": 2 * GB, "free": GB }] });
        assert_eq!(
            present("disk_overview", &json!({}), &two).unwrap()["data"][0],
            json!({ "label": "C", "used": 3.0, "free": 1.0 })
        );

        let rows = json!({ "results": vec![json!({ "path": r"C:\a\big.iso", "size": GB }); 25] });
        assert_eq!(
            present("search_files", &json!({ "sort": "size" }), &rows).unwrap()["data"][0],
            json!({ "label": "big.iso", "value": 1.0 })
        );
        assert_eq!(
            present("search_files", &json!({ "name": "x" }), &rows),
            None,
            "a plain list"
        );
        let few = json!({ "results": vec![json!({ "size": 1 }); 5] });
        assert_eq!(
            present("search_files", &json!({ "sort": "size" }), &few),
            None
        );
        // Cleanup: reclaimable categories only (system files are informational).
        let cats = json!({ "categories": [
            { "id": "temp", "title": "Temp", "total_size": 3 * GB },
            { "id": "system_files", "title": "System files", "total_size": 30 * GB },
            { "id": "recycle_bin", "title": "Recycle Bin", "total_size": 2 * GB },
            { "id": "crash_dumps", "title": "Dumps", "total_size": GB },
        ] });
        let p = present("cleanup_candidates", &json!({}), &cats).unwrap();
        let labels: Vec<&str> = p["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["Temp", "Recycle Bin", "Dumps"]);
        for t in ["delete_paths", "empty_recycle_bin"] {
            assert_eq!(present(t, &json!({}), &sizes), None);
        }
        // First key in the result.
        let out = annotate("folder_sizes", &json!({}), sizes);
        assert_eq!(
            out.as_object().unwrap().keys().next().map(String::as_str),
            Some("present")
        );
        assert!(
            serde_json::to_string(&out)
                .unwrap()
                .starts_with(r#"{"present":"#)
        );
        // Bars that round to nothing are left out; no "-0.0"; one bar left is no chart.
        let mut tiny = json!({ "path": "x", "children": [
            { "name": "big", "path": "x\\big", "change": -15 * GB as i64 },
            { "name": "small", "path": "x\\small", "change": 1 << 20 },
            { "name": "less", "path": "x\\less", "change": -(1i64 << 20) },
        ] });
        assert_eq!(present("folder_growth", &json!({}), &tiny), None);
        tiny["children"][1]["change"] = json!(2 * GB);
        let p = present("folder_growth", &json!({}), &tiny).unwrap();
        assert_eq!(
            p["data"],
            json!([{ "label": "big", "value": -15.0 }, { "label": "small", "value": 2.0 }])
        );
        assert_eq!(
            serde_json::to_string(&json!(round1(-1.0, GB as f64))).unwrap(),
            "0.0"
        );
        assert_eq!(unit(5 << 40).0, "GB");
        assert_eq!(unit(5 << 20).0, "MB");
    }
}
