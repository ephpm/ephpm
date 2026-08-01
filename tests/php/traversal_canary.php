<?php
/**
 * Fixture for the path-traversal E2E suite
 * (crates/ephpm-e2e/tests/path_traversal.rs).
 *
 * This file lives OUTSIDE tests/docroot/ on purpose. The bare-process E2E
 * harness (xtask/src/e2e_bare.rs) serves tests/docroot/ as the document root,
 * so `tests/php/` is a sibling directory that is reachable ONLY by escaping
 * that document root — exactly the shape of the vulnerability the suite pins.
 *
 * If the marker below ever appears in an HTTP response body, a request
 * traversed out of the document root and executed PHP outside it.
 */
echo "EPHPM_TRAVERSAL_CANARY_a41f7c2e\n";
