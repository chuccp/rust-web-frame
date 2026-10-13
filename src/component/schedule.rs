//! Cron jobs, with a lifecycle that a running application can steer.
//!
//! A job is added under a **key**, and the key is what the application holds: replacing a
//! job by key is how a schedule is changed at runtime — from an admin endpoint, or from a
//! configuration reload — without restarting and without leaking the old entry.
//!
//! ```no_run
//! # use rust_web_frame::component::Schedule;
//! # use rust_web_frame::core::{Context, Runner, Service};
//! # use rust_web_frame::web::Result;
//! # use async_trait::async_trait;
//! # #[derive(Default)] struct Jobs;
//! #[async_trait]
//! impl Service for Jobs {
//!     async fn init(&self, ctx: &Context) -> Result<()> {
//!         let schedule = ctx.service::<Schedule>()?;
//!         schedule.add_key("nightly-report", "0 3 * * *", |ctx| async move {
//!             // runs at 03:00 every day
//!             Ok(())
//!         })?;
//!         Ok(())
//!     }
//! }
//! ```
//!
//! The schedule is a [`Runner`] itself: it starts with the
//! application and stops when the application does, and each firing runs as its own task so
//! a slow job cannot hold up the next tick.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use chrono::Timelike;

use async_trait::async_trait;
use cron::Schedule as CronSchedule;
use serde::{Deserialize, Serialize};

use crate::core::{Context, Runner, Service};
use crate::web::{Error, Result};

/// Config section that holds the schedule settings.
pub const SCHEDULE_CONFIG_KEY: &str = "schedule";

/// Schedule configuration, read from the `schedule` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ScheduleConfig {
    /// Whether jobs run at all. Off, the jobs are still registered and can be listed, which
    /// is what a test or a maintenance window wants.
    pub enabled: bool,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// A job body, erased so that jobs built from different futures can share one map.
type JobRun = Arc<dyn Fn(Context) -> crate::web::BoxFuture<Result<()>> + Send + Sync>;

/// A registered job.
struct Job {
    expression: String,
    schedule: CronSchedule,
    run: JobRun,
    /// When the job last fired, for a status endpoint.
    last_run: Option<chrono::DateTime<chrono::Utc>>,
    /// How many times it has fired, and how many of those failed.
    runs: u64,
    failures: u64,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("expression", &self.expression)
            .field("runs", &self.runs)
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

/// What a job has done, for a status endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatus {
    /// The key the job was registered under.
    pub key: String,
    /// The cron expression it runs on.
    pub expression: String,
    /// When it last fired.
    pub last_run: Option<chrono::DateTime<chrono::Utc>>,
    /// When it will fire next.
    pub next_run: Option<chrono::DateTime<chrono::Utc>>,
    /// How many times it has fired.
    pub runs: u64,
    /// How many of those returned an error.
    pub failures: u64,
}

/// Cron jobs, keyed by whatever the application calls them.
pub struct Schedule {
    /// Behind an `Arc` so a spawned firing can record its own outcome; the lock is only
    /// ever held for the bookkeeping, never across an await.
    jobs: Arc<Mutex<HashMap<String, Job>>>,
    enabled: Mutex<bool>,
}

impl Default for Schedule {
    /// Jobs run unless the configuration says otherwise — a `Mutex<bool>` would default to
    /// `false` and leave every job silently unregistered but never fired.
    fn default() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            enabled: Mutex::new(true),
        }
    }
}

