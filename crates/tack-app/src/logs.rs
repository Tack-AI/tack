//! `tack logs`: view the structured JSONL trace written by the
//! observability layer (`~/.tack/agent/logs/tack-<day>.jsonl`). Options:
//! `--tail N` (default 50), `--level <lvl>` (minimum level), `--target <prefix>`
//! (crate/module filter), `--follow` (stream new events).

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct LogOptions {
    pub tail: usize,
    pub level: Option<String>,
    pub target: Option<String>,
    pub follow: bool,
}

/// Today's log file plus earlier days, newest first.
fn log_files(agent_dir: &std::path::Path) -> Vec<PathBuf> {
    let dir = agent_dir.join("logs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    // The observability layer names files tack-<days-since-epoch>.jsonl, so
    // the filename is a reliable chronological key (mtimes can tie/lie).
    files.sort_by_key(|p| day_number(p).unwrap_or(0));
    files.reverse();
    files
}

fn day_number(path: &std::path::Path) -> Option<u64> {
    path.file_stem()?
        .to_str()?
        .strip_prefix("tack-")?
        .parse()
        .ok()
}

fn level_rank(level: &str) -> u8 {
    match level.to_ascii_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        _ => 2,
    }
}

fn format_event(line: &Value) -> String {
    let level = line["level"].as_str().unwrap_or("INFO");
    let target = line["target"].as_str().unwrap_or("");
    let message = line["message"].as_str().unwrap_or("");
    let fields = &line["fields"];
    let extras = match fields.as_object() {
        Some(map) if !map.is_empty() => {
            let pairs: Vec<String> = map
                .iter()
                .take(6)
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            format!("  ({})", pairs.join(" "))
        }
        _ => String::new(),
    };
    let ts = line["ts"].as_u64().unwrap_or(0);
    let secs = ts / 1000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02} {level:<5} {target}: {message}{extras}")
}

fn matches_filters(line: &Value, min_level: u8, target: Option<&str>) -> bool {
    let level = line["level"].as_str().unwrap_or("info");
    if level_rank(level) < min_level {
        return false;
    }
    if let Some(prefix) = target {
        let event_target = line["target"].as_str().unwrap_or("");
        if !event_target.starts_with(prefix) {
            return false;
        }
    }
    true
}

/// Read + filter + format the tail of the log files. Shared by the CLI and
/// the TUI /trace command.
pub fn tail_events(agent_dir: &std::path::Path, options: &LogOptions) -> Vec<String> {
    let min_level = level_rank(options.level.as_deref().unwrap_or("trace"));
    // Read newest-first, taking only the most recent matching events per
    // file until the tail is full; older days fill what the newest day
    // couldn't provide.
    let mut chunks: Vec<Vec<Value>> = Vec::new();
    let mut collected = 0usize;
    for file in log_files(agent_dir) {
        if collected >= options.tail {
            break;
        }
        let chunk = tail_file_events(
            &file,
            options.tail - collected,
            min_level,
            options.target.as_deref(),
        );
        collected += chunk.len();
        chunks.push(chunk);
    }
    // Back to chronological order (chunks were collected newest-day-first).
    let mut lines: Vec<&Value> = Vec::new();
    for chunk in chunks.iter().rev() {
        lines.extend(chunk.iter());
    }
    lines
        .iter()
        .skip(lines.len().saturating_sub(options.tail))
        .map(|v| format_event(v))
        .collect()
}

/// Initial read window (from EOF) for tail_file_events; doubled until the
/// requested number of matching events is found or the whole file is read.
const TAIL_WINDOW_BYTES: u64 = 256 * 1024;

/// The last `want` matching events of one log file, scanned BACKWARDS from
/// EOF in exponentially growing windows: a hundreds-of-MB log is never
/// slurped whole (and its matching lines never all collected) when the
/// requested tail sits near the end.
fn tail_file_events(
    file: &std::path::Path,
    want: usize,
    min_level: u8,
    target: Option<&str>,
) -> Vec<Value> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    if want == 0 {
        return Vec::new();
    }
    let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Vec::new();
    }
    let Ok(mut handle) = std::fs::File::open(file) else {
        return Vec::new();
    };
    let mut window = TAIL_WINDOW_BYTES.min(size);
    loop {
        let start = size.saturating_sub(window);
        if handle.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if handle.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        let content = String::from_utf8_lossy(&buf);
        // The first line of a mid-file window is partial: skip it (a wider
        // window re-reads it whole). A UTF-8 boundary inside the window's
        // first bytes is covered by the same skip (lossy-decode damage
        // only touches the partial line).
        let body = if start > 0 {
            match content.find('\n') {
                Some(i) => &content[i + 1..],
                None => "",
            }
        } else {
            &content[..]
        };
        let mut matches: Vec<Value> = body
            .lines()
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(|value| matches_filters(value, min_level, target))
            .collect();
        if matches.len() >= want || start == 0 {
            // Keep the LAST `want` matches (the most recent events).
            let skip = matches.len().saturating_sub(want);
            return matches.split_off(skip);
        }
        window = (window.saturating_mul(2)).min(size);
    }
}

