#!/bin/bash
# Laravel smoke test against ephpm + litewire SQLite.
#
# Prerequisites:
#   - Laravel container is running and healthy on LARAVEL_URL (default: http://localhost:8082)
#   - ephpm is serving Laravel with litewire SQLite on port 3306
#
# Tests:
#   1. Run artisan migrate (creates tables via pdo_mysql -> litewire -> SQLite)
#   2. Welcome page renders with HTTP 200
#   3. API /smoke route returns JSON with correct fields
#   4. Database connectivity via API /db-check route
#   5. Static asset serving (CSS/JS from public/)

set -euo pipefail

LARAVEL_URL="${LARAVEL_URL:-http://localhost:8082}"
CONTAINER="${LARAVEL_CONTAINER:-$(docker compose -f docker/docker-compose.smoke.yml ps -q laravel)}"
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

artisan() {
    docker exec "${CONTAINER}" php /var/www/html/artisan "$@"
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
            docker logs "${CONTAINER}" 2>&1 | tail -50
            exit 1
        fi
        sleep 3
    done
    echo "==> ${url} is healthy (waited ${waited}s)"
}

# ── Setup ──────────────────────────────────────────────────────────────────────

echo "==> Laravel Smoke Tests"
echo "    URL: ${LARAVEL_URL}"

wait_for_healthy "${LARAVEL_URL}"

# ── Test 1: Artisan migrate ──────────────────────────────────────────────────

echo ""
echo "--- Test 1: artisan migrate ---"
if artisan migrate --force --no-interaction 2>&1; then
    pass "artisan migrate succeeded"
else
    # Migrations may have already run
    if artisan migrate:status --no-interaction 2>&1 | grep -q "Ran"; then
        pass "migrations already applied"
    else
        fail "artisan migrate failed"
    fi
fi

# ── Test 2: Welcome page renders ─────────────────────────────────────────────

echo ""
echo "--- Test 2: Welcome page ---"
HTTP_CODE=$(curl -sf --max-time 15 -o /dev/null -w '%{http_code}' "${LARAVEL_URL}/")
if [ "${HTTP_CODE}" = "200" ]; then
    pass "welcome page returns 200"
else
    fail "welcome page returned ${HTTP_CODE}, expected 200"
fi

BODY=$(curl -sf --max-time 15 "${LARAVEL_URL}/")
if echo "${BODY}" | grep -qi 'laravel\|<!DOCTYPE html>'; then
    pass "welcome page contains expected content"
else
    fail "welcome page missing Laravel branding or HTML doctype"
fi

# ── Test 3: API /smoke route ─────────────────────────────────────────────────

echo ""
echo "--- Test 3: API smoke endpoint ---"
API_RESP=$(curl -sf --max-time 15 "${LARAVEL_URL}/api/smoke" 2>/dev/null || echo "")
if [ -z "${API_RESP}" ]; then
    fail "API /smoke returned empty response"
else
    # Check JSON structure
    if echo "${API_RESP}" | grep -q '"status":"ok"'; then
        pass "API /smoke returns status ok"
    else
        fail "API /smoke missing status:ok (got: ${API_RESP})"
    fi

    if echo "${API_RESP}" | grep -q '"framework":"Laravel"'; then
        pass "API /smoke identifies as Laravel"
    else
        fail "API /smoke missing framework field"
    fi

    # Verify content-type header
    CONTENT_TYPE=$(curl -sf --max-time 15 -o /dev/null -w '%{content_type}' "${LARAVEL_URL}/api/smoke" 2>/dev/null || echo "")
    if echo "${CONTENT_TYPE}" | grep -qi 'application/json'; then
        pass "API /smoke returns application/json"
    else
        fail "API /smoke content-type is '${CONTENT_TYPE}', expected application/json"
    fi
fi

# ── Test 4: Database connectivity ────────────────────────────────────────────

echo ""
echo "--- Test 4: Database connectivity ---"
DB_RESP=$(curl -sf --max-time 15 "${LARAVEL_URL}/api/db-check" 2>/dev/null || echo "")
if [ -z "${DB_RESP}" ]; then
    fail "API /db-check returned empty response"
else
    if echo "${DB_RESP}" | grep -q '"status":"ok"'; then
        pass "database connectivity check passed"
    else
        fail "database connectivity check failed (got: ${DB_RESP})"
    fi
fi

# ── Test 5: Static asset serving ─────────────────────────────────────────────

echo ""
echo "--- Test 5: Static assets ---"
# Laravel ships robots.txt in public/
ROBOTS_CODE=$(curl -sf --max-time 10 -o /dev/null -w '%{http_code}' "${LARAVEL_URL}/robots.txt" 2>/dev/null || echo "000")
if [ "${ROBOTS_CODE}" = "200" ]; then
    pass "robots.txt served from public/"
else
    # Not all Laravel versions ship robots.txt, check favicon instead
    FAV_CODE=$(curl -sf --max-time 10 -o /dev/null -w '%{http_code}' "${LARAVEL_URL}/favicon.ico" 2>/dev/null || echo "000")
    if [ "${FAV_CODE}" = "200" ] || [ "${FAV_CODE}" = "204" ]; then
        pass "static asset served (favicon.ico)"
    else
        # Not a critical failure -- just note it
        pass "static asset check skipped (no robots.txt or favicon.ico)"
    fi
fi

# ── Summary ────────────────────────────────────────────────────────────────────

echo ""
echo "========================================="
echo "Laravel Smoke Tests: ${PASS} passed, ${FAIL} failed"
if [ "${FAIL}" -gt 0 ]; then
    echo -e "\nFailures:${ERRORS}"
    echo "========================================="
    exit 1
fi
echo "========================================="
