//! Integration tests for database URL parsing.
//!
//! Exercises the public [`DbUrl`] parser with various URL formats,
//! edge cases, and error conditions.

use ephpm_db::url::DbUrl;

#[test]
fn mysql_full_url() {
    let u = DbUrl::parse("mysql://admin:s3cret@db.example.com:3307/production").unwrap();
    assert_eq!(u.scheme, "mysql");
    assert_eq!(u.username, "admin");
    assert_eq!(u.password, "s3cret");
    assert_eq!(u.host, "db.example.com");
    assert_eq!(u.port, 3307);
    assert_eq!(u.database, "production");
    assert_eq!(u.addr(), "db.example.com:3307");
}

#[test]
fn postgres_default_port() {
    let u = DbUrl::parse("postgres://app:pass@10.0.0.1/mydb").unwrap();
    assert_eq!(u.scheme, "postgres");
    assert_eq!(u.port, 5432);
    assert_eq!(u.host, "10.0.0.1");
}

#[test]
fn postgresql_scheme_alias() {
    let u = DbUrl::parse("postgresql://user:pw@host/db").unwrap();
    assert_eq!(u.scheme, "postgres");
}

#[test]
fn percent_encoded_password() {
    let u = DbUrl::parse("mysql://user:p%40ss%23word@host:3306/db").unwrap();
    assert_eq!(u.password, "p@ss#word");
}

#[test]
fn empty_password() {
    let u = DbUrl::parse("mysql://readonly@host/db").unwrap();
    assert_eq!(u.username, "readonly");
    assert_eq!(u.password, "");
}

#[test]
fn missing_scheme_fails() {
    let result = DbUrl::parse("user:pass@host/db");
    assert!(result.is_err());
}

#[test]
fn unsupported_scheme_fails() {
    let result = DbUrl::parse("sqlite://path/to/db");
    assert!(result.is_err());
}

#[test]
fn missing_at_sign_fails() {
    let result = DbUrl::parse("mysql://host:3306/db");
    assert!(result.is_err());
}
