#!/usr/bin/env bash
# End-to-end test for ePHPm: build container, start it, test PHP execution.
set -euo pipefail

CE="${CONTAINER_ENGINE:-podman}"
IMAGE="ephpm:latest"
CONTAINER="ephpm-e2e-test"
PORT=8080
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

passed=0
failed=0

pass() {
    echo "  ✓ $1"
    ((passed++))
}

fail() {
    echo "  ✗ $1"
    echo "    Expected: $2"
    echo "    Got: $3"
    ((failed++))
}

cleanup() {
    echo ""
    echo "Cleaning up..."
    $CE stop "$CONTAINER" 2>/dev/null || true
    $CE rm "$CONTAINER" 2>/dev/null || true
}

trap cleanup EXIT

# ── Start container ──────────────────────────────────────────────────────────

echo "Starting container..."
# Remove stale container if present
$CE rm -f "$CONTAINER" 2>/dev/null || true

$CE run -d --name "$CONTAINER" \
    -p "$PORT:8080" \
    -v "$PROJECT_DIR/tests/docroot:/var/www/html:ro" \
    -v "$PROJECT_DIR/tests/ephpm-test.toml:/etc/ephpm/ephpm.toml:ro" \
    "$IMAGE"

# Wait for the server to be ready
echo "Waiting for server..."
for i in $(seq 1 30); do
    if curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/" 2>/dev/null | grep -q '200'; then
        echo "Server ready after ${i}s"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: Server did not start within 30s"
        echo "Container logs:"
        $CE logs "$CONTAINER"
        exit 1
    fi
    sleep 1
done

# ── Tests ────────────────────────────────────────────────────────────────────

echo ""
echo "Running e2e tests..."
echo ""

# Test 1: index.php returns PHP output (not stub)
echo "Test 1: index.php — basic PHP execution"
body=$(curl -s "http://localhost:$PORT/")
if echo "$body" | grep -q "Hello from ePHPm!"; then
    pass "Contains 'Hello from ePHPm!'"
else
    fail "Contains 'Hello from ePHPm!'" "greeting" "$body"
fi
if echo "$body" | grep -q "PHP Version:"; then
    pass "Contains PHP version"
else
    fail "Contains PHP version" "PHP Version: X.Y.Z" "$body"
fi
if echo "$body" | grep -qE "Server API: embed|Server API: litespeed"; then
    pass "SAPI is embed (not stub)"
else
    fail "SAPI is embed" "embed" "$body"
fi

# Test 2: info.php returns phpinfo HTML
echo ""
echo "Test 2: info.php — phpinfo()"
body=$(curl -s "http://localhost:$PORT/info.php")
if echo "$body" | grep -q "<title>phpinfo()</title>"; then
    pass "phpinfo() HTML page returned"
else
    # Some phpinfo versions use different title format
    if echo "$body" | grep -qi "PHP Version"; then
        pass "phpinfo() HTML page returned (alt format)"
    else
        fail "phpinfo() page" "<title>phpinfo()</title>" "${body:0:200}"
    fi
fi
if echo "$body" | grep -q "PHP License"; then
    pass "Contains PHP License section"
else
    fail "Contains PHP License" "PHP License" "${body:0:200}"
fi

# Test 3: test.php with query string
echo ""
echo "Test 3: test.php?foo=bar — GET params"
body=$(curl -s "http://localhost:$PORT/test.php?foo=bar")
if echo "$body" | grep -q "REQUEST_METHOD: GET"; then
    pass "REQUEST_METHOD is GET"
else
    fail "REQUEST_METHOD" "GET" "$body"
fi
if echo "$body" | grep -q "foo = bar"; then
    pass "GET param foo=bar parsed"
else
    fail "GET param" "foo = bar" "$body"
fi
if echo "$body" | grep -q "QUERY_STRING: foo=bar"; then
    pass "QUERY_STRING populated"
else
    fail "QUERY_STRING" "foo=bar" "$body"
fi

# Test 4: POST request
echo ""
echo "Test 4: test.php POST — POST params"
body=$(curl -s -X POST -d "name=world" "http://localhost:$PORT/test.php")
if echo "$body" | grep -q "REQUEST_METHOD: POST"; then
    pass "REQUEST_METHOD is POST"
else
    fail "REQUEST_METHOD" "POST" "$body"
fi
if echo "$body" | grep -q "name = world"; then
    pass "POST param name=world parsed"
else
    fail "POST param" "name = world" "$body"
fi

# Test 5: Content-Type header on test.php
echo ""
echo "Test 5: test.php — custom Content-Type header"
ct=$(curl -s -o /dev/null -w '%{content_type}' "http://localhost:$PORT/test.php")
if echo "$ct" | grep -q "text/plain"; then
    pass "Content-Type is text/plain"
else
    fail "Content-Type" "text/plain" "$ct"
fi

# Test 6: 404 for missing file
echo ""
echo "Test 6: nonexistent.html — 404"
status=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/nonexistent.html")
if [ "$status" = "404" ]; then
    pass "Returns 404 for missing file"
else
    fail "HTTP status" "404" "$status"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "════════════════════════════════"
echo "  Results: $passed passed, $failed failed"
echo "════════════════════════════════"

if [ "$failed" -gt 0 ]; then
    echo ""
    echo "Container logs:"
    $CE logs "$CONTAINER"
    exit 1
fi

echo ""
echo "All e2e tests passed!"
