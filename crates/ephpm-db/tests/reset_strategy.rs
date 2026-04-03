//! Integration tests for [`ResetStrategy`] parsing.

use ephpm_db::ResetStrategy;

#[test]
fn parse_always() {
    let s: ResetStrategy = "always".parse().unwrap();
    assert!(matches!(s, ResetStrategy::Always));
}

#[test]
fn parse_never() {
    let s: ResetStrategy = "never".parse().unwrap();
    assert!(matches!(s, ResetStrategy::Never));
}

#[test]
fn parse_smart_default() {
    let s: ResetStrategy = "smart".parse().unwrap();
    assert!(matches!(s, ResetStrategy::Smart));
}

#[test]
fn parse_unknown_falls_back_to_smart() {
    let s: ResetStrategy = "garbage".parse().unwrap();
    assert!(matches!(s, ResetStrategy::Smart));
}

#[test]
fn parse_case_insensitive() {
    let s: ResetStrategy = "ALWAYS".parse().unwrap();
    assert!(matches!(s, ResetStrategy::Always));

    let s: ResetStrategy = "Never".parse().unwrap();
    assert!(matches!(s, ResetStrategy::Never));
}

#[test]
fn default_is_smart() {
    assert!(matches!(ResetStrategy::default(), ResetStrategy::Smart));
}
