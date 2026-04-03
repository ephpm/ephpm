#!/bin/bash
# WordPress smoke test against ephpm + litewire SQLite.
#
# Prerequisites:
#   - WordPress container is running and healthy on WP_URL (default: http://localhost:8081)
#   - ephpm is serving WordPress with litewire SQLite on port 3306
#
# Tests:
#   1. WP-CLI core install (creates tables via pdo_mysql -> litewire -> SQLite)
#   2. Front page renders with <!DOCTYPE html>
#   3. Admin login page loads (/wp-login.php)
#   4. Create a post via WP-CLI and verify it appears
#   5. REST API returns JSON

set -euo pipefail

WP_URL="${WP_URL:-http://localhost:8081}"
CONTAINER="${WP_CONTAINER:-$(docker compose -f docker/docker-compose.smoke.yml ps -q wordpress)}"
PASS=0
FAIL=0
ERRORS=""

# ── Helpers ────────────────────────────────────────────────────────────────────

pass() {
    PASS=$((PASS + 1))
    echo "  PASS: $1"
}

fail() {
    FAIL=$((FAIL + 1))
    ERRORS="${ERRORS}\n  FAIL: $1"
    echo "  FAIL: $1" >&2
}

wp_exec() {
    docker exec "${CONTAINER}" wp --path=/var/www/html --allow-root "$@"
}

wait_for_healthy() {
    local url="$1"
    local max_wait=120
    local waited=0
    echo "==> Waiting for ${url} to become healthy..."
    while ! curl -sf --max-time 5 "${url}" >/dev/null 2>&1; do
        waited=$((waited + 3))
        if [ "$waited" -ge "$max_wait" ]; then
            echo "ERROR: ${url} not healthy after ${max_wait}s" >&2
            # Dump container logs for debugging
            docker logs "${CONTAINER}" 2>&1 | tail -50
            exit 1
        fi
        sleep 3
    done
    echo "==> ${url} is healthy (waited ${waited}s)"
}

# ── Setup ──────────────────────────────────────────────────────────────────────

echo "==> WordPress Smoke Tests"
echo "    URL: ${WP_URL}"

# Wait for ephpm + litewire to be ready
wait_for_healthy "${WP_URL}/wp-login.php"

# ── Test 1: WP-CLI core install ───────────────────────────────────────────────

echo ""
echo "--- Test 1: WordPress core install ---"
if wp_exec core install \
    --url="${WP_URL}" \
    --title="ePHPm Smoke Test" \
    --admin_user=admin \
    --admin_password=admin123 \
    --admin_email="admin@test.local" \
    --skip-email 2>&1; then
    pass "wp core install succeeded"
else
    # May already be installed from a previous run
    if wp_exec core is-installed 2>&1; then
        pass "wp core already installed"
    else
        fail "wp core install failed"
    fi
fi

# ── Test 2: Front page renders HTML ───────────────────────────────────────────

echo ""
echo "--- Test 2: Front page renders ---"
BODY=$(curl -sf --max-time 15 "${WP_URL}/")
if echo "${BODY}" | grep -qi '<!DOCTYPE html>'; then
    pass "front page contains <!DOCTYPE html>"
else
    fail "front page missing <!DOCTYPE html>"
fi

if echo "${BODY}" | grep -qi 'ePHPm Smoke Test'; then
    pass "front page contains site title"
else
    fail "front page missing site title 'ePHPm Smoke Test'"
fi

# ── Test 3: Admin login page loads ────────────────────────────────────────────

echo ""
echo "--- Test 3: Admin login page ---"
HTTP_CODE=$(curl -sf --max-time 15 -o /dev/null -w '%{http_code}' "${WP_URL}/wp-login.php")
if [ "${HTTP_CODE}" = "200" ]; then
    pass "wp-login.php returns 200"
else
    fail "wp-login.php returned ${HTTP_CODE}, expected 200"
fi

LOGIN_BODY=$(curl -sf --max-time 15 "${WP_URL}/wp-login.php")
if echo "${LOGIN_BODY}" | grep -qi 'user_login'; then
    pass "login page has user_login form field"
else
    fail "login page missing user_login form field"
fi

# ── Test 4: Create a post and verify it appears ──────────────────────────────

echo ""
echo "--- Test 4: Create and read a post ---"
POST_ID=$(wp_exec post create --post_title="Smoke Test Post" --post_status=publish --porcelain 2>&1)
if [ -n "${POST_ID}" ] && [ "${POST_ID}" -gt 0 ] 2>/dev/null; then
    pass "created post with ID ${POST_ID}"

    # Fetch the post via the REST API
    REST_RESP=$(curl -sf --max-time 15 "${WP_URL}/wp-json/wp/v2/posts/${POST_ID}" 2>/dev/null || echo "")
    if echo "${REST_RESP}" | grep -q "Smoke Test Post"; then
        pass "REST API returns the created post"
    else
        fail "REST API did not return post ${POST_ID}"
    fi
else
    fail "failed to create post (got: ${POST_ID})"
fi

# ── Test 5: REST API returns JSON ─────────────────────────────────────────────

echo ""
echo "--- Test 5: REST API content type ---"
CONTENT_TYPE=$(curl -sf --max-time 15 -o /dev/null -w '%{content_type}' "${WP_URL}/wp-json/wp/v2/posts" 2>/dev/null || echo "")
if echo "${CONTENT_TYPE}" | grep -qi 'application/json'; then
    pass "REST API returns application/json content-type"
else
    fail "REST API content-type is '${CONTENT_TYPE}', expected application/json"
fi

# ── Summary ────────────────────────────────────────────────────────────────────

echo ""
echo "========================================="
echo "WordPress Smoke Tests: ${PASS} passed, ${FAIL} failed"
if [ "${FAIL}" -gt 0 ]; then
    echo -e "\nFailures:${ERRORS}"
    echo "========================================="
    exit 1
fi
echo "========================================="
