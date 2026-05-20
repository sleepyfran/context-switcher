use crate::cli::ListArgs;
use anyhow::Result;
use chrono::{DateTime, Utc};
use csw_core::config::Config;
use csw_core::identity;
use csw_core::ops::{self, ListRequest, TaskEntry};
use serde_json::json;

pub fn run(args: ListArgs) -> Result<i32> {
    let config = Config::load()?;
    let user = identity::resolve_username(&config)?;
    let request = ListRequest {
        user,
        only_repo: args.repo,
    };
    let entries = ops::list::list(&config, &request)?;

    if args.json {
        emit_json(&entries);
    } else {
        emit_table(&entries);
    }
    Ok(0)
}

fn emit_json(entries: &[TaskEntry]) {
    let payload: Vec<_> = entries
        .iter()
        .map(|t| {
            json!({
                "task_id": t.task_id,
                "title": t.title,
                "created_at": t.created_at,
                "repos": t.repos.iter().map(|r| json!({
                    "repo": r.repo,
                    "worktree_path": r.worktree_path,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".into())
    );
}

fn emit_table(entries: &[TaskEntry]) {
    if entries.is_empty() {
        println!("(no task copies on disk)");
        return;
    }

    let now = Utc::now();
    let header = ["TASK", "REPOS", "TITLE", "AGE"];
    let rows: Vec<[String; 4]> = entries
        .iter()
        .map(|t| {
            let repos: Vec<&str> = t.repos.iter().map(|r| r.repo.as_str()).collect();
            [
                t.task_id.clone(),
                repos.join(", "),
                t.title.clone().unwrap_or_else(|| "(no title)".into()),
                t.created_at
                    .map(|c| format_age(now, c))
                    .unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    print_table(&header, &rows);
}

fn format_age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(then);
    let secs = delta.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn print_table(header: &[&str; 4], rows: &[[String; 4]]) {
    let mut widths = header.map(|h| h.len());
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    print_row(header.map(|s| s.to_string()), &widths);
    for row in rows {
        print_row(row.clone(), &widths);
    }
}

fn print_row(row: [String; 4], widths: &[usize; 4]) {
    let cells: Vec<String> = row
        .iter()
        .zip(widths.iter())
        .map(|(c, w)| format!("{c:<width$}", width = *w))
        .collect();
    println!("{}", cells.join("  "));
}
