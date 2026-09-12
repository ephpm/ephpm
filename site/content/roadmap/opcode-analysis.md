# Engine-Backed Code Analysis — Opcode Scanning, Taint, Detonation

> **Status:** L0 (text scan) and L1 (opcode-level sink detection, the
> opt-in `opcode-scan` analyzer) are **shipped**. Everything after that —
> superglobal taint tracking, the shared compiled-tree representation, and
> L2 detonation — is **Planned — not yet implemented**.

## The moat

Every other PHP static analyzer — PHPStan, Psalm, semgrep — re-implements
PHP parsing in its own front end and never touches the runtime. ePHPm
*embeds* the runtime. `ephpm analyze` can therefore compile each PHP file
with the **same Zend compiler that would execute it** and analyze the
resulting opcode stream: no parser drift, no comments, no
strings-mistaken-for-code, and compiler lowerings for free (the backtick
operator arrives as a genuine `shell_exec` call site). An opcode-level
match is evidence about what would actually run — which is why
`opcode-scan` findings carry `confirmed` confidence and can participate in
hard-deny escalation, where a text-scan hotspot never can.

## Levels

| Level | What | Status |
|-------|------|--------|
| L0 | `dangerous-sinks` — naive text/token pass, `suspected` confidence | Shipped (default profile) |
| L1 | `opcode-scan` — compile to opcodes (never execute), detect `eval` + dangerous statically-named calls, `confirmed` confidence | **Shipped, opt-in** — see the [`ephpm analyze` reference](/reference/cli/analyze/) |
| L1.5 | Superglobal taint: flag when a sink's argument operand is fed by a `ZEND_FETCH_*` of `$_GET`/`$_POST`/`$_REQUEST`/`$_COOKIE`, bumping severity for the one-hop request-data→sink shape | Planned — not yet implemented |
| L2 | Engine-in-the-loop detonation: sandboxed evaluation of suspicious inputs (the `engine:` config section, parsed but inert today) | Planned — not yet implemented |

## What L1 ships (and does not)

Shipped: per-file compile via `zend_compile_file` under
`ZEND_COMPILE_WITHOUT_EXECUTION` (the `opcache_compile_file()` model —
nothing executes), a walk of the top-level op_array plus every nested
op_array (functions, methods, closures, arrow functions, conditional
declarations), detection of the `eval` construct and statically-named
calls to `system` / `exec` / `shell_exec` / `passthru` / `proc_open` /
`popen` / `create_function` / `assert` / `unserialize`, with source line
numbers from the opcodes. Requires a PHP-linked build; stub builds skip
the analyzer with a diagnostic.

Deliberately not in L1 (known false negatives, same as any opcode-only
pass): dynamic calls (`$f()`, `call_user_func`), names built by string
concatenation, and variable-variable tricks. Those are exactly the shapes
the planned taint pass and L2 detonation exist to chase — do not expect
`opcode-scan` alone to catch an obfuscated dropper; pair it with
`malware-yara` and the rest of the `security` profile.

Design seams already in place for the next phases: the `Analyzer` trait,
finding shape, and policy engine are final; `AnalysisCtx` reserves a
lazily-built shared compiled representation for when a second
opcode-level pass exists; the `engine:` config section is parsed (and
warns when set) so L2 configs are shaped now.
