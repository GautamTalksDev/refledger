//! Poll scheduler — the only component that reads the wall clock.
//!
//! M1 policy (GitHub Actions; one sweep per scheduled run):
//! - one tier: every poll group once per 300s (cron every 5 minutes)
//! - concurrency 4 (still used by the interactive scheduler)
//! - global secondary-points governor at 300/minute (one third of the
//!   documented 900 ceiling), applied to every request
//! - refusals pause globally; due groups get one Skipped observation each
//! - `once` runs also cap requests (300) and new peels (120) per job

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use thiserror::Error;
use time::{Duration, OffsetDateTime};

use refledger_log::normalize_to_utc_millis;

use crate::observation::{Method, Observation, ObservationError, Outcome, SkipReason};
use crate::population::PollGroup;

/// Poll interval for M1 on GitHub Actions (one sweep per scheduled run).
pub const M1_INTERVAL: Duration = Duration::seconds(300);
/// Hard cap on GitHub API requests in a single `once` run (stable PAT budget).
pub const MAX_REQUESTS_PER_RUN: u32 = 300;
/// Cap on first-seen object peels per run so large tag sets warm across runs.
pub const MAX_NEW_PEELS_PER_RUN: u32 = 120;
/// Confirmation re-poll delay after a detected movement (same run).
pub const CONFIRM_DELAY: Duration = Duration::seconds(60);
/// Global secondary-points ceiling used by the governor.
pub const M1_POINTS_PER_MINUTE: u32 = 300;
/// Documented REST secondary ceiling (for commentary / tests).
pub const DOCUMENTED_SECONDARY_RPM: u32 = 900;
/// In-flight request cap for M1.
pub const M1_CONCURRENCY: u32 = 4;
/// Minimum pause when Retry-After is absent.
pub const MIN_REFUSAL_PAUSE: Duration = Duration::seconds(60);
/// Half-rate recovery window after a refusal.
pub const RECOVERY_WINDOW: Duration = Duration::minutes(10);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SchedulerError {
    #[error("observation: {0}")]
    Observation(#[from] ObservationError),
    #[error("{0}")]
    Message(String),
}

/// Wall clock. Production uses [`SystemClock`]; tests inject [`FakeClock`].
pub trait Clock: Send {
    fn now(&self) -> OffsetDateTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        normalize_to_utc_millis(OffsetDateTime::now_utc())
    }
}

#[derive(Debug)]
pub struct FakeClock {
    now: std::sync::Mutex<OffsetDateTime>,
}

impl FakeClock {
    pub fn new(start: OffsetDateTime) -> Self {
        Self {
            now: std::sync::Mutex::new(start),
        }
    }

    pub fn set(&self, t: OffsetDateTime) {
        *self.now.lock().expect("clock") = t;
    }

    pub fn advance(&self, d: Duration) {
        let mut g = self.now.lock().expect("clock");
        *g += d;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> OffsetDateTime {
        *self.now.lock().expect("clock")
    }
}

/// Sliding one-minute window of secondary points. One governor for every
/// request class (ref listing, dereference, compare, action.yml).
#[derive(Debug, Clone)]
pub struct PointsGovernor {
    cap: u32,
    /// Timestamps of charged points (one entry per point).
    stamps: VecDeque<OffsetDateTime>,
}

impl PointsGovernor {
    pub fn new(cap: u32) -> Self {
        Self {
            cap,
            stamps: VecDeque::new(),
        }
    }

    pub fn m1() -> Self {
        Self::new(M1_POINTS_PER_MINUTE)
    }

    pub fn cap(&self) -> u32 {
        self.cap
    }

    fn prune(&mut self, now: OffsetDateTime) {
        let window_start = now - Duration::seconds(60);
        while self.stamps.front().is_some_and(|t| *t < window_start) {
            self.stamps.pop_front();
        }
    }

    pub fn used(&mut self, now: OffsetDateTime) -> u32 {
        self.prune(now);
        self.stamps.len() as u32
    }

    /// Charge one secondary point. Returns false if the cap would be exceeded.
    pub fn try_charge(&mut self, now: OffsetDateTime) -> bool {
        self.prune(now);
        if self.stamps.len() as u32 >= self.cap {
            return false;
        }
        self.stamps.push_back(now);
        true
    }

