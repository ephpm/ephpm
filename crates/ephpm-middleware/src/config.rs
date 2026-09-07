//! Config-payload key checking for middleware modules.
//!
//! A mount's `config` block is free-form JSON, so a module that reads the keys
//! it knows and ignores the rest turns an operator's explicit instruction into
//! a no-op with no diagnostic anywhere (issue #473):
//!
//! ```toml
//! [[server.middleware]]
//! library = "ratelimit"
//! config = { per_ip_rp = 50 }   # typo — the real key is `per_ip_rps`
//! ```
//!
//! used to parse, mount, and run the *default* rate limit with every health
//! check green. For a security control (`ip_allowlist`, `api_key`, `jwt`,
//! `ratelimit`) that quietly widens what is allowed.
//!
//! A module declares its accepted key set with
//! [`Middleware::CONFIG_KEYS`](crate::Middleware::CONFIG_KEYS); the two
//! execution lanes ([`builtin::BuiltinModule`](crate::builtin::BuiltinModule)
//! and the [`declare!`](crate::declare) C ABI glue) both run
//! [`init_checked`](crate::init_checked), which enforces the declaration
//! before the module's own `init` sees the payload. Declaring is **opt-in**:
//! a module that leaves `CONFIG_KEYS` at its default (`None`) keeps the old
//! lenient behaviour, which is what a third-party module built against an
//! earlier version of this crate gets.
//!
//! Modules whose config nests (a section object, e.g. `header_transform`'s
//! `request` / `response`) call [`reject_unknown_keys`] themselves for the
//! inner objects, since the trait const describes only the top level.
//!
//! Free-form sub-objects — `api_key`'s `keys` map, `redirect`'s `host_map`,
//! `header_transform`'s `*.set` — are **not** checkable: every key in them is
//! operator data (an API key, a hostname, a header name), not a schema name.
//! They stay unchecked by design.

/// Reject config keys a module does not accept.
///
/// `path` is the dotted location of `config` inside the mount's payload — `""`
/// for the top level, `"request"` for a section — and is used to build a key
/// name an operator can find in `ephpm.toml`. `accepted` is the complete set
/// of keys valid at that location.
///
/// Only *objects* are checked. A `Null` (the value a mount with no `config`
/// gets) and any non-object value pass through: whether a scalar where an
/// object was expected is an error belongs to the module, which has the type
/// information to say so.
///
/// # Errors
///
/// Returns a message naming every unrecognised key (sorted, so the message is
/// stable), a did-you-mean hint for near misses, and the accepted set. The
/// caller turns that into a startup failure.
///
/// ```
/// # use ephpm_middleware::config::reject_unknown_keys;
/// let cfg = serde_json::json!({ "per_ip_rp": 50 });
/// let err = reject_unknown_keys(&cfg, "", &["burst", "key_headers", "per_ip_rps"])
///     .expect_err("typo must be rejected");
/// assert!(err.contains("did you mean `per_ip_rps`"), "{err}");
/// ```
pub fn reject_unknown_keys(
    config: &serde_json::Value,
    path: &str,
    accepted: &[&str],
) -> Result<(), String> {
    let Some(map) = config.as_object() else {
        return Ok(());
    };
    let mut unknown: Vec<&String> =
        map.keys().filter(|k| !accepted.contains(&k.as_str())).collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();

    let qualify =
        |key: &str| if path.is_empty() { key.to_owned() } else { format!("{path}.{key}") };
    let named = unknown
        .iter()
        .map(|k| {
            let hint = did_you_mean(k, accepted)
                .map(|s| format!(" (did you mean `{}`?)", qualify(s)))
                .unwrap_or_default();
            format!("`{}`{hint}", qualify(k))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let plural = if unknown.len() == 1 { "key" } else { "keys" };
    let accepted_list = if accepted.is_empty() {
        "this module accepts no config keys".to_owned()
    } else {
        format!(
            "accepted {}: {}",
            if accepted.len() == 1 { "key" } else { "keys" },
            accepted.iter().map(|k| format!("`{}`", qualify(k))).collect::<Vec<_>>().join(", ")
        )
    };
    Err(format!("unknown config {plural} {named}; {accepted_list}"))
}

/// The closest accepted key to `key`, when one is close enough to be worth
/// suggesting.
///
/// A spelling that differs only in case or in `-`/`_` is always suggested; a
/// genuine misspelling is suggested when its edit distance is within a third
/// of the key's length (at most 3), which catches `per_ip_rp` → `per_ip_rps`
/// without proposing `csp` for `hsts`.
fn did_you_mean<'a>(key: &str, accepted: &[&'a str]) -> Option<&'a str> {
    let normalize = |s: &str| s.to_ascii_lowercase().replace(['-', '_'], "");
    let normalized = normalize(key);
    if let Some(hit) = accepted.iter().find(|a| normalize(a) == normalized) {
        return Some(hit);
    }
    let budget = (key.chars().count() / 3).clamp(1, 3);
    accepted
        .iter()
        .map(|a| (edit_distance(&normalized, &normalize(a)), *a))
        .filter(|(d, _)| *d <= budget)
        .min_by_key(|(d, a)| (*d, a.len()))
        .map(|(_, a)| a)
}

