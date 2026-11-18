//! Stage 1d — render the analysis as a single self-contained HTML page.
//!
//! The report is the artefact a human actually reads before approving work.
//! It must be:
//!
//! * **Self-contained** — no CDN, no external CSS, no JS dependencies, so it
//!   opens from a network share or an air-gapped machine.
//! * **Honest about estimates** — every projected saving is labelled as an
//!   estimate, computed from bits-per-pixel rather than from real encodes.
//! * **Actionable** — the batches the plan produced are listed with their ids,
//!   because those ids are what a later `ma apply --batch <id>` will consume.

use std::fmt::Write as _;

use crate::analysis::{Analysis, Decision};
use crate::plan::Plan;
use crate::policy::Policy;
use crate::{human_bytes, RunDir};

/// Render the report to `run.report()`.
pub fn write_report(
    run: &RunDir,
    analysis: &Analysis,
    plan: &Plan,
    policy: &Policy,
) -> anyhow::Result<()> {
    let html = render(analysis, plan, policy, &run.root.to_string_lossy());
    std::fs::write(run.report(), html)?;
    Ok(())
}

pub fn render(analysis: &Analysis, plan: &Plan, policy: &Policy, run_root: &str) -> String {
    let mut html = String::with_capacity(256 * 1024);

    html.push_str(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>MediaArchiving report</title>
<style>
:root{--bg:#0f1115;--panel:#171a21;--line:#262b36;--text:#e6e9ef;--dim:#9aa4b5;
--accent:#6ea8fe;--good:#57d0a0;--warn:#f0b350;--bad:#f2708a}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--text);
font:14px/1.55 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}
main{max-width:1180px;margin:0 auto;padding:40px 24px 80px}
h1{font-size:26px;margin:0 0 4px;letter-spacing:-.01em}
h2{font-size:17px;margin:36px 0 12px;padding-bottom:6px;border-bottom:1px solid var(--line)}
h3{font-size:14px;margin:20px 0 8px;color:var(--dim);font-weight:600}
.sub{color:var(--dim);margin:0 0 28px}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:14px 16px}
.card .k{color:var(--dim);font-size:12px;text-transform:uppercase;letter-spacing:.06em}
.card .v{font-size:22px;font-weight:650;margin-top:4px;font-variant-numeric:tabular-nums}
.card .n{color:var(--dim);font-size:12px;margin-top:2px}
table{width:100%;border-collapse:collapse;font-size:13px}
th,td{text-align:left;padding:7px 10px;border-bottom:1px solid var(--line)}
th{color:var(--dim);font-weight:600;font-size:12px;text-transform:uppercase;letter-spacing:.05em}
td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}
tr:hover td{background:#1b1f28}
.tag{display:inline-block;padding:1px 7px;border-radius:999px;font-size:11px;font-weight:600}
.t-low{background:#1d3b30;color:var(--good)}
.t-medium{background:#3d3220;color:var(--warn)}
.t-high{background:#3d2029;color:var(--bad)}
.d-transcode{color:var(--good)}
.d-blocked{color:var(--bad)}
.d-skip,.d-archive_only{color:var(--dim)}
.note{background:var(--panel);border:1px solid var(--line);border-left:3px solid var(--warn);
border-radius:6px;padding:12px 16px;margin:16px 0;color:var(--dim)}
code{background:#20242e;padding:1px 5px;border-radius:4px;font-size:12px}
details{margin:8px 0}
summary{cursor:pointer;color:var(--accent);font-size:13px}
.paths{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:11.5px;
color:var(--dim);max-height:260px;overflow:auto;margin:8px 0 0;
background:#12151b;border-radius:6px;padding:10px 12px;white-space:pre}
</style></head><body><main>
"#,
    );

    // ---- Header ----
    let _ = write!(
        html,
        "<h1>MediaArchiving report</h1>\n<p class=\"sub\">intent <code>{}</code> · run <code>{}</code></p>\n",
        analysis.intent.label(),
        escape(run_root)
    );

    // ---- Headline numbers ----
    let t = &analysis.totals;
    html.push_str("<div class=\"cards\">\n");
    card(
        &mut html,
        "Files scanned",
        &t.files.to_string(),
        &human_bytes(t.total_bytes),
    );
    card(
        &mut html,
        "Transcode candidates",
        &t.transcodable_files.to_string(),
        &human_bytes(t.transcodable_bytes),
    );
    card(
        &mut html,
        "Estimated saving",
        &human_bytes(t.estimated_saving_bytes),
        "from re-encoding alone",
    );
    card(
        &mut html,
        "Redundant duplicates",
        &human_bytes(t.redundant_bytes),
        "non-keeper copies",
    );
    card(
        &mut html,
        "Needs fixing first",
        &t.blocked_files.to_string(),
        "unreadable or mislabelled",
    );
    html.push_str("</div>\n");

    html.push_str(
        "<div class=\"note\"><strong>Savings are estimates.</strong> \
They are modelled from bits-per-pixel, not measured by encoding. \
Actual results vary with image content; a sample batch must be encoded and \
verified before any bulk approval.</div>\n",
    );

    // ---- Container breakdown ----
    html.push_str(
        "<h2>By format</h2>\n<table><thead><tr>\
<th>Container</th><th class=\"num\">Files</th><th class=\"num\">Size</th>\
<th class=\"num\">Candidates</th><th class=\"num\">Est. saving</th>\
<th class=\"num\">Saving %</th></tr></thead><tbody>\n",
    );

    let mut containers: Vec<_> = t.by_container.iter().collect();
    containers.sort_by_key(|a| std::cmp::Reverse(a.1.bytes));
    for (name, stats) in containers {
        let pct = if stats.bytes > 0 {
            stats.saving_bytes as f64 / stats.bytes as f64 * 100.0
        } else {
            0.0
        };
        let _ = writeln!(
            html,
            "<tr><td><code>{}</code></td><td class=\"num\">{}</td>\
<td class=\"num\">{}</td><td class=\"num\">{}</td>\
<td class=\"num\">{}</td><td class=\"num\">{:.1}%</td></tr>",
            escape(name),
            stats.files,
            human_bytes(stats.bytes),
            stats.transcodable,
            human_bytes(stats.saving_bytes),
            pct
        );
    }
    html.push_str("</tbody></table>\n");

    // ---- Risk distribution ----
    html.push_str(
        "<h2>Risk layering</h2>\n<p class=\"sub\">Batches are layered by risk first, \
then split by directory, so the bulk of low-risk work can be approved in one sweep \
while the files that could actually be damaged get individual attention.</p>\n",
    );
    html.push_str("<div class=\"cards\">\n");
    for tier in ["high", "medium", "low"] {
        let count = t.by_risk.get(tier).copied().unwrap_or(0);
        let label = match tier {
            "high" => "High risk",
            "medium" => "Medium risk",
            _ => "Low risk",
        };
        let hint = match tier {
            "high" => "colour profile, rotation, or large files",
            "medium" => "metadata carried over",
            _ => "safe to batch-approve",
        };
        card(&mut html, label, &count.to_string(), hint);
    }
    html.push_str("</div>\n");

    // ---- Batches ----
    let _ = write!(
        html,
        "<h2>Review batches</h2>\n<p class=\"sub\">{} batches covering {} files \
({}) · estimated saving {}</p>\n",
        plan.totals.batches,
        plan.totals.files,
        human_bytes(plan.totals.bytes),
        human_bytes(plan.totals.estimated_saving)
    );

    if plan.batches.is_empty() {
        html.push_str(
            "<p class=\"sub\">No files qualified for transcoding under this policy.</p>\n",
        );
    } else {
        html.push_str(
            "<table><thead><tr>\
<th>Batch</th><th>Risk</th><th>Directory</th><th class=\"num\">Files</th>\
<th class=\"num\">Size</th><th class=\"num\">Est. saving</th>\
</tr></thead><tbody>\n",
        );
        for batch in &plan.batches {
            let _ = writeln!(
                html,
                "<tr><td><code>{}</code></td><td><span class=\"tag t-{}\">{}</span></td>\
<td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
<td class=\"num\">{}</td></tr>",
                escape(&batch.id),
                batch.risk.as_str(),
                batch.risk.as_str(),
                escape(&batch.root),
                batch.files,
                human_bytes(batch.bytes),
                human_bytes(batch.estimated_saving)
            );
        }
        html.push_str("</tbody></table>\n");

        // Full path lists, collapsed, for the high-risk batches only. Listing
        // every path of every batch would make the page unreadably large.
        for batch in plan.batches.iter().filter(|b| b.risk.as_str() == "high") {
            let _ = writeln!(
                html,
                "<details><summary>Paths in {} ({} files)</summary><div class=\"paths\">{}</div></details>",
                escape(&batch.id),
                batch.paths.len(),
                escape(&batch.paths.join("\n"))
            );
        }
    }

    // ---- Blocked ----
    if !plan.blocked.is_empty() {
        let _ = write!(
            html,
            "<h2>Needs fixing first</h2>\n<p class=\"sub\">{} files could not be planned. \
These are unreadable, mislabelled, or redundant duplicate copies. \
Nothing will be done to them until they are resolved.</p>\n",
            plan.blocked.len()
        );
        let shown: Vec<_> = plan.blocked.iter().take(200).collect();
        html.push_str("<details open><summary>Show paths</summary><div class=\"paths\">");
        html.push_str(&escape(
            &shown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        html.push_str("</div></details>\n");
        if plan.blocked.len() > shown.len() {
            let _ = writeln!(
                html,
                "<p class=\"sub\">…and {} more.</p>",
                plan.blocked.len() - shown.len()
            );
        }
    }

    // ---- Policy ----
    html.push_str("<h2>Policy in force</h2>\n<table><tbody>\n");
    let _ = write!(
        html,
        "<tr><td>intent</td><td><code>{}</code></td></tr>\n\
<tr><td>image target</td><td><code>{}</code> quality {}</td></tr>\n\
<tr><td>image min saving</td><td>{}%</td></tr>\n\
<tr><td>resize</td><td><code>{}</code></td></tr>\n\
<tr><td>video target</td><td><code>{}</code> crf {}</td></tr>\n\
<tr><td>keep EXIF / ICC</td><td>{} / {}</td></tr>\n\
<tr><td>min pixels</td><td>{}</td></tr>\n",
        analysis.intent.label(),
        escape(&policy.image.target),
        policy.image.quality,
        policy.image.min_savings_pct,
        escape(&policy.image.resize),
        escape(&policy.video.target),
        policy.video.crf,
        yes_no(policy.metadata.keep_exif),
        yes_no(policy.metadata.keep_icc),
        policy.skip.min_pixels
    );
    html.push_str("</tbody></table>\n");

    // ---- Largest candidates ----
    let mut biggest: Vec<_> = analysis
        .files
        .iter()
        .filter(|f| f.decision == Decision::Transcode)
        .collect();
    biggest.sort_by_key(|a| std::cmp::Reverse(a.estimated_saving));

    if !biggest.is_empty() {
        html.push_str(
            "<h2>Largest opportunities</h2>\n<table><thead><tr>\
<th>Path</th><th>Format</th><th class=\"num\">Size</th>\
<th class=\"num\">Est. after</th><th class=\"num\">Est. saving</th>\
<th>Risk</th></tr></thead><tbody>\n",
        );
        for file in biggest.iter().take(100) {
            let _ = writeln!(
                html,
                "<tr><td>{}</td><td><code>{}</code></td><td class=\"num\">{}</td>\
<td class=\"num\">{}</td><td class=\"num\">{}</td>\
<td><span class=\"tag t-{}\">{}</span></td></tr>",
                escape(&file.path),
                escape(&file.container),
                human_bytes(file.size),
                human_bytes(file.estimated_size.unwrap_or(0)),
                human_bytes(file.estimated_saving),
                file.risk.as_str(),
                file.risk.as_str()
            );
        }
        html.push_str("</tbody></table>\n");
    }

    // ---- Stage 2 notice ----
    html.push_str(
        "<h2>What this run did not do</h2>\n<div class=\"note\">\
<strong>No files were modified.</strong> This is stage 1: scan, analyse, plan, report. \
Transcoding, verification and original-file handling are stage 2 and are not implemented yet. \
Review the batches above, then adjust <code>policy.toml</code> and re-run <code>ma plan</code>.\
</div>\n",
    );

    html.push_str("</main></body></html>\n");
    html
}

fn card(html: &mut String, key: &str, value: &str, note: &str) {
    let _ = writeln!(
        html,
        "<div class=\"card\"><div class=\"k\">{}</div><div class=\"v\">{}</div>\
<div class=\"n\">{}</div></div>",
        escape(key),
        escape(value),
        escape(note)
    );
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// Minimal HTML escaping — enough for the strings we emit (paths and ids).
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}