    /// Wait until a point is available, or `None` if already available.
    pub fn delay_until_available(&mut self, now: OffsetDateTime) -> Option<Duration> {
        self.prune(now);
        if (self.stamps.len() as u32) < self.cap {
            return None;
        }
        let oldest = *self.stamps.front()?;
        let ready = oldest + Duration::seconds(60);
        if ready <= now {
            None
        } else {
            Some(ready - now)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    RefListing,
    Dereference,
    Compare,
    ActionYml,
}

/// Outcome of one scheduler step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Dispatch a poll for this group at `due`.
    Dispatch {
        group: String,
        due: OffsetDateTime,
    },
    /// Wait until `until` (governor, pause, or next slot).
    Wait {
        until: OffsetDateTime,
    },
    /// Emit a skipped observation (refusal pause, lag, shutdown).
    Skip {
        group: String,
        reason: SkipReason,
        at: OffsetDateTime,
    },
    Idle,
}

#[derive(Debug, Clone)]
struct GroupState {
    repo: String,
    next_due: OffsetDateTime,
    /// Last time a sweep was successfully started for this group.
    last_started: Option<OffsetDateTime>,
}

/// M1 single-tier scheduler.
pub struct Scheduler<C: Clock> {
    clock: C,
    interval: Duration,
    concurrency: u32,
    governor: PointsGovernor,
    groups: BTreeMap<String, GroupState>,
    /// Deterministic jitter seeds so tests don't need RNG.
    jitter_seed: u64,
    in_flight: u32,
    pause_until: Option<OffsetDateTime>,
    /// When the current refusal recovery started (half-rate → full over 10 min).
    recovery_start: Option<OffsetDateTime>,
    shutting_down: bool,
    /// Groups that were due during a pause and already got their Skipped obs.
    skipped_this_pause: BTreeSet<String>,
}

impl<C: Clock> Scheduler<C> {
    pub fn m1(clock: C, poll_groups: &[PollGroup], epoch: OffsetDateTime) -> Self {
        let epoch = normalize_to_utc_millis(epoch);
        let mut groups = BTreeMap::new();
        let n = poll_groups.len().max(1) as u64;
        for (i, g) in poll_groups.iter().enumerate() {
            // Spread first dues evenly across the interval — never a burst at :00.
            let offset = Duration::milliseconds(
                ((i as u64) * (M1_INTERVAL.whole_milliseconds() as u64) / n) as i64,
            );
            groups.insert(
                g.repo.clone(),
                GroupState {
                    repo: g.repo.clone(),
                    next_due: epoch + offset,
                    last_started: None,
                },
            );
        }
        Self {
            clock,
            interval: M1_INTERVAL,
            concurrency: M1_CONCURRENCY,
            governor: PointsGovernor::m1(),
            groups,
            jitter_seed: 0xC0FFEE,
            in_flight: 0,
            pause_until: None,
            recovery_start: None,
            shutting_down: false,
            skipped_this_pause: BTreeSet::new(),
        }
    }

    pub fn clock(&self) -> &C {
        &self.clock
    }

    pub fn governor_mut(&mut self) -> &mut PointsGovernor {
        &mut self.governor
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight
    }

    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    /// Charge the shared governor for any request (listing, peel, compare, yml).
    pub fn charge(&mut self, _kind: RequestKind) -> bool {
        let now = normalize_to_utc_millis(self.clock.now());
        self.effective_try_charge(now)
    }

    fn effective_cap(&self, now: OffsetDateTime) -> u32 {
        let Some(start) = self.recovery_start else {
            return self.governor.cap();
        };
        let elapsed = now - start;
        if elapsed >= RECOVERY_WINDOW {
            return self.governor.cap();
        }
        // Half rate at t=0, linear to full at RECOVERY_WINDOW.
        let half = self.governor.cap() / 2;
        let span = self.governor.cap() - half;
        let frac =
            elapsed.whole_milliseconds() as f64 / RECOVERY_WINDOW.whole_milliseconds() as f64;
        half + (span as f64 * frac).round() as u32
    }

    fn effective_try_charge(&mut self, now: OffsetDateTime) -> bool {
        let cap = self.effective_cap(now);
        self.governor.prune(now);
        if self.governor.stamps.len() as u32 >= cap {
            return false;
        }
        self.governor.stamps.push_back(now);
        true
    }

    /// Record a 403/429. One global pause; due groups each get one Skipped.
    pub fn on_refusal(
        &mut self,
        retry_after: Option<Duration>,
    ) -> Result<Vec<Observation>, SchedulerError> {
        let now = normalize_to_utc_millis(self.clock.now());
        let pause = retry_after
            .filter(|d| *d >= MIN_REFUSAL_PAUSE)
            .unwrap_or(MIN_REFUSAL_PAUSE);
        let until = now + pause;
        self.pause_until = Some(until);
        self.recovery_start = Some(until);
        self.skipped_this_pause.clear();

        let mut out = Vec::new();
        for g in self.groups.values() {
            if g.next_due <= until {
                self.skipped_this_pause.insert(g.repo.clone());
                out.push(skipped_obs(
                    &g.repo,
                    now,
                    SkipReason::SecondaryLimitBackoff,
                )?);
            }
        }
        Ok(out)
    }

