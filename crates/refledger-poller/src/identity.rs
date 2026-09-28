//! Crawler identity: default `log_id`, User-Agent, and contact-URL checks.
//!
//! OPERATIONS.md §2 promises a contact URL that is always present and always
//! resolves. Until `refledger.dev` (or successor) is live, the contact URL
//! points at the public repository's `OPERATIONS.md`.

use thiserror::Error;

/// Default `log_id` baked into every signed head. Chosen before genesis; do
/// not change after the first entry exists.
pub const DEFAULT_LOG_ID: &str = "refledger";

/// Crate version embedded in the User-Agent.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Public policy document. Must stay reachable before the first poll.
///
/// Uses the raw GitHub URL so a GET returns the markdown body (not an HTML
/// repository page). Replace with `https://refledger.dev/operations` once the
/// domain is live — update this constant and OPERATIONS.md §2 together.
pub const DEFAULT_CONTACT_URL: &str =
    "https://raw.githubusercontent.com/refledger/refledger/main/OPERATIONS.md";

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("User-Agent contact URL still contains a placeholder ('<' or '>'): {0}")]
    Placeholder(String),
    #[error("User-Agent contact URL failed to parse: {0}")]
    InvalidUrl(String),
    #[error("User-Agent is missing a contact URL (+https://...)")]
    MissingContact,
    #[error("contact URL did not resolve: {0}")]
    Unreachable(String),
}

/// `refledger/<version> (+<contact_url>)`
pub fn user_agent() -> String {
    user_agent_with(DEFAULT_CONTACT_URL)
}

pub fn user_agent_with(contact_url: &str) -> String {
    format!("refledger/{VERSION} (+{contact_url})")
}

/// Extract the contact URL from a User-Agent of the form `name/ver (+URL)`.
pub fn contact_url_from_ua(ua: &str) -> Result<&str, IdentityError> {
    let start = ua.rfind("(+").ok_or(IdentityError::MissingContact)? + 2;
    let end = ua[start..]
        .find(')')
        .ok_or(IdentityError::MissingContact)?
        + start;
    let url = &ua[start..end];
    if url.is_empty() {
        return Err(IdentityError::MissingContact);
    }
    Ok(url)
}

/// Refuse to poll if the contact URL still has a placeholder or does not parse.
pub fn validate_user_agent(ua: &str) -> Result<(), IdentityError> {
    let url = contact_url_from_ua(ua)?;
    if url.contains('<') || url.contains('>') {
        return Err(IdentityError::Placeholder(url.to_owned()));
    }
    // Minimal parse: scheme + host. Avoids a URL-crate dependency for one check.
    let parsed = url::Url::parse(url).map_err(|e| IdentityError::InvalidUrl(format!("{url}: {e}")))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(IdentityError::InvalidUrl(format!(
            "{url}: scheme must be http or https"
        )));
    }
    if parsed.host_str().is_none() {
        return Err(IdentityError::InvalidUrl(format!("{url}: missing host")));
    }
    Ok(())
}

/// OPERATIONS.md §2: the contact URL must resolve before the first request.
pub fn ensure_contact_resolves(ua: &str) -> Result<(), IdentityError> {
    validate_user_agent(ua)?;
    let url = contact_url_from_ua(ua)?;
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    let response = agent
        .get(url)
        .set("User-Agent", ua)
        .call()
        .map_err(|e| IdentityError::Unreachable(format!("{url}: {e}")))?;
    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(IdentityError::Unreachable(format!("{url}: HTTP {status}")));
    }
    Ok(())
}

/// Combined startup gate: validate shape, then confirm the policy resolves.
pub fn refuse_to_poll_unless_identified(ua: &str) -> Result<(), IdentityError> {
    ensure_contact_resolves(ua)
}
