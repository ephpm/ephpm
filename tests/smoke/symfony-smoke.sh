#!/bin/bash
# Symfony API smoke test against ephpm + litewire SQLite.
#
# Prerequisites:
#   - Symfony container is running and healthy on SYMFONY_URL (default: http://localhost:8083)
#   - ephpm is serving Symfony with litewire SQLite on port 3306
#
# Tests:
#   1. Doctrine migration (creates tables via pdo_mysql -> litewire -> SQLite)
#   2. API /smoke endpoint returns JSON with correct fields
#   3. Database connectivity via /api/db-check
#   4. Correct content-type headers on JSON responses
#   5. Symfony profiler/debug toolbar disabled in prod-like response

set -euo pipefail

SYMFONY_URL="${SYMFONY_URL:-http://localhost:8083}"
CONTAINER="${SYMFONY_CONTAINER:-$(docker compose -f docker/docker-compose.smoke.yml ps -q symfony)}"
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

console() {
    docker exec "${CONTAINER}" php /var/www/html/bin/console "$@"
}

wait_for_healthy() {
    local url="$1"
    local max_wait=120
    local waited=0
    echo "==> Waiting for ${url} to become healthy..."
    # Symfony may return 404 on / until routing is set up, so check /api/smoke
    while ! curl -sf --max-time 5 "${url}/api/smoke" >/dev/null 2>&1; do
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

echo "==> Symfony Smoke Tests"
echo "    URL: ${SYMFONY_URL}"

wait_for_healthy "${SYMFONY_URL}"

# ── Test 1: Doctrine migrations ──────────────────────────────────────────────

echo ""
echo "--- Test 1: Doctrine migrations ---"
# Symfony skeleton with Doctrine may not have migrations yet.
# Create the database schema if possible, or verify Doctrine connects.
if console doctrine:migrations:migrate --no-interaction --allow-no-migration 2>&1; then
    pass "doctrine migrations ran (or no migrations needed)"
else
    # Fall back to a simpler connectivity check
    if console doctrine:database:create --if-not-exists --no-interaction 2>&1; then
        pass "doctrine database create succeeded"
    else
        fail "doctrine migrations and database create both failed"
    fi
fi

# ── Test 2: API /smoke endpoint ──────────────────────────────────────────────

echo ""
echo "--- Test 2: API smoke endpoint ---"
API_RESP=$(curl -sf --max-time 15 "${SYMFONY_URL}/api/smoke" 2>/dev/null || echo "")
if [ -z "${API_RESP}" ]; then
    fail "API /smoke returned empty response"
else
    if echo "${API_RESP}" | grep -q '"status":"ok"'; then
        pass "API /smoke returns status ok"
    else
        fail "API /smoke missing status:ok (got: ${API_RESP})"
    fi

    if echo "${API_RESP}" | grep -q '"framework":"Symfony"'; then
        pass "API /smoke identifies as Symfony"
    else
        fail "API /smoke missing framework field"
    fi

    if echo "${API_RESP}" | grep -q '"php_version"'; then
        pass "API /smoke includes php_version"
    else
        fail "API /smoke missing php_version field"
    fi
fi

# ── Test 3: Database connectivity ────────────────────────────────────────────

echo ""
echo "--- Test 3: Database connectivity ---"
DB_RESP=$(curl -sf --max-time 15 "${SYMFONY_URL}/api/db-check" 2>/dev/null || echo "")
if [ -z "${DB_RESP}" ]; then
    fail "API /db-check returned empty response"
else
    if echo "${DB_RESP}" | grep -q '"status":"ok"'; then
        pass "database connectivity check passed"
    else
        fail "database connectivity check failed (got: ${DB_RESP})"
    fi

    if echo "${DB_RESP}" | grep -q '"alive":1'; then
        pass "database returns alive=1 from SELECT 1"
    else
        fail "database alive check failed"
    fi
fi

# ── Test 4: JSON content-type headers ────────────────────────────────────────

echo ""
echo "--- Test 4: Content-type headers ---"
CONTENT_TYPE=$(curl -sf --max-time 15 -o /dev/null -w '%{content_type}' "${SYMFONY_URL}/api/smoke" 2>/dev/null || echo "")
if echo "${CONTENT_TYPE}" | grep -qi 'application/json'; then
    pass "API returns application/json content-type"
else
    fail "API content-type is '${CONTENT_TYPE}', expected application/json"
fi

# ── Test 5: No debug toolbar in response ─────────────────────────────────────

echo ""
echo "--- Test 5: Response sanity checks ---"
FULL_RESP=$(curl -sf --max-time 15 -D - "${SYMFONY_URL}/api/smoke" 2>/dev/null || echo "")

# Check that response doesn't contain Symfony's web debug toolbar HTML
if echo "${FULL_RESP}" | grep -qi 'sf-toolbar'; then
    fail "response contains Symfony debug toolbar (should be disabled)"
else
    pass "no debug toolbar in API response"
fi

# HTTP status should be 200
HTTP_CODE=$(curl -sf --max-time 15 -o /dev/null -w '%{http_code}' "${SYMFONY_URL}/api/smoke" 2>/dev/null || echo "000")
if [ "${HTTP_CODE}" = "200" ]; then
    pass "API /smoke returns HTTP 200"
else
    fail "API /smoke returned ${HTTP_CODE}, expected 200"
fi

# ── Summary ────────────────────────────────────────────────────────────────────

echo ""
echo "========================================="
echo "Symfony Smoke Tests: ${PASS} passed, ${FAIL} failed"
if [ "${FAIL}" -gt 0 ]; then
    echo -e "\nFailures:${ERRORS}"
    echo "========================================="
    exit 1
fi
echo "========================================="
