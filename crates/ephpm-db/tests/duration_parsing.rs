//! Integration tests for the duration string parser.

use std::time::Duration;

use ephpm_db::duration::parse_duration;

#[test]
fn parse_milliseconds() {
    assert_eq!(parse_duration("100ms").unwrap(), Duration::from_millis(100));
    assert_eq!(parse_duration("0ms").unwrap(), Duration::from_millis(0));
}

#[test]
fn parse_seconds() {
    assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
}

#[test]
fn parse_minutes() {
    assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
}

#[test]
fn parse_hours() {
    assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
}

#[test]
fn reject_no_suffix() {
    assert!(parse_duration("42").is_err());
}

#[test]
fn reject_non_numeric() {
    assert!(parse_duration("abcs").is_err());
}

#[test]
fn reject_empty_string() {
    assert!(parse_duration("").is_err());
}
