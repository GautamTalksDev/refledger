//! A 304 exempts the primary rate limit only when the request carried a valid Authorization header.
//! The secondary limit is real, costs one point per 304, and is unobservable from response headers.
//!
//! [`PrimaryPoints`] and [`SecondaryPoints`] share no arithmetic and no equality. One integer
//! standing for both budgets is how every bug in this class gets written.
//! [`SecondaryBudget`] is a single variant, [`SecondaryBudget::Unobservable`], until M0
//! calibration replaces it with a measured ceiling in this type alone.

use std::sync::atomic::{AtomicU32, Ordering};

use thiserror::Error;
use time::{Duration, OffsetDateTime};

use crate::observation::{
    ErrorClass, Method, Observation, ObservationError, Outcome, SecondaryLimitEvent,
};

/// Points counted against GitHub's documented primary rate limit.
///
/// Not comparable with [`SecondaryPoints`] and not addable to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrimaryPoints(u32);

impl PrimaryPoints {
    pub const fn new(points: u32) -> Self {
        Self(points)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    const fn saturating_add(self, n: u32) -> Self {
        Self(self.0.saturating_add(n))
    }
}

/// Points counted against the undocumented secondary rate limit.
///
/// A 304 costs one of these and, when the request was authorized, none of
/// [`PrimaryPoints`]. Not comparable with [`PrimaryPoints`] and not addable to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecondaryPoints(u32);

impl SecondaryPoints {
    pub const fn new(points: u32) -> Self {
        Self(points)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    const fn saturating_add(self, n: u32) -> Self {
        Self(self.0.saturating_add(n))
    }
}

/// What the process can know about the secondary-limit ceiling.
///
/// GitHub states there is no way to check secondary-limit status. Absence of a
/// header is not zero and it is not unlimited. When M0 calibration measures a
/// ceiling, that measurement is added here and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecondaryBudget {
    Unobservable,
}

/// Primary remaining, parsed from `x-ratelimit-remaining` only.
///
/// The secondary budget on this value is always [`SecondaryBudget::Unobservable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedLimits {
    primary_remaining: Option<PrimaryPoints>,
    secondary_budget: SecondaryBudget,
}

impl ParsedLimits {
    pub const fn primary_remaining(self) -> Option<PrimaryPoints> {
        self.primary_remaining
    }

    pub const fn secondary_budget(self) -> SecondaryBudget {
        self.secondary_budget
    }
}

/// Parse rate-limit headers.
///
/// `x-ratelimit-remaining` becomes [`PrimaryPoints`]. Every other header,
/// including any name that looks like a secondary budget, is ignored. A missing
/// or unparseable remaining header is `None`, not zero.
pub fn parse_rate_limit_headers<'a, I>(headers: I) -> ParsedLimits
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut primary_remaining = None;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("x-ratelimit-remaining") {
            primary_remaining = value.trim().parse::<u32>().ok().map(PrimaryPoints::new);
        }
    }
    ParsedLimits {
        primary_remaining,
        secondary_budget: SecondaryBudget::Unobservable,
    }
}

/// Running totals for one authorized poll stream.
///
/// `record(200)` charges both budgets. `record(304)` charges only the secondary
/// budget. This ledger is only meaningful for requests built with Authorization;
/// [`crate::github::etag::ConditionalRequest::build`] refuses to construct any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointLedger {
    primary_points_used: PrimaryPoints,
    secondary_points_used: SecondaryPoints,
}

impl Default for PointLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl PointLedger {
    pub const fn new() -> Self {
        Self {
            primary_points_used: PrimaryPoints::new(0),
            secondary_points_used: SecondaryPoints::new(0),
        }
    }

    pub const fn primary_points_used(self) -> PrimaryPoints {
        self.primary_points_used
    }

    pub const fn secondary_points_used(self) -> SecondaryPoints {
        self.secondary_points_used
    }

    pub fn record(&mut self, http_status: u16) {
        match http_status {
            200 => {
                self.primary_points_used = self.primary_points_used.saturating_add(1);
                self.secondary_points_used = self.secondary_points_used.saturating_add(1);
            }
            304 => {
                self.secondary_points_used = self.secondary_points_used.saturating_add(1);
            }
            _ => {}
        }
    }
}