    pub fn complete_request(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// Advance the schedule. Pure given the clock and prior state.
    pub fn poll(&mut self) -> Result<Step, SchedulerError> {
        let now = normalize_to_utc_millis(self.clock.now());

        if self.shutting_down {
            // Drain: skip every group that still has a due we never started.
            if let Some(repo) = self
                .groups
                .values()
                .find(|g| g.next_due <= now)
                .map(|g| g.repo.clone())
            {
                if let Some(g) = self.groups.get_mut(&repo) {
                    g.next_due = now + self.interval;
                }
                return Ok(Step::Skip {
                    group: repo,
                    reason: SkipReason::ShutdownMidSweep,
                    at: now,
                });
            }
            return Ok(Step::Idle);
        }

        if let Some(until) = self.pause_until {
            if now < until {
                // Emit Skipped for any group that becomes due during the pause
                // and has not yet been recorded for this pause.
                if let Some(repo) = self
                    .groups
                    .values()
                    .filter(|g| g.next_due <= now && !self.skipped_this_pause.contains(&g.repo))
                    .map(|g| g.repo.clone())
                    .next()
                {
                    self.skipped_this_pause.insert(repo.clone());
                    return Ok(Step::Skip {
                        group: repo,
                        reason: SkipReason::SecondaryLimitBackoff,
                        at: now,
                    });
                }
                return Ok(Step::Wait { until });
            }
            self.pause_until = None;
            self.skipped_this_pause.clear();
        }

        if let Some(start) = self.recovery_start {
            if now - start >= RECOVERY_WINDOW {
                self.recovery_start = None;
            }
        }

        // Liveness: miss > 2× interval → Skipped SchedulerLag, then reschedule.
        if let Some(repo) = self
            .groups
            .values()
            .filter(|g| now - g.next_due > self.interval * 2)
            .map(|g| g.repo.clone())
            .next()
        {
            let scheduled = self.groups.get(&repo).map(|g| g.next_due).unwrap_or(now);
            let jitter = self.slot_jitter(&repo);
            if let Some(g) = self.groups.get_mut(&repo) {
                g.next_due = now + jitter;
            }
            let scheduled_ts = crate::observation::Timestamp::from_offset_datetime(scheduled)
                .map_err(SchedulerError::Observation)?;
            let actual_ts = crate::observation::Timestamp::from_offset_datetime(now)
                .map_err(SchedulerError::Observation)?;
            return Ok(Step::Skip {
                group: repo,
                reason: SkipReason::SchedulerLag {
                    scheduled: scheduled_ts,
                    actual: actual_ts,
                },
                at: now,
            });
        }

        if self.in_flight >= self.concurrency {
            return Ok(Step::Wait {
                until: now + Duration::milliseconds(50),
            });
        }

        let due = self
            .groups
            .values()
            .filter(|g| g.next_due <= now)
            .min_by_key(|g| g.next_due)
            .map(|g| g.repo.clone());

        let Some(repo) = due else {
            let next = self
                .groups
                .values()
                .map(|g| g.next_due)
                .min()
                .unwrap_or(now + self.interval);
            return Ok(Step::Wait { until: next });
        };

        if !self.effective_try_charge(now) {
            let wait = self
                .governor
                .delay_until_available(now)
                .unwrap_or(Duration::milliseconds(100));
            return Ok(Step::Wait { until: now + wait });
        }

        let due_at = self.groups.get(&repo).map(|g| g.next_due).unwrap_or(now);
        let jitter = self.slot_jitter(&repo);
        if let Some(g) = self.groups.get_mut(&repo) {
            g.last_started = Some(now);
            g.next_due = now + self.interval + jitter;
        }
        self.in_flight += 1;
        Ok(Step::Dispatch {
            group: repo,
            due: due_at,
        })
    }

    fn slot_jitter(&mut self, repo: &str) -> Duration {
        // Deterministic ±5% of interval from seed + repo bytes. Never clusters at :00.
        self.jitter_seed = self
            .jitter_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(repo.bytes().map(|b| b as u64).sum::<u64>());
        let span = self.interval.whole_milliseconds() as i64 / 10;
        let offset = (self.jitter_seed % (span as u64 * 2 + 1)) as i64 - span;
        Duration::milliseconds(offset)
    }
}

fn skipped_obs(
    repo: &str,
    at: OffsetDateTime,
    reason: SkipReason,
) -> Result<Observation, ObservationError> {
    Observation::builder()
        .repo(repo)?
        .observed_at(at)?
        .method(Method::Rest)
        .outcome(Outcome::Skipped { reason })
        .build()
}

/// Build a Skipped observation for an external caller (tests / main loop).
pub fn skip_observation(
    repo: &str,
    at: OffsetDateTime,
    reason: SkipReason,
) -> Result<Observation, ObservationError> {
    skipped_obs(repo, at, reason)
}

/// True when `repo` is the canary (excluded from public ecosystem stats).
pub fn is_canary(repo: &str, note: Option<&str>) -> bool {
    note == Some("canary") || repo.ends_with("/canary")
}
