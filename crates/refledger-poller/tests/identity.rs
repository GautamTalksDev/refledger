//! Identity / User-Agent startup checks.

use refledger_poller::identity::{
    apply_contact_reachability, contact_url_from_ua, format_contact_warning, user_agent,
    user_agent_with, validate_user_agent, IdentityError, DEFAULT_CONTACT_URL, DEFAULT_LOG_ID,
};

#[test]
fn default_log_id_is_refledger() {
    assert_eq!(DEFAULT_LOG_ID, "refledger");
}

#[test]
fn user_agent_embeds_resolving_contact_url() {
    let ua = user_agent();
    assert!(ua.starts_with("refledger/"), "{ua}");
    assert!(ua.contains(DEFAULT_CONTACT_URL), "{ua}");
    validate_user_agent(&ua).expect("default UA must validate");
    assert_eq!(contact_url_from_ua(&ua).unwrap(), DEFAULT_CONTACT_URL);
}

#[test]
fn placeholder_contact_url_is_rejected() {
    let ua = user_agent_with("https://<domain>/operations");
    let err = validate_user_agent(&ua).unwrap_err();
    assert!(err.to_string().contains("placeholder"), "{err}");
}

#[test]
fn unparseable_contact_url_is_rejected() {
    let ua = "refledger/0.1.0 (+not a url)";
    let err = validate_user_agent(ua).unwrap_err();
    assert!(err.to_string().contains("parse"), "{err}");
}

#[test]
fn missing_contact_is_rejected() {
    let err = validate_user_agent("refledger/0.1.0").unwrap_err();
    assert!(err.to_string().contains("missing"), "{err}");
}

#[test]
fn contact_unreachable_is_fatal_before_genesis() {
    let err = IdentityError::Unreachable("https://example.test: HTTP 503".into());
    let out = apply_contact_reachability(false, Err(err));
    assert!(out.is_err(), "pre-genesis must refuse to start");
}

#[test]
fn contact_unreachable_is_warning_after_genesis() {
    let err = IdentityError::Unreachable("https://example.test: HTTP 503".into());
    let warning = apply_contact_reachability(true, Err(err))
        .expect("post-genesis must continue")
        .expect("warning present");
    assert!(warning.contains("contact URL unreachable"), "{warning}");
    assert_eq!(
        warning,
        format_contact_warning(&IdentityError::Unreachable(
            "https://example.test: HTTP 503".into()
        ))
    );
}

#[test]
fn contact_ok_yields_no_warning() {
    assert_eq!(apply_contact_reachability(true, Ok(())).unwrap(), None);
    assert_eq!(apply_contact_reachability(false, Ok(())).unwrap(), None);
}