/// Delay before the next attempt.
///
/// A present `Retry-After` is returned unchanged, including zero. GitHub's
/// guidance for the no-header case is to wait at least one minute and back off
/// exponentially, so a missing header yields `60s * 2^attempt + jitter` and
/// never less than 60 seconds. Jitter is added, never subtracted.
pub fn backoff_delay(
    retry_after: Option<Duration>,
    attempt: u32,
    jitter: Duration,
) -> Result<Duration, RateLimitError> {
    if let Some(given) = retry_after {
        if given.is_negative() {
            return Err(RateLimitError::NegativeRetryAfter);
        }
        return Ok(given);
    }
    if jitter.is_negative() {
        return Err(RateLimitError::NegativeJitter);
    }
    let mut base = Duration::seconds(60);
    for _ in 0..attempt.min(16) {
        base = base.checked_mul(2).unwrap_or(base);
    }
    Ok(base.checked_add(jitter).unwrap_or(base))
}

/// In-flight ceiling. The default is 20, under GitHub's documented 100-concurrent cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrencyCap {
    max_in_flight: u32,
}

impl ConcurrencyCap {
    pub const DEFAULT_MAX_IN_FLIGHT: u32 = 20;
    /// Documented secondary-limit concurrency ceiling. A configured cap must stay below it.
    pub const DOCUMENTED_CEILING: u32 = 100;

    pub fn new(max_in_flight: u32) -> Result<Self, RateLimitError> {
        if max_in_flight == 0 || max_in_flight >= Self::DOCUMENTED_CEILING {
            return Err(RateLimitError::ConcurrencyCap(max_in_flight));
        }
        Ok(Self { max_in_flight })
    }

    pub const fn max_in_flight(self) -> u32 {
        self.max_in_flight
    }
}

impl Default for ConcurrencyCap {
    fn default() -> Self {
        Self {
            max_in_flight: Self::DEFAULT_MAX_IN_FLIGHT,
        }
    }
}

/// Counts in-flight requests against a [`ConcurrencyCap`].
#[derive(Debug)]
pub struct InFlight {
    cap: u32,
    current: AtomicU32,
}

impl InFlight {
    pub fn new(cap: ConcurrencyCap) -> Self {
        Self {
            cap: cap.max_in_flight,
            current: AtomicU32::new(0),
        }
    }

    pub fn try_acquire(&self) -> Result<(), RateLimitError> {
        self.current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current >= self.cap {
                    None
                } else {
                    Some(current + 1)
                }
            })
            .map(|_| ())
            .map_err(|_| RateLimitError::ConcurrencySaturated)
    }

    pub fn release(&self) {
        let _ = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(1))
            });
    }
}

/// A 403 or 429, the backoff that was applied, and the event the observation must carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitHit {
    pub event: SecondaryLimitEvent,
    pub backoff: Duration,
    pub error_class: ErrorClass,
}

impl LimitHit {
    /// Build the observation Prompt 18 requires for a refusal.
    ///
    /// The [`SecondaryLimitEvent`] is attached inside this function. A 403 or 429
    /// that reached this path cannot be persisted without it.
    pub fn observation(
        &self,
        repo: &str,
        observed_at: OffsetDateTime,
        rate_limit_remaining: Option<u32>,
    ) -> Result<Observation, ObservationError> {
        let mut built = Observation::builder()
            .repo(repo)?
            .observed_at(observed_at)?
            .method(Method::Rest)
            .secondary_limit_observed(self.event.clone())
            .outcome(Outcome::Failed {
                http_status: self.event.status,
                error_class: self.error_class,
                backoff_applied: self.backoff,
            });
        if let Some(remaining) = rate_limit_remaining {
            built = built.rate_limit_remaining(remaining);
        }
        built.build()
    }
}

/// Every 403 and 429 becomes a [`LimitHit`]. Other statuses are not refusals.
pub fn classify_refusal(
    status: u16,
    retry_after: Option<Duration>,
    request_rate_rpm: u32,
    attempt: u32,
    jitter: Duration,
) -> Result<LimitHit, RateLimitError> {
    if !matches!(status, 403 | 429) {
        return Err(RateLimitError::NotALimitStatus(status));
    }
    let backoff = backoff_delay(retry_after, attempt, jitter)?;
    Ok(LimitHit {
        event: SecondaryLimitEvent {
            status,
            retry_after,
            request_rate_rpm,
        },
        backoff,
        error_class: ErrorClass::SecondaryRateLimit,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RateLimitError {
    #[error("retry-after is negative")]
    NegativeRetryAfter,
    #[error("jitter is negative")]
    NegativeJitter,
    #[error("concurrency cap {0} must be in 1..100")]
    ConcurrencyCap(u32),
    #[error("concurrency cap is saturated")]
    ConcurrencySaturated,
    #[error("status {0} is not a 403 or 429 refusal")]
    NotALimitStatus(u16),
}
