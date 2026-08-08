+++
title = "Diagnosing Crashes"
weight = 14
+++

Native code in the ePHPm process — a PHP extension, a
[native middleware](/guides/native-middleware/) `.so`, or the Zend engine itself —
can fault in ways no amount of Rust safety prevents. When that happens the
whole process dies: there is one address space, and the Zend memory manager,
the glibc heap and the OPcache shared memory are shared by every thread in it.

ePHPm cannot recover from that, and does not try. What it does is **tell you
what happened** before it goes.

## What you get

On `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE` or `SIGABRT`, ePHPm writes a block
like this to stderr and then dies:

```
=== ephpm fatal signal ===
signal:   SIGSEGV (11)  si_code=1 SEGV_MAPERR (address not mapped)
fault_at: 0x0000000000000000  (null or near-null: a null-pointer dereference)
thread:   "tokio-rt-worker" tid=49 pid=10
version:  ephpm 0.1.0
altstack: yes
frames:   15 captured, repeats collapsed (for ephpm's own frames feed the +0x offsets to `addr2line -Cfpe <ephpm binary>`)
  #00 0x0000614614d1bde8
  #01 0x000070f1608e0050
  #02 0x000070f16061f3ae
  ...
symbols:
ephpm(+0x6b1bde8)[0x614614d1bde8]
/lib/x86_64-linux-gnu/libc.so.6(+0x3c050)[0x70f1608e0050]
/opt/myext/crash_ext.so(zif_ephpm_crash+0x9c)[0x70f16061f3ae]
ephpm(execute_ex+0x5f27)[0x61461661d647]
ephpm(zend_execute+0x196)[0x614616622a96]
ephpm(zend_execute_script+0x62)[0x61461655b9d2]
ephpm(php_execute_script_ex+0x1c2)[0x6146166380c2]
ephpm(ephpm_execute_request+0xac9)[0x6146178ff8e9]
=== end ephpm fatal signal ===
```

The line that answers the question is usually in `symbols:`. Here it names the
extension (`crash_ext.so`) and the exact function (`zif_ephpm_crash`), sitting
under the full Zend call chain — so this was PHP userland calling into a
broken extension, not the engine misbehaving on its own.

## Reading it

**`signal` / `si_code`.** The signal says *how* the process died; `si_code`
says *why the kernel raised it*.

| What you see | What it usually means |
|---|---|
| `SIGSEGV` + `SEGV_MAPERR` | dereferenced an address that isn't mapped — a null or wild pointer |
| `SIGSEGV` + `SEGV_ACCERR` | the page is mapped but not accessible — most often a **stack-overflow** guard-page hit |
| `SIGBUS` + `BUS_ADRERR` | touched an `mmap`'d file past its end (a truncated SQLite file, for example) |
| `SIGABRT` + `SI_TKILL` | something called `abort()`: a glibc heap check, or Rust's stack-overflow handler |
| `SIGFPE` + `FPE_INTDIV` | integer division by zero in native code |

**`fault_at`.** The faulting address. `0x0` (and anything under `0x1000`) is
flagged inline as a null-pointer dereference, which is by far the most common
extension bug.

**`thread`.** PHP requests execute on tokio's blocking pool; middleware runs on
async worker threads. Both are named `tokio-rt-worker`, so the thread name
tells you it was ePHPm's runtime rather than a thread a library spawned for
itself — it does not tell you which request was in flight (see
[caveats](#caveats)).

**`frames` / `symbols`.** Two views of the same stack. The raw addresses are
written first and always succeed; the symbol lines are resolved afterwards and
can fail. Runs of the same address are collapsed with a `(xN, recursion)`
marker, so a runaway recursion is four lines instead of a hundred and thirty.

**`altstack`.** Expected to say `yes` on every crash. If it ever says `NO`, the
alternate signal stack did not take effect and a genuine stack-overflow fault
could not have been reported at all — that is a bug worth filing.

## Resolving ePHPm's own frames

Shared objects (`.so` files — PHP extensions and native middleware) export
dynamic symbols, so they show up by name. So does PHP itself, because `libphp`
exports its symbols for extensions to link against. **The ephpm binary's own
Rust frames do not** — they appear as `ephpm(+0x6dc4cb0)`.

Resolve them with the `+0x` offset (not the absolute address, which changes on
every run under ASLR), against the exact binary that crashed:

```bash
addr2line -Cfpe /usr/local/bin/ephpm 0x6dc4cb0
```

## Stack overflows

A stack overflow produces `SIGSEGV` + `SEGV_ACCERR`, and the report is followed
by Rust's own message and a note that the process is going down:

```
=== end ephpm fatal signal ===

thread 'tokio-rt-worker' (47) has overflowed its stack
fatal runtime error: stack overflow, aborting
[ephpm] SIGABRT raised by the chained handler (expected after a stack overflow) - terminating
```

That second block comes from the Rust standard library, which owns the
guard-page range and is the authority on whether a fault really was a stack
overflow. ePHPm chains to it rather than replacing it. The final exit status is
134 (`SIGABRT`), not 139 — which is what it was before this reporting existed.

The backtrace for this case is honest but not very useful: every captured frame
is the same recursing function, so you get its name and a repeat count and
nothing about the caller. `backtrace(3)` unwinds inward-out and stops at 64
frames, which a runaway recursion consumes entirely.

## Exit status is unchanged

The handler always restores the default disposition and re-raises, so the
process dies with exactly the status it would have had without it:

| Fault | Exit status |
|---|---|
| null dereference, wild write | 139 (`SIGSEGV`) |
| stack overflow, `abort()`, glibc double-free | 134 (`SIGABRT`) |

Kubernetes still records `Error(139)` / `Error(134)`, CrashLoopBackOff
detection is unaffected, and alerting keyed on exit code keeps working.

## Turning it off

The handler is on by default and costs nothing until something crashes — one
`sigaction` per signal plus a single warm-up call at startup.

If it ever misbehaves, disable it without rebuilding:

```bash
EPHPM_FATAL_HANDLER=0
```

Crashes then become silent again, exactly as they were before. The exit status
does not change either way. See
[Environment Variables](/reference/environment-variables/).

## Caveats

**No request or vhost is recorded.** The report names the thread, not the URL.
This is deliberate: for a corrupting bug the crashing thread is frequently
*not* the guilty one. A Zend-memory-manager smash has been observed returning
HTTP 200 to its own client and killing the process 17 ms later on a different
thread serving an unrelated request — naming that request would point at the
victim, not the cause.

**Symbolisation can fail.** Resolving names takes the dynamic-loader lock, so a
thread that faulted inside `dlopen` could block there. The raw addresses are
printed before that is attempted, precisely so you keep something usable if it
happens.

**Unix only.** Windows delivers memory faults as SEH exceptions rather than
signals; that path is not implemented, and `EPHPM_FATAL_HANDLER` is ignored
there.

**Not every corruption crashes promptly.** Heap and Zend-allocator corruption
can leave the process running for a while — or indefinitely. When it does
finally crash, the report describes where it *died*, which may be far from
where it broke.

## See also

- [PHP Extensions](/guides/php-extensions/) — loading shared extensions.
- [Native Middleware](/guides/native-middleware/) — the other `.so` in the process.
- [Environment Variables](/reference/environment-variables/) — `EPHPM_FATAL_HANDLER`.
