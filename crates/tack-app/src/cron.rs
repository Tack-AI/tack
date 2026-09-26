//! Scheduled prompts (cron jobs). Jobs persist in
//! `~/.tack/agent/cron.json` and fire inside the TUI session: when the agent
//! is idle the prompt starts a run; mid-run it is queued as steering.
//!
//! Schedule formats:
//!   "every 10m" / "every 1h30m" / "every 45s"  — fixed interval
//!   "*/5 * * * *"  — standard 5-field cron (minute hour dom month dow)
//!
//! Missed runs while the app was closed fire once at startup (catch-up),
//! then resume the regular schedule.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub schedule: String,
    pub prompt: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Unix millis of the last firing (None = never fired).
    #[serde(default)]
    pub last_fired: Option<u64>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Schedule {
    /// Fixed interval in milliseconds.
    Every(u64),
    /// 5-field cron expression (validated at parse time).
    Cron(String),
}

/// Parse a schedule string. Accepts "every <dur>" or a 5-field cron expr.
fn parse_schedule(input: &str) -> Result<Schedule, String> {
    let input = input.trim();
    if let Some(rest) = input.strip_prefix("every ") {
        return parse_duration(rest.trim()).map(Schedule::Every);
    }
    // Validate as cron. The cron crate's Schedule is seconds-first
    // ("sec min hour dom mon dow [year]"); classic 5-field expressions get a
    // "0 " seconds prefix.
    let fields = input.split_whitespace().count();
    if !(5..=7).contains(&fields) {
        return Err(format!(
            "invalid schedule {input:?}: use \"every 10m\" or a 5-field cron expression"
        ));
    }
    let normalized = if fields == 5 {
        format!("0 {input}")
    } else {
        input.to_string()
    };
    cron::Schedule::from_str(&normalized)
        .map_err(|e| format!("invalid cron expression {input:?}: {e}"))?;
    Ok(Schedule::Cron(normalized))
}

/// "1h30m" / "10m" / "45s" / "2d" → milliseconds.
fn parse_duration(input: &str) -> Result<u64, String> {
    let mut total_ms = 0u64;
    let mut num = String::new();
    let mut parts = 0;
    for c in input.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let value: u64 = num
            .parse()
            .map_err(|_| format!("invalid duration {input:?}"))?;
        num.clear();
        let unit_ms = match c {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => {
                return Err(format!(
                    "invalid duration unit {c:?} in {input:?} (s/m/h/d)"
                ));
            }
        };
        // Absurd values must error, not overflow (panic in debug, wrap in
        // release) the multiplication/accumulation.
        total_ms = value
            .checked_mul(unit_ms)
            .and_then(|part| total_ms.checked_add(part))
            .ok_or_else(|| format!("duration {input:?} is too large"))?;
        parts += 1;
    }
    if !num.is_empty() {
        return Err(format!("duration {input:?} ends without a unit (s/m/h/d)"));
    }
    if parts == 0 || total_ms < 10_000 {
        return Err("interval must be at least 10s".to_string());
    }
    Ok(total_ms)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Next fire time (unix millis) for a schedule given last_fired/now.
fn next_fire(schedule: &Schedule, last_fired: Option<u64>, now: u64) -> u64 {
    match schedule {
        Schedule::Every(interval) => match last_fired {
            // Missed runs collapse to "fire at the next tick" (catch-up once).
            Some(last) => {
                let next = last + interval;
                if next <= now { now } else { next }
            }
            None => now + interval,
        },
        Schedule::Cron(expr) => {
            let Ok(schedule) = cron::Schedule::from_str(expr) else {
                return u64::MAX;
            };
            let Some(dt) = chrono::DateTime::from_timestamp_millis(now as i64)
                .map(|dt| dt.with_timezone(&chrono::Local))
            else {
                return u64::MAX;
            };
            // Catch-up (once, like interval jobs): the first occurrence
            // after last_fired falling at or before now was missed while
            // the app was closed — fire immediately. last_fired = now
            // after firing, so this cannot retrigger until the schedule
            // genuinely advances past now. Never-fired jobs have nothing
            // to catch up (same as Schedule::Every's None arm).
            if let Some(last) = last_fired
                && let Some(last_dt) = chrono::DateTime::from_timestamp_millis(last as i64)
                    .map(|dt| dt.with_timezone(&chrono::Local))
                && let Some(missed) = schedule.after(&last_dt).next()
                && missed.timestamp_millis() as u64 <= now
            {
                return now;
            }
            schedule
                .after(&dt)
                .next()
                .map(|next| next.timestamp_millis() as u64)
                .unwrap_or(u64::MAX)
        }
    }
}

/// Persistent job store backed by `<agent dir>/cron.json`.
pub struct CronStore {
    path: PathBuf,
    pub jobs: Vec<CronJob>,
}

impl std::fmt::Debug for CronStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CronStore")
            .field("jobs", &self.jobs.len())
            .finish()
    }
}