/// Optimal string alignment distance over `char`s — Levenshtein plus adjacent
/// **transposition** as a single edit.
///
/// Transpositions have to count as one: `mode` → `moed` and `per_ip_rps` →
/// `per_ip_rsp` are the typos operators actually make, and plain Levenshtein
/// scores them 2, which puts them outside the suggestion budget for a short
/// key. Rolling three rows; the strings are config key names, so the
/// allocation is bounded by the schema.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // `prev2` is row i-2, `prev` row i-1, `cur` the row being filled.
    let mut prev2 = vec![0_usize; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0_usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (prev[j - 1] + cost).min(prev[j] + 1).min(cur[j - 1] + 1);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            cur[j] = d;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: &[&str] = &["burst", "key_headers", "per_ip_rps"];

    #[test]
    fn known_keys_and_absent_config_pass() {
        assert!(reject_unknown_keys(&serde_json::Value::Null, "", KEYS).is_ok());
        assert!(reject_unknown_keys(&serde_json::json!({}), "", KEYS).is_ok());
        assert!(
            reject_unknown_keys(&serde_json::json!({ "per_ip_rps": 5, "burst": 0 }), "", KEYS)
                .is_ok()
        );
        // Not an object: the module's own type checking owns this case.
        assert!(reject_unknown_keys(&serde_json::json!("nope"), "", KEYS).is_ok());
    }

    #[test]
    fn unknown_key_is_named_with_a_hint_and_the_accepted_set() {
        let err = reject_unknown_keys(&serde_json::json!({ "per_ip_rp": 50 }), "", KEYS)
            .expect_err("must reject");
        assert!(err.contains("unknown config key `per_ip_rp`"), "{err}");
        assert!(err.contains("did you mean `per_ip_rps`"), "{err}");
        assert!(err.contains("`key_headers`"), "{err}");
    }

    #[test]
    fn every_unknown_key_is_named_in_sorted_order() {
        let err = reject_unknown_keys(
            &serde_json::json!({ "zeta": 1, "alpha": 2, "per_ip_rps": 5 }),
            "",
            KEYS,
        )
        .expect_err("must reject");
        assert!(err.starts_with("unknown config keys `alpha`, `zeta`;"), "{err}");
    }

    #[test]
    fn a_path_qualifies_both_the_bad_key_and_the_suggestion() {
        let err =
            reject_unknown_keys(&serde_json::json!({ "sett": {} }), "response", &["set", "remove"])
                .expect_err("must reject");
        assert!(err.contains("`response.sett`"), "{err}");
        assert!(err.contains("did you mean `response.set`"), "{err}");
    }

    #[test]
    fn case_and_separator_spellings_are_suggested() {
        assert_eq!(did_you_mean("PER_IP_RPS", KEYS), Some("per_ip_rps"));
        assert_eq!(did_you_mean("key-headers", KEYS), Some("key_headers"));
        assert_eq!(did_you_mean("keyheaders", KEYS), Some("key_headers"));
    }

    #[test]
    fn an_unrelated_key_gets_no_hint() {
        assert_eq!(did_you_mean("csp", KEYS), None);
        let err = reject_unknown_keys(&serde_json::json!({ "csp": "x" }), "", KEYS)
            .expect_err("must reject");
        assert!(!err.contains("did you mean"), "{err}");
    }

    #[test]
    fn a_module_that_accepts_nothing_says_so() {
        let err =
            reject_unknown_keys(&serde_json::json!({ "x": 1 }), "", &[]).expect_err("must reject");
        assert!(err.contains("this module accepts no config keys"), "{err}");
    }

    #[test]
    fn edit_distance_counts_a_transposition_as_one_edit() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("moed", "mode"), 1);
        assert_eq!(edit_distance("peripsr", "periprs"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn a_transposed_short_key_still_gets_a_hint() {
        assert_eq!(did_you_mean("brust", &["burst", "per_ip_rps"]), Some("burst"));
    }
}