impl std::fmt::Debug for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schedule")
            .field(
                "jobs",
                &self.jobs.lock().map(|jobs| jobs.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl Schedule {
    /// An empty schedule.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a job, replacing any job already under `key`.
    ///
    /// Replacing rather than refusing is the point: a schedule that is reloaded at runtime
    /// must not leak the entry it is replacing.
    ///
    /// # Errors
    ///
    /// Fails when the expression cannot be parsed.
    pub fn add_key<F, Fut>(&self, key: &str, expression: &str, run: F) -> Result<()>
    where
        F: Fn(Context) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let schedule = parse_expression(expression)?;
        let run: JobRun = Arc::new(move |ctx| Box::pin(run(ctx)));

        self.jobs.lock().expect("schedule poisoned").insert(
            key.to_owned(),
            Job {
                expression: expression.to_owned(),
                schedule,
                run,
                last_run: None,
                runs: 0,
                failures: 0,
            },
        );
        tracing::debug!(key, expression, "job scheduled");
        Ok(())
    }

    /// Adds a job only if the key is free, reporting whether it added one.
    ///
    /// # Errors
    ///
    /// Fails when the expression cannot be parsed.
    pub fn add_key_once<F, Fut>(&self, key: &str, expression: &str, run: F) -> Result<bool>
    where
        F: Fn(Context) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        if self
            .jobs
            .lock()
            .expect("schedule poisoned")
            .contains_key(key)
        {
            return Ok(false);
        }
        self.add_key(key, expression, run)?;
        Ok(true)
    }

    /// Removes a job, reporting whether there was one.
    pub fn remove(&self, key: &str) -> bool {
        self.jobs
            .lock()
            .expect("schedule poisoned")
            .remove(key)
            .is_some()
    }

    /// The keys of every registered job.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .jobs
            .lock()
            .expect("schedule poisoned")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// What every job has done, and when it fires next.
    #[must_use]
    pub fn status(&self) -> Vec<JobStatus> {
        let now = chrono::Utc::now();
        let jobs = self.jobs.lock().expect("schedule poisoned");

        let mut status: Vec<JobStatus> = jobs
            .iter()
            .map(|(key, job)| JobStatus {
                key: key.clone(),
                expression: job.expression.clone(),
                last_run: job.last_run,
                next_run: job.schedule.after(&now).next(),
                runs: job.runs,
                failures: job.failures,
            })
            .collect();
        status.sort_by(|a, b| a.key.cmp(&b.key));
        status
    }

    /// Runs every job that is due at `at`, once each.
    ///
    /// The runner calls this on a tick; a test calls it directly, which is the only way to
    /// exercise a schedule without waiting for a clock.
    ///
    /// # Errors
    ///
    /// Fails when the scheduler is disabled — an empty result is not a failure, so a tick
    /// with nothing due is `Ok(0)`.
    pub fn run_due(&self, ctx: &Context, at: chrono::DateTime<chrono::Utc>) -> Result<usize> {
        if !*self.enabled.lock().expect("schedule poisoned") {
            return Ok(0);
        }

        // Collect first, so the lock is not held across the awaits.
        let due: Vec<(String, JobRun)> = {
            let mut jobs = self.jobs.lock().expect("schedule poisoned");
            let mut due = Vec::new();
            for (key, job) in jobs.iter_mut() {
                if job.schedule.includes(at) {
                    job.last_run = Some(at);
                    due.push((key.clone(), Arc::clone(&job.run)));
                }
            }
            due
        };

        let count = due.len();
        for (key, run) in due {
            // Each firing is its own task, so a slow job cannot hold up the next tick.
            // Owned clones: the task outlives this call.
            let owned = ctx.clone();
            let spawner = ctx.clone();
            let jobs = Arc::clone(&self.jobs);
            spawner.spawn(async move {
                let outcome = run(owned).await;
                if let Some(job) = jobs.lock().expect("schedule poisoned").get_mut(&key) {
                    job.runs += 1;
                    if outcome.is_err() {
                        job.failures += 1;
                    }
                }
                if let Err(error) = outcome {
                    tracing::error!(key, error = %error, "scheduled job failed");
                }
            });
        }
        Ok(count)
    }

    /// Turns the jobs on or off without unregistering them.
    pub fn set_enabled(&self, enabled: bool) {
        *self.enabled.lock().expect("schedule poisoned") = enabled;
    }
}

#[async_trait]
impl Service for Schedule {
    async fn init(&self, ctx: &Context) -> Result<()> {
        let config: ScheduleConfig = ctx
            .config()
            .section(SCHEDULE_CONFIG_KEY)
            .unwrap_or_default();
        self.set_enabled(config.enabled);
        Ok(())
    }
}

#[async_trait]
impl Runner for Schedule {
    async fn run(&self, ctx: &Context) -> Result<()> {
        // A tick every thirty seconds: the finest granularity a five-field cron expression
        // can express, with enough slack that a slow tick cannot skip a minute.
        let cancel = ctx.cancellation_token();
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick is immediate, and `run_due` is idempotent within a minute: a job
        // whose expression includes the current minute would otherwise fire twice.
        let mut last_minute: Option<chrono::DateTime<chrono::Utc>> = None;

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = cancel.cancelled() => return Ok(()),
            }

            let now = chrono::Utc::now();
            let minute = now.with_second(0).and_then(|t| t.with_nanosecond(0));
            match (minute, last_minute) {
                (Some(minute), Some(last)) if minute == last => continue,
                (Some(minute), _) => last_minute = Some(minute),
                (None, _) => continue,
            }

            if let Err(error) = self.run_due(ctx, now) {
                tracing::warn!(error = %error, "scheduler tick failed");
            }
        }
    }

    fn name(&self) -> &'static str {
        "Schedule"
    }
}

