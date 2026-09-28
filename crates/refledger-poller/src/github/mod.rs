//! GitHub poller transport: conditional requests, rate-limit budgets, and
//! content-addressed tag resolution.
//!
//! A 304 exempts the primary rate limit only when the request carried a valid Authorization header.
//! The secondary limit is real, costs one point per 304, and is unobservable from response headers.

pub mod etag;
pub mod ratelimit;
pub mod rest;
