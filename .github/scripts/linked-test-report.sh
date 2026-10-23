#!/usr/bin/env bash
# Report a PHP-linked `cargo nextest` run and fail loudly when it did not run.
#
# Usage: linked-test-report.sh <nextest-log> <leg-label> <report-file>
#
# Parses nextest's "Starting N tests" and final "Summary [...] N tests run: ..."
# lines from <nextest-log>, appends a row to $GITHUB_STEP_SUMMARY, writes a
# one-line TSV row to <report-file> (uploaded so the Nightly Summary job can
# build the cross-leg table), and annotates every failed test.
#
# Exit status: 1 when the suite executed ZERO tests (link failure, missing SDK,
# a step that died before nextest started -- the failure mode that hid every
# PHP-linked test for weeks behind continue-on-error, v0.13.0 audit H1) or when
# any test failed. 0 otherwise.
set -u

log=$1
label=$2
report=$3

clean=$(mktemp)
if [ -f "$log" ]; then
  # Drop ANSI colour codes so the patterns below see plain text. (A literal
  # ESC, not `\x1b`: BSD sed on the macOS runner has no `\x` escapes.)
  esc=$(printf '\033')
  sed "s/${esc}\[[0-9;]*m//g" "$log" > "$clean"
fi

started=$(grep -aoE 'Starting [0-9]+ tests?' "$clean" | head -1 | grep -oE '[0-9]+' || true)
summary=$(grep -aE '^ *Summary \[' "$clean" | tail -1 || true)
num() { printf '%s\n' "$summary" | grep -oE "[0-9]+ $1" | head -1 | grep -oE '^[0-9]+' || true; }
ran=$(num 'tests? run')
passed=$(num 'passed')
failed=$(num 'failed')
timed_out=$(num 'timed out')
skipped=$(num 'skipped')
: "${started:=0}" "${ran:=0}" "${passed:=0}" "${failed:=0}" "${timed_out:=0}" "${skipped:=0}"
failed=$((failed + timed_out))

# Failed tests, from the recap nextest prints after the Summary line.
failures=$(awk '/^ *Summary \[/{s=1; next} s' "$clean" \
  | grep -aE '^ *(FAIL|TIMEOUT|SIG[A-Z]+|ABORT|LEAK-FAIL) \[' \
  | sed -E 's/^ *([A-Z-]+) \[[^]]*\] *(\([^)]*\) *)?/\1 /' | sort -u || true)

if [ "$ran" -eq 0 ]; then
  status="NOT RUN"
elif [ "$failed" -gt 0 ]; then
  status="FAILED"
else
  status="passed"
fi

printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$status" "$ran" "$passed" "$failed" "$skipped" > "$report"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### PHP-linked tests: $label"
    echo ""
    echo "| Status | Started | Run | Passed | Failed | Skipped |"
    echo "|--------|---------|-----|--------|--------|---------|"
    echo "| $status | $started | $ran | $passed | $failed | $skipped |"
    if [ -n "$failures" ]; then
      echo ""
      echo '```'
      printf '%s\n' "$failures"
      echo '```'
    fi
  } >> "$GITHUB_STEP_SUMMARY"
fi

echo "PHP-linked tests ($label): $status -- started $started, run $ran, passed $passed, failed $failed, skipped $skipped"
rm -f "$clean"

if [ "$ran" -eq 0 ]; then
  echo "::error title=PHP-linked tests did not run ($label)::nextest executed 0 tests -- the suite failed to build or link, or never started. See the 'Run PHP-linked tests' step log."
  exit 1
fi
if [ "$failed" -gt 0 ]; then
  while IFS= read -r line; do
    [ -n "$line" ] && echo "::error title=PHP-linked test failed ($label)::$line"
  done <<< "$failures"
  exit 1
fi
exit 0
