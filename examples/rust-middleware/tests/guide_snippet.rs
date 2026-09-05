//! Docs-vs-code lockstep for the tenant-scope pattern in
//! `site/content/guides/native-middleware.md`.
//!
//! # Why this test exists
//!
//! The guide used to carry a hand-typed snippet that was *type-correct and
//! semantically wrong* (issue #453): it denied every request whose
//! `vhost_id()` was `None`, which on a single-site node is every request. It
//! compiled; it would have passed a doctest; it contradicted the prose eight
//! lines above it and shipped that way from birth. Nothing in the tree tied
//! the words to the code.
//!
//! So the guide no longer owns that snippet. It quotes the block between the
//! `GUIDE-SNIPPET-BEGIN/END: tenant-scope` markers in this crate's
//! `src/lib.rs` — real, compiled, clippy-linted code whose behaviour is
//! asserted by `a_single_site_node_is_served_not_denied` and its siblings —
//! and this test fails the build when the two drift apart.
//!
//! Both files are pulled in with `include_str!`, so moving or renaming either
//! is a compile error rather than a silently skipped check.
//!
//! # What it does not catch
//!
//! It pins *one* marked block in *one* guide. A wrong pattern written
//! somewhere else in the docs is out of scope, except for the specific shape
//! of #453, which [`no_unconditional_vhost_denial_in_the_guide`] greps for
//! across the whole file. And CI skips docs-only changes
//! (`ci.yml`'s `paths-ignore`), so a PR that edits only markdown will not run
//! this — the mismatch surfaces on the next PR that touches code.

/// The module the guide quotes.
const SOURCE: &str = include_str!("../src/lib.rs");

/// The guide that quotes it.
const GUIDE: &str = include_str!("../../../site/content/guides/native-middleware.md");

/// Marks the quoted region in [`SOURCE`].
const SOURCE_BEGIN: &str = "GUIDE-SNIPPET-BEGIN: tenant-scope";
/// Ends the quoted region in [`SOURCE`].
const SOURCE_END: &str = "GUIDE-SNIPPET-END: tenant-scope";
/// Marks the quoting fenced block in [`GUIDE`]; the ```` ```rust ```` fence
/// on the next line opens the block this test compares.
const GUIDE_MARKER: &str = "<!-- guide-snippet: tenant-scope -->";

/// Strip the common leading indentation and normalise line endings, so the
/// guide can present the block unindented while the source keeps it nested
/// inside `invoke`.
fn dedent(lines: &[&str]) -> String {
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| if l.len() >= indent { &l[indent..] } else { l.trim_start() })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The marked region of `src/lib.rs`, marker lines excluded.
fn snippet_from_source() -> String {
    let lines: Vec<&str> = SOURCE.lines().collect();
    let begin = lines
        .iter()
        .position(|l| l.contains(SOURCE_BEGIN))
        .expect("src/lib.rs must carry the GUIDE-SNIPPET-BEGIN marker");
    let end = lines
        .iter()
        .position(|l| l.contains(SOURCE_END))
        .expect("src/lib.rs must carry the GUIDE-SNIPPET-END marker");
    assert!(end > begin + 1, "the marked region must not be empty");
    dedent(&lines[begin + 1..end])
}

/// The fenced Rust block the guide introduces with [`GUIDE_MARKER`].
fn snippet_from_guide() -> String {
    let lines: Vec<&str> = GUIDE.lines().collect();
    let marker = lines
        .iter()
        .position(|l| l.trim() == GUIDE_MARKER)
        .expect("the guide must introduce the quoted block with the HTML marker");
    let open = marker + 1;
    assert_eq!(
        lines.get(open).map(|l| l.trim()),
        Some("```rust"),
        "the marker must be immediately followed by a ```rust fence"
    );
    let close = lines[open + 1..]
        .iter()
        .position(|l| l.trim() == "```")
        .expect("the quoted block must be closed")
        + open
        + 1;
    dedent(&lines[open + 1..close])
}

/// The guide's tenant-scope block must be the compiled module's, verbatim.
///
/// Editing one without the other fails here. Fix it by copying the source
/// block into the guide — the source is the original, because it is the copy
/// that runs.
#[test]
fn guide_quotes_the_compiled_tenant_scope_block() {
    let source = snippet_from_source();
    let guide = snippet_from_guide();
    assert_eq!(
        guide, source,
        "site/content/guides/native-middleware.md has drifted from \
         examples/rust-middleware/src/lib.rs.\n\n--- guide ---\n{guide}\n\n--- source ---\n{source}"
    );
}

/// Issue #453's exact shape, anywhere in the guide.
///
/// `let Some(x) = req.vhost_id() else { ... }` has no correct unconditional
/// form: the `else` arm runs on every request of a single-site node, so a
/// snippet written that way either denies all traffic or teaches a reader to.
/// A module that genuinely wants to deny gates it on operator config (see the
/// `require_vhost` branch of the block above) rather than on `None` alone.
#[test]
fn no_unconditional_vhost_denial_in_the_guide() {
    for (n, line) in GUIDE.lines().enumerate() {
        let compact: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            !compact.contains("= req.vhost_id() else"),
            "native-middleware.md:{} reintroduces the #453 let-else denial: {}",
            n + 1,
            line.trim()
        );
    }
}
