#!/bin/bash
# Run all smoke tests sequentially.
#
# Usage:
#   tests/smoke/run-all.sh [--app wordpress|laravel|symfony]
#
# With no args, runs all three. With --app, runs just one.
# Exits non-zero if any test suite fails.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="docker/docker-compose.smoke.yml"
FAILURES=0

run_suite() {
    local name="$1"
    local script="$2"
    echo ""
    echo "################################################################"
    echo "# Running ${name} smoke tests"
    echo "################################################################"
    echo ""
    if bash "${script}"; then
        echo ""
        echo "${name}: ALL PASSED"
    else
        echo ""
        echo "${name}: FAILED" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

# Parse args
APP=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --app) APP="$2"; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

# Run requested suites
if [ -z "${APP}" ] || [ "${APP}" = "wordpress" ]; then
    run_suite "WordPress" "${SCRIPT_DIR}/wp-smoke.sh"
fi

if [ -z "${APP}" ] || [ "${APP}" = "laravel" ]; then
    run_suite "Laravel" "${SCRIPT_DIR}/laravel-smoke.sh"
fi

if [ -z "${APP}" ] || [ "${APP}" = "symfony" ]; then
    run_suite "Symfony" "${SCRIPT_DIR}/symfony-smoke.sh"
fi

echo ""
echo "================================================================"
if [ "${FAILURES}" -gt 0 ]; then
    echo "SMOKE TESTS: ${FAILURES} suite(s) FAILED"
    exit 1
else
    echo "SMOKE TESTS: ALL SUITES PASSED"
fi
echo "================================================================"