pub async fn run(options: LogOptions) -> Result<()> {
    let agent_dir = tack_session::default_agent_dir();
    let events = tail_events(&agent_dir, &options);
    if events.is_empty() {
        println!(
            "no trace events found in {} (enable with TACK_TRACE_FILE=1 or observability.enabled)",
            agent_dir.join("logs").display()
        );
        return Ok(());
    }
    for event in &events {
        crate::cli_output::print_out(&format!("{event}\n"));
    }
    if options.follow {
        // Tail the newest file for new lines: keep a byte offset and read
        // only the appended increment each tick (never the whole file).
        // A truncated/rotated file (size < offset) restarts from 0.
        let Some(file) = log_files(&agent_dir).into_iter().next() else {
            return Ok(());
        };
        let mut offset = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        let min_level = level_rank(options.level.as_deref().unwrap_or("trace"));
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let Ok(meta) = std::fs::metadata(&file) else {
                continue;
            };
            if meta.len() < offset {
                // Rotated or truncated since the last tick: restart at the
                // beginning of the new contents.
                offset = 0;
            }
            if meta.len() == offset {
                continue;
            }
            let Some((segment, consumed)) = read_increment(&file, offset) else {
                continue;
            };
            offset = consumed;
            for raw in segment.lines() {
                if let Ok(value) = serde_json::from_str::<Value>(raw)
                    && matches_filters(&value, min_level, options.target.as_deref())
                {
                    crate::cli_output::print_out(&format!("{}\n", format_event(&value)));
                }
            }
        }
    }
    Ok(())
}

