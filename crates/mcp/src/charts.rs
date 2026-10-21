//! Optional chart hints: results where a picture says more than the numbers
//! (a breakdown, a change over time, a comparison) get a one-line `chart`
//! field naming the chart, the fields to plot and why. AI apps that can draw
//! inline charts use it; others ignore it. Small or flat results get none,
//! and neither do actions (deleting, emptying the Recycle Bin).

use serde_json::{Value, json};

/// Every hint starts like this, so a client that can't draw skips it.
const IF: &str = "Optional, if you can render charts:";

/// The hint for `tool`'s `result` (called with `args`), if one would help.
pub fn suggest(tool: &str, args: &Value, result: &Value) -> Option<String> {
    match tool {
        "folder_sizes" => folder_sizes(result),
        "folder_growth" => folder_growth(result),
        "disk_overview" => disk_overview(result),
        "cleanup_candidates" => cleanup(result),
        "search_files" => search(args, result),
        _ => None,
    }
}

/// Real entries of a `children` list (not the "(n more)" summary rows).
fn entries(v: &Value) -> Vec<&Value> {
    v.get("children")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|c| c.get("path").is_some()).collect())
        .unwrap_or_default()
}

fn folder_sizes(r: &Value) -> Option<String> {
    let kids = entries(r);
    if kids.len() < 3 || r["size"].as_u64().unwrap_or(0) == 0 {
        return None;
    }
    let nested = kids.iter().any(|c| !entries(c).is_empty());
    Some(if nested {
        format!(
            "{IF} a treemap (or sunburst) of children[].size, nested by children, labelled with name and size_human: shows at a glance which folders take the space."
        )
    } else {
        format!(
            "{IF} a horizontal bar chart of children[].size by name, largest first (percent_of_parent for labels): shows how the space splits."
        )
    })
}

fn folder_growth(r: &Value) -> Option<String> {
    let changed = entries(r)
        .iter()
        .filter(|c| c["change"].as_i64().unwrap_or(0) != 0)
        .count();
    let points = r["timeline"]["times"].as_array().map_or(0, Vec::len);
    let bars =
        "a diverging bar chart of children[].change by name (growth one side, shrinking the other)";
    match (points >= 3, changed >= 3) {
        (true, true) => Some(format!(
            "{IF} a line chart of timeline.series[].sizes over timeline.times (one line per path) for the trend, and {bars} for what changed most."
        )),
        (true, false) => Some(format!(
            "{IF} a line chart of timeline.series[].sizes over timeline.times (one line per path): shows when the size changed."
        )),
        (false, true) => Some(format!("{IF} {bars}: shows what grew and what shrank.")),
        _ => None,
    }
}

fn disk_overview(r: &Value) -> Option<String> {
    let drives = r["volumes"]
        .as_array()?
        .iter()
        .filter(|v| v["capacity"].as_u64().is_some_and(|c| c > 0))
        .count();
    (drives >= 2).then(|| format!("{IF} one stacked bar per drive, volumes[].capacity split into used (capacity - free) and free, labelled with used_percent: compares how full the drives are."))
}

fn cleanup(r: &Value) -> Option<String> {
    let cats = r["categories"]
        .as_array()?
        .iter()
        .filter(|c| c["total_size"].as_u64().unwrap_or(0) > 0)
        .count();
    (cats >= 3).then(|| format!("{IF} a horizontal bar chart of categories[].total_size by title, largest first: shows where the reclaimable space is."))
}

fn search(a: &Value, r: &Value) -> Option<String> {
    let rows = r["results"].as_array()?.len();
    if rows < 20 || r.get("content").is_some() {
        return None; // a list of files reads fine as a list
    }
    let sized = a.get("min_size").is_some() || a.get("max_size").is_some();
    match a["sort"].as_str() {
        Some("size") => Some(format!(
            "{IF} a histogram of results[].size (log scale): shows whether a few big files or many medium ones add up."
        )),
        None if sized => Some(format!(
            "{IF} a histogram of results[].size (log scale): shows whether a few big files or many medium ones add up."
        )),
        Some("modified") => Some(format!(
            "{IF} a scatter of results[].modified (x) by results[].size (y, log scale): shows when the big files appeared."
        )),
        _ => None,
    }
}

/// Adds the hint to an object result.
pub fn annotate(tool: &str, args: &Value, mut result: Value) -> Value {
    if let Some(h) = suggest(tool, args, &result)
        && result.is_object()
    {
        result["chart"] = json!(h);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kids(n: usize, nested: bool) -> Vec<Value> {
        (0..n)
            .map(|i| {
                let mut c = json!({ "name": format!("f{i}"), "path": format!(r"C:\f{i}"), "size": 100 - i, "change": i as i64 - 1 });
                if nested {
                    c["children"] = json!([{ "name": "x", "path": r"C:\f\x", "size": 1 }]);
                }
                c
            })
            .collect()
    }

    #[test]
    fn hints_only_where_a_chart_helps() {
        let sizes = json!({ "size": 300, "children": kids(4, true) });
        assert!(
            suggest("folder_sizes", &json!({}), &sizes)
                .unwrap()
                .contains("treemap")
        );
        let flat = json!({ "size": 300, "children": kids(4, false) });
        assert!(
            suggest("folder_sizes", &json!({}), &flat)
                .unwrap()
                .contains("horizontal bar")
        );
        let tiny = json!({ "size": 300, "children": kids(2, false) });
        assert_eq!(suggest("folder_sizes", &json!({}), &tiny), None);

        let growth = json!({ "children": kids(5, false), "timeline": { "times": ["a", "b", "c", "now"], "series": [] } });
        let h = suggest("folder_growth", &json!({}), &growth).unwrap();
        assert!(
            h.contains("line chart") && h.contains("diverging bar"),
            "{h}"
        );
        let two = json!({ "children": kids(5, false) });
        assert!(
            !suggest("folder_growth", &json!({}), &two)
                .unwrap()
                .contains("line chart")
        );
        assert_eq!(
            suggest(
                "folder_growth",
                &json!({}),
                &json!({ "children": kids(2, false) })
            ),
            None
        );

        let one_drive = json!({ "volumes": [{ "capacity": 10, "free": 2 }] });
        assert_eq!(suggest("disk_overview", &json!({}), &one_drive), None);
        let two_drives =
            json!({ "volumes": [{ "capacity": 10, "free": 2 }, { "capacity": 10, "free": 9 }] });
        assert!(
            suggest("disk_overview", &json!({}), &two_drives)
                .unwrap()
                .contains("stacked bar")
        );

        let rows = json!({ "results": vec![json!({ "size": 1 }); 25] });
        assert!(
            suggest("search_files", &json!({ "sort": "size" }), &rows)
                .unwrap()
                .contains("histogram")
        );
        assert!(
            suggest("search_files", &json!({ "sort": "modified" }), &rows)
                .unwrap()
                .contains("scatter")
        );
        assert_eq!(
            suggest("search_files", &json!({ "name": "x" }), &rows),
            None,
            "a plain list"
        );
        let few = json!({ "results": vec![json!({ "size": 1 }); 5] });
        assert_eq!(
            suggest("search_files", &json!({ "sort": "size" }), &few),
            None
        );

        for t in ["delete_paths", "empty_recycle_bin"] {
            assert_eq!(suggest(t, &json!({}), &sizes), None);
        }
        assert!(
            annotate("folder_sizes", &json!({}), sizes)["chart"]
                .as_str()
                .unwrap()
                .starts_with(IF)
        );
    }
}