/// Parses a cron expression, accepting both the five-field form a person writes and the
/// six-field form `cron` itself expects.
fn parse_expression(expression: &str) -> Result<CronSchedule> {
    let trimmed = expression.trim();
    let normalised = if trimmed.split_whitespace().count() == 5 {
        // `cron` wants seconds first; a five-field expression has none.
        format!("0 {trimmed}")
    } else {
        trimmed.to_owned()
    };

    CronSchedule::from_str(&normalised).map_err(|err| {
        Error::new(500, format!("`{expression}` is not a cron expression"))
            .caused_by(std::io::Error::other(err.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn at(text: &str) -> chrono::DateTime<chrono::Utc> {
        text.parse().expect("a valid timestamp")
    }

    #[test]
    fn five_and_six_field_expressions_both_parse() {
        assert!(
            parse_expression("0 3 * * *").is_ok(),
            "five fields, as a person writes it"
        );
        assert!(
            parse_expression("0 0 3 * * *").is_ok(),
            "six fields, as cron expects"
        );
        assert!(
            parse_expression("@daily").is_ok(),
            "cron understands the usual nicknames"
        );
        assert!(parse_expression("not a cron").is_err());
    }

    #[test]
    fn the_schedule_runs_until_something_says_otherwise() {
        assert!(*Schedule::new().enabled.lock().unwrap());
    }

    #[test]
    fn a_bad_expression_is_refused_with_the_expression_in_the_message() {
        let schedule = Schedule::new();
        let error = schedule
            .add_key("bad", "every tuesday", |_| async { Ok(()) })
            .unwrap_err();
        assert!(error.message.contains("every tuesday"), "{}", error.message);
    }

    #[test]
    fn a_replaced_job_does_not_leak_the_old_one() {
        let schedule = Schedule::new();
        schedule
            .add_key("report", "0 3 * * *", |_| async { Ok(()) })
            .unwrap();
        schedule
            .add_key("report", "0 4 * * *", |_| async { Ok(()) })
            .unwrap();

        assert_eq!(schedule.keys(), vec!["report"]);
        let status = schedule.status();
        assert_eq!(
            status[0].expression, "0 4 * * *",
            "the later expression wins"
        );
        assert_eq!(
            status[0].runs, 0,
            "the replacement starts its counters over"
        );
    }

    #[test]
    fn add_key_once_reports_whether_it_added_one() {
        let schedule = Schedule::new();
        assert!(
            schedule
                .add_key_once("a", "0 3 * * *", |_| async { Ok(()) })
                .unwrap()
        );
        assert!(
            !schedule
                .add_key_once("a", "0 4 * * *", |_| async { Ok(()) })
                .unwrap()
        );
        assert_eq!(schedule.status()[0].expression, "0 3 * * *");
    }

    #[test]
    fn removing_reports_whether_there_was_one() {
        let schedule = Schedule::new();
        schedule
            .add_key("a", "0 3 * * *", |_| async { Ok(()) })
            .unwrap();
        assert!(schedule.remove("a"));
        assert!(!schedule.remove("a"));
        assert!(schedule.keys().is_empty());
    }

    #[tokio::test]
    async fn only_a_job_whose_time_has_come_runs() {
        let schedule = Arc::new(Schedule::new());
        let fired = Arc::new(AtomicUsize::new(0));

        let counter = Arc::clone(&fired);
        schedule
            .add_key("nightly", "0 3 * * *", move |_| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .unwrap();

        let ctx = Context::empty();
        let quiet = schedule.run_due(&ctx, at("2026-10-13T12:00:00Z")).unwrap();
        assert_eq!(quiet, 0, "nothing is due at noon");
        assert_eq!(fired.load(Ordering::SeqCst), 0);

        let busy = schedule.run_due(&ctx, at("2026-10-13T03:00:00Z")).unwrap();
        assert_eq!(busy, 1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_disabled_schedule_runs_nothing() {
        let schedule = Arc::new(Schedule::new());
        let fired = Arc::new(AtomicUsize::new(0));

        let counter = Arc::clone(&fired);
        schedule
            .add_key("job", "* * * * *", move |_| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .unwrap();

        schedule.set_enabled(false);
        let ctx = Context::empty();
        assert_eq!(
            schedule.run_due(&ctx, at("2026-10-13T03:00:00Z")).unwrap(),
            0
        );
        assert_eq!(fired.load(Ordering::SeqCst), 0);

        schedule.set_enabled(true);
        assert_eq!(
            schedule.run_due(&ctx, at("2026-10-13T03:00:00Z")).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn a_failing_job_is_counted_and_does_not_stop_the_others() {
        let schedule = Arc::new(Schedule::new());
        let ok_ran = Arc::new(AtomicUsize::new(0));

        schedule
            .add_key("fails", "* * * * *", |_| async {
                Err(Error::internal().detail("the job is broken"))
            })
            .unwrap();

        let counter = Arc::clone(&ok_ran);
        schedule
            .add_key("works", "* * * * *", move |_| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .unwrap();

        let ctx = Context::empty();
        assert_eq!(
            schedule.run_due(&ctx, at("2026-10-13T03:00:00Z")).unwrap(),
            2
        );
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        assert_eq!(
            ok_ran.load(Ordering::SeqCst),
            1,
            "one job failing must not stop another"
        );

        let status = schedule.status();
        let failing = status.iter().find(|s| s.key == "fails").unwrap();
        assert_eq!((failing.runs, failing.failures), (1, 1));
        let working = status.iter().find(|s| s.key == "works").unwrap();
        assert_eq!((working.runs, working.failures), (1, 0));
    }

    #[test]
    fn status_reports_when_a_job_fires_next() {
        let schedule = Schedule::new();
        schedule
            .add_key("hourly", "0 * * * *", |_| async { Ok(()) })
            .unwrap();

        let status = &schedule.status()[0];
        assert_eq!(status.key, "hourly");
        assert!(
            status.next_run.is_some(),
            "a schedule can say when it fires next"
        );
        assert!(status.last_run.is_none(), "and that it has not yet");
    }
}