/// Read the bytes appended after `offset`. Returns the complete-line prefix
/// of the increment plus the offset just past its last newline: a
/// half-written trailing line is NOT consumed yet, so the next tick sees it
/// whole (the writer appends line-by-line).
fn read_increment(file: &std::path::Path, offset: u64) -> Option<(String, u64)> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut handle = std::fs::File::open(file).ok()?;
    handle.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    handle.read_to_end(&mut buf).ok()?;
    let last_newline = buf.iter().rposition(|b| *b == b'\n')?;
    let complete = String::from_utf8_lossy(&buf[..last_newline]).to_string();
    Some((complete, offset + last_newline as u64 + 1))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn tail_filters_by_level_and_target() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        std::fs::create_dir_all(&dir).unwrap();
        let day = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 86_400;
        std::fs::write(
            dir.join(format!("tack-{day}.jsonl")),
            concat!(
                "{\"ts\":1,\"level\":\"DEBUG\",\"target\":\"a\",\"message\":\"d1\",\"fields\":{}}\n",
                "{\"ts\":2,\"level\":\"WARN\",\"target\":\"tack_app::x\",\"message\":\"w1\",\"fields\":{}}\n",
                "{\"ts\":3,\"level\":\"ERROR\",\"target\":\"other\",\"message\":\"e1\",\"fields\":{}}\n",
            ),
        )
        .unwrap();

        let options = LogOptions {
            tail: 10,
            level: None,
            target: None,
            follow: false,
        };
        let events = tail_events(tmp.path(), &options);
        assert_eq!(events.len(), 3);

        let options = LogOptions {
            tail: 10,
            level: Some("warn".into()),
            target: None,
            follow: false,
        };
        let events = tail_events(tmp.path(), &options);
        assert_eq!(events.len(), 2);

        let options = LogOptions {
            tail: 10,
            level: None,
            target: Some("tack_app".into()),
            follow: false,
        };
        let events = tail_events(tmp.path(), &options);
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("w1"));

        let options = LogOptions {
            tail: 1,
            level: None,
            target: None,
            follow: false,
        };
        assert_eq!(tail_events(tmp.path(), &options).len(), 1);
    }

    #[test]
    fn tail_fills_from_older_days() {
        // The newest day's file holds fewer events than --tail: older days
        // must fill the rest (newest-first order preserved).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        std::fs::create_dir_all(&dir).unwrap();
        let older: String = (1..=5)
            .map(|i| {
                format!(
                    "{{\"ts\":{i},\"level\":\"INFO\",\"target\":\"t\",\"message\":\"old{i}\",\"fields\":{{}}}}\n"
                )
            })
            .collect();
        std::fs::write(dir.join("tack-99.jsonl"), older).unwrap();
        let newer: String = (1..=2)
            .map(|i| {
                format!(
                    "{{\"ts\":1{i},\"level\":\"INFO\",\"target\":\"t\",\"message\":\"new{i}\",\"fields\":{{}}}}\n"
                )
            })
            .collect();
        std::fs::write(dir.join("tack-100.jsonl"), newer).unwrap();

        let options = LogOptions {
            tail: 10,
            level: None,
            target: None,
            follow: false,
        };
        let events = tail_events(tmp.path(), &options);
        assert_eq!(events.len(), 7, "older day must fill the tail: {events:?}");
        // Chronological order: old events before new.
        assert!(events[0].contains("old1"), "{events:?}");
        assert!(events[6].contains("new2"), "{events:?}");

        // A small tail takes the MOST RECENT events across the files.
        let options = LogOptions {
            tail: 3,
            level: None,
            target: None,
            follow: false,
        };
        let events = tail_events(tmp.path(), &options);
        assert_eq!(events.len(), 3);
        assert!(events[0].contains("old5"), "{events:?}");
        assert!(events[1].contains("new1"), "{events:?}");
        assert!(events[2].contains("new2"), "{events:?}");
    }

    #[test]
    fn tail_scans_huge_files_backwards() {
        // F12: the tail must come from the END of the file without slurping
        // hundreds of MB. Write a file larger than the initial read window
        // whose only matching events sit at the very start — window growth
        // must reach them — plus a dense tail served from the first window.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        std::fs::create_dir_all(&dir).unwrap();
        let day = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 86_400;
        let mut content = String::new();
        for i in 1..=3 {
            content.push_str(&format!(
                "{{\"ts\":{i},\"level\":\"ERROR\",\"target\":\"t\",\"message\":\"early-err{i}\",\"fields\":{{}}}}\n"
            ));
        }
        for i in 1..=6000 {
            content.push_str(&format!(
                "{{\"ts\":1{i:04},\"level\":\"INFO\",\"target\":\"t\",\"message\":\"noise line {i:05} padding padding\",\"fields\":{{}}}}\n"
            ));
        }
        assert!(
            content.len() > super::TAIL_WINDOW_BYTES as usize,
            "fixture must exceed the initial window"
        );
        std::fs::write(dir.join(format!("tack-{day}.jsonl")), content).unwrap();

        // Dense tail: the last events come from the final window only.
        let options = LogOptions {
            tail: 5,
            level: None,
            target: None,
            follow: false,
        };
        let events = tail_events(tmp.path(), &options);
        assert_eq!(events.len(), 5);
        assert!(events[4].contains("noise line 06000"), "{events:?}");

        // Sparse matches: only the first lines qualify — the window must
        // grow to the whole file instead of returning an empty tail.
        let options = LogOptions {
            tail: 5,
            level: Some("error".into()),
            target: None,
            follow: false,
        };
        let events = tail_events(tmp.path(), &options);
        assert_eq!(events.len(), 3, "window must grow to EOF=0: {events:?}");
        assert!(events[0].contains("early-err1"), "{events:?}");
    }

    #[test]
    fn follow_increment_reads_only_appended_complete_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("log.jsonl");
        std::fs::write(&file, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let offset = std::fs::metadata(&file).unwrap().len();
        // Append one complete line + a half-written one.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        write!(f, "{{\"c\":3}}\n{{\"d\":").unwrap();
        let (segment, consumed) = super::read_increment(&file, offset).unwrap();
        assert_eq!(segment, "{\"c\":3}");
        assert_eq!(consumed, offset + "{\"c\":3}\n".len() as u64);
        // The partial line is consumed by the NEXT increment, once complete.
        writeln!(f, "4}}").unwrap();
        let (segment, _) = super::read_increment(&file, consumed).unwrap();
        assert_eq!(segment, "{\"d\":4}");
    }
}