impl CronStore {
    pub fn load(agent_dir: &Path) -> Self {
        let path = agent_dir.join("cron.json");
        let jobs = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|v| serde_json::from_value::<Vec<CronJob>>(v["jobs"].clone()).ok())
            .unwrap_or_default();
        CronStore { path, jobs }
    }

    fn save(&self) -> Result<(), String> {
        let value = json!({ "jobs": self.jobs });
        crate::atomic_write::atomic_write(
            &self.path,
            &serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }

    /// Add a job; returns its id.
    pub fn add(&mut self, schedule: &str, prompt: &str) -> Result<String, String> {
        parse_schedule(schedule)?; // validate
        let id: String = rand::random::<[u8; 4]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.jobs.push(CronJob {
            id: id.clone(),
            schedule: schedule.to_string(),
            prompt: prompt.to_string(),
            enabled: true,
            last_fired: None,
        });
        self.save()?;
        Ok(id)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.jobs.len();
        self.jobs.retain(|j| j.id != id);
        if self.jobs.len() != before {
            let _ = self.save();
            true
        } else {
            false
        }
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
            job.enabled = enabled;
            let _ = self.save();
            true
        } else {
            false
        }
    }

    /// Drain jobs whose fire time has come; marks them fired and persists.
    pub fn take_due(&mut self) -> Vec<CronJob> {
        let now = now_millis();
        let mut due = Vec::new();
        for job in &mut self.jobs {
            if !job.enabled {
                continue;
            }
            let Ok(schedule) = parse_schedule(&job.schedule) else {
                continue;
            };
            if next_fire(&schedule, job.last_fired, now) <= now {
                job.last_fired = Some(now);
                due.push(job.clone());
            }
        }
        if !due.is_empty() {
            let _ = self.save();
        }
        due
    }

    /// Human-readable next-fire description for the list view.
    pub fn describe_next_fire(job: &CronJob) -> String {
        let Ok(schedule) = parse_schedule(&job.schedule) else {
            return "invalid schedule".to_string();
        };
        let next = next_fire(&schedule, job.last_fired, now_millis());
        if next == u64::MAX {
            return "never".to_string();
        }
        let secs = next.saturating_sub(now_millis()) / 1000;
        if secs < 60 {
            format!("in {secs}s")
        } else if secs < 3600 {
            format!("in {}m{}s", secs / 60, secs % 60)
        } else {
            format!("in {}h{}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parse_every_duration() {
        assert_eq!(
            parse_schedule("every 10m").unwrap(),
            Schedule::Every(600_000)
        );
        assert_eq!(
            parse_schedule("every 1h30m").unwrap(),
            Schedule::Every(5_400_000)
        );
        assert!(parse_schedule("every 5s").is_err()); // below the 10s floor
        assert!(parse_schedule("every 10").is_err());
        // Huge values overflowed u64 (debug panic / release wrap) — now an error.
        assert!(parse_schedule("every 999999999999999d").is_err());
        assert!(parse_schedule("every 1000000000000000000h30m").is_err());
    }

    #[test]
    fn parse_cron_expression() {
        assert!(matches!(
            parse_schedule("*/5 * * * *").unwrap(),
            Schedule::Cron(_)
        ));
        assert!(matches!(
            parse_schedule("0 9 * * 1-5").unwrap(),
            Schedule::Cron(_)
        ));
        assert!(parse_schedule("* * *").is_err());
        assert!(parse_schedule("not a schedule").is_err());
    }

    #[test]
    fn next_fire_interval_catches_up_once() {
        let now = 1_000_000u64;
        // Never fired: interval from now.
        assert_eq!(next_fire(&Schedule::Every(60_000), None, now), now + 60_000);
        // Fired long ago: fire immediately (catch-up).
        assert_eq!(next_fire(&Schedule::Every(60_000), Some(100), now), now);
        // Fired recently: next at last+interval.
        assert_eq!(
            next_fire(&Schedule::Every(60_000), Some(now - 10_000), now),
            now + 50_000
        );
    }

    #[test]
    fn cron_next_fire_is_in_the_future() {
        let now = now_millis();
        let next = next_fire(&parse_schedule("* * * * *").unwrap(), None, now);
        assert!(next > now && next <= now + 61_000, "next={next} now={now}");
    }

    #[test]
    fn cron_missed_run_catches_up_once() {
        // Every-minute schedule; last fired 5 minutes ago → four minute
        // boundaries passed without firing (app closed): fire now, once.
        let now = now_millis();
        let schedule = parse_schedule("* * * * *").unwrap();
        assert_eq!(next_fire(&schedule, Some(now - 300_000), now), now);
        // After the catch-up firing the schedule resumes: the next fire
        // is the upcoming minute boundary, not "now" again.
        let next = next_fire(&schedule, Some(now), now);
        assert!(next > now && next <= now + 61_000, "next={next} now={now}");
        // Never-fired jobs don't catch up (nothing was missed).
        assert!(next_fire(&schedule, None, now) > now);
    }

    #[test]
    fn store_roundtrip_and_due_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = CronStore::load(tmp.path());
        let id = store.add("every 10m", "check the thing").unwrap();
        assert!(store.take_due().is_empty()); // just created, not due

        // Force due by backdating last_fired.
        store.jobs[0].last_fired = Some(now_millis() - 3_600_000);
        let due = store.take_due();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, id);
        assert!(store.take_due().is_empty()); // drained

        // Persistence.
        let reloaded = CronStore::load(tmp.path());
        assert_eq!(reloaded.jobs.len(), 1);
        assert_eq!(reloaded.jobs[0].prompt, "check the thing");

        // Pause/resume/remove.
        assert!(store.set_enabled(&id, false));
        store.jobs[0].last_fired = Some(0);
        assert!(store.take_due().is_empty()); // paused jobs don't fire
        assert!(store.remove(&id));
        assert!(store.jobs.is_empty());
    }
}
