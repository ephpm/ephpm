# Unit & Integration Test Coverage

Current state of test coverage and gaps to fill.

---

## Summary

| Crate | Unit (src/) | Integration (tests/) | Stub-mode | Total |
|-------|:-----------:|:--------------------:|:---------:|:-----:|
| ephpm-config | 19 | 0 | 19 | 19 |
| ephpm-php | 40 (18 + 22 php_linked) | 0 | 18 | 40 |
| ephpm-server | 42 | 0 | 42 | 42 |
| ephpm-kv | 109 | 49 (48 + 1 doctest) | 158 | 158 |
| ephpm-db | 5 | 0 | 5 | 5 |
| ephpm | 0 | 3 (php_linked) | 0 | 3 |
| **Total** | **215** | **52** | **242** | **267** |

*"Stub-mode" = tests that run without PHP linked (the default `cargo test` experience).*

---

## ephpm-config (19 tests)

**File**: `crates/ephpm-config/src/lib.rs`

| Test | What it covers |
|------|----------------|
| `test_default_config` | Default configuration loads correctly |
| `test_load_valid_toml` | Parses complete TOML config |
| `test_load_partial_toml_fills_defaults` | Missing fields use defaults |
| `test_load_missing_file_uses_defaults` | Non-existent file falls through to defaults |
| `test_env_var_overrides_toml` | EPHPM_ env vars override TOML values |
| `test_env_var_override_without_file` | Env vars work standalone |
| `test_ini_overrides_from_toml` | PHP ini_overrides loaded from TOML |
| `test_php_etag_cache_defaults` | ETag cache default values |
| `test_php_etag_cache_from_toml` | ETag cache from TOML |
| `test_php_etag_cache_indefinite_ttl` | ETag cache with ttl=-1 (indefinite) |
| `test_kv_compression_defaults` | KV compression default settings |
| `test_kv_compression_gzip_from_toml` | Gzip compression config |
| `test_kv_compression_zstd_from_toml` | Zstd compression config |
| `test_kv_compression_brotli_from_toml` | Brotli compression config |
| `test_env_var_overrides_php_etag_cache` | Env vars override ETag cache |
| `test_env_var_overrides_kv_compression` | Env vars override KV compression |
| `test_env_var_overrides_compression_level` | Env var overrides compression level |
| `test_env_var_overrides_compression_min_size` | Env var overrides min size |
| `test_combined_php_etag_and_compression_config` | Combined ETag + compression config |

---

## ephpm-php (40 tests total, 18 stub-mode)

### request.rs (12 tests)

| Test | What it covers |
|------|----------------|
| `test_server_variables_core_fields` | REQUEST_METHOD, REQUEST_URI, SERVER_NAME, SERVER_PORT |
| `test_server_variables_script_paths` | SCRIPT_FILENAME, DOCUMENT_ROOT, SCRIPT_NAME |
| `test_server_variables_rewritten_request` | Fallback rewrite sets correct paths |
| `test_server_variables_http_header_mapping` | HTTP headers to $_SERVER mapping |
| `test_server_variables_host_header` | Host header to HTTP_HOST |
| `test_server_variables_content_type_no_http_prefix` | CONTENT_TYPE (no HTTP_ prefix) |
| `test_server_variables_content_length_no_http_prefix` | CONTENT_LENGTH header |
| `test_server_variables_https_on` | HTTPS=on when is_https=true |
| `test_server_variables_https_absent_when_false` | HTTPS absent when is_https=false |
| `test_cookie_string_found` | Cookie header extracted |
| `test_cookie_string_missing` | Missing cookie returns empty |
| `test_cookie_string_case_insensitive` | Case-insensitive cookie lookup |

### lib.rs (6 stub-mode tests)

| Test | What it covers |
|------|----------------|
| `test_stub_init_succeeds` | Init succeeds in stub mode |
| `test_stub_init_is_idempotent` | Double init is safe |
| `test_stub_shutdown_after_init` | Shutdown after init |
| `test_stub_shutdown_without_init` | Shutdown without init is no-op |
| `test_stub_execute_returns_stub_html` | Stub execution returns placeholder HTML |
| `test_stub_execute_without_init_returns_not_initialized` | Not-initialized error |

### kv_bridge.rs (22 tests, all php_linked)

| Test | What it covers |
|------|----------------|
| `get_missing_returns_zero` | kv_get on missing key |
| `set_and_get_round_trip` | Set/get value round-trip |
| `get_result_reflects_thread_local_after_get` | Thread-local buffer state |
| `set_stores_value` | kv_set stores correctly |
| `set_with_ttl_stores_value_with_expiry` | TTL sets expiry |
| `set_with_zero_ttl_stores_without_expiry` | Zero TTL = no expiry |
| `set_handles_binary_value` | Binary data including null bytes |
| `del_existing_returns_one` | Delete existing key |
| `del_missing_returns_zero` | Delete missing key |
| `exists_present_returns_one` | EXISTS present key |
| `exists_absent_returns_zero` | EXISTS absent key |
| `incr_by_creates_key` | INCR creates new key |
| `incr_by_delta_accumulates` | INCR accumulates |
| `incr_by_negative_decrements` | Negative delta decrements |
| `incr_by_non_integer_returns_zero` | Non-integer error |
| `expire_sets_ttl_on_existing_key` | EXPIRE sets TTL |
| `expire_on_missing_key_returns_zero` | EXPIRE on missing |
| `expire_zero_or_negative_returns_zero` | Invalid TTL |
| `pttl_no_expiry_returns_minus_one` | PTTL no-expiry |
| `pttl_missing_key_returns_minus_two` | PTTL missing |
| `pttl_with_expiry_returns_positive` | PTTL with expiry |
| `get_buffer_is_thread_local` | Thread isolation |

---

## ephpm-server (42 tests)

### router.rs (36 tests)

| Group | Count | Tests |
|-------|:-----:|-------|
| Fallback resolution | 7 | `test_existing_file_matches_uri`, `test_existing_php_file_matches_uri`, `test_directory_with_index_matches_uri_slash`, `test_directory_falls_to_index_html`, `test_permalink_falls_to_index_php`, `test_missing_file_with_404_fallback`, `test_missing_php_with_404_fallback`, `test_missing_with_no_index_falls_to_fallback`, `test_subdirectory_with_index` |
| Helpers | 3 | `test_expand_variables`, `test_split_path_query`, `test_is_php_file_check` |
| Security | 1 | `test_has_hidden_segment` |
| Compression | 4 | `test_gzip_compress_small_body`, `test_gzip_compress_non_compressible`, `test_gzip_compress_html`, `test_gzip_compress_custom_min_size` |
| Trusted proxies | 2 | `test_resolve_xff_rightmost_untrusted`, `test_resolve_xff_all_trusted_uses_leftmost` |
| Port parsing | 2 | `test_new_parses_port`, `test_new_defaults_port_when_invalid` |
| Blocked paths | 3 | `test_blocked_exact_path`, `test_blocked_wildcard_directory`, `test_blocked_extension_wildcard` |
| PHP path allow | 3 | `test_php_allowed_empty_allows_all`, `test_php_allowed_exact_match`, `test_php_allowed_wildcard_directory` |
| Glob matching | 3 | `test_glob_match_exact`, `test_glob_match_star_segment`, `test_glob_match_star_catches_directory` |
| ETag caching | 6 | `test_php_etag_cache_key_without_query`, `test_php_etag_cache_key_with_query`, `test_etag_matches_value_exact`, `test_etag_matches_value_wildcard`, `test_etag_matches_value_comma_separated`, `test_etag_matches_value_with_whitespace` |

### static_files.rs (6 tests)

| Test | What it covers |
|------|----------------|
| `test_serve_html_file` | HTML content-type |
| `test_serve_css_file` | CSS content-type |
| `test_serve_content_length_header` | Content-Length set |
| `test_serve_unknown_extension` | Falls back to `application/octet-stream` |
| `test_serve_missing_file_returns_404` | 404 for missing |
| `test_serve_path_traversal_blocked` | `../` blocked |

---

## ephpm-kv (158 tests: 109 unit + 48 integration + 1 doctest)

### command.rs (67 unit tests)

| Group | Count | Coverage |
|-------|:-----:|----------|
| Connection | 5 | PING (bare, with message), ECHO, SELECT, QUIT, COMMAND |
| GET/SET | 12 | Round-trip, overwrite, EX/PX TTL, NX/XX flags, SET...GET option, SETEX (valid, invalid TTL, wrong args), SETNX |
| MSET/MGET | 2 | Multi-key set/get, odd-args error |
| DEL/EXISTS | 5 | Single/multi-key delete, existence checks, multiple keys count |
| INCR/DECR | 7 | Create, increment, decrement, INCRBY, DECRBY, non-integer value error, non-integer delta error |
| APPEND/STRLEN/GETSET | 5 | Create, concatenate, length, atomic swap, missing key |
| TTL/EXPIRE | 9 | TTL/PTTL semantics, EXPIRE/PEXPIRE set and missing, PERSIST remove + no-ttl, TYPE existing + missing |
| RENAME | 3 | Rename existing, missing key error, TTL preservation |
| KEYS/DBSIZE/FLUSH | 5 | Wildcard, pattern filter, count, FLUSHDB, FLUSHALL |
| INFO | 1 | Returns bulk with redis_version |
| Error handling | 7 | Unknown command, missing args (GET/SET/DEL/MGET/MSET), invalid expire time, case insensitivity |
| Additional | 6 | Edge cases: missing args for individual commands |

### store/mod.rs (23 unit tests)

| Test | What it covers |
|------|----------------|
| `set_and_get` | Basic set/get |
| `missing_key` | Get returns None for missing |
| `overwrite` | Set overwrites existing |
| `delete` | Remove key |
| `exists` | Key existence check |
| `ttl_expiry` | Key expires after TTL |
| `pttl_no_expiry` | PTTL=-1 for no-expiry key |
| `pttl_missing` | PTTL=-2 for missing key |
| `incr` | Integer increment |
| `incr_non_integer` | Increment on non-integer fails |
| `append_new_key` | Append creates new key |
| `append_existing` | Append concatenates |
| `flush` | Clear all keys |
| `keys_pattern` | KEYS with glob pattern |
| `glob_matching` | Glob `*` and `?` matching |
| `expire_pass_reaps` | Background reaper removes expired |
| `noeviction_rejects_writes` | NoEviction policy rejects over-limit |
| `compression_gzip_round_trip` | Gzip compress/decompress |
| `compression_brotli_round_trip` | Brotli compress/decompress |
| `compression_zstd_round_trip` | Zstd compress/decompress |
| `compression_below_min_size_not_compressed` | Below threshold = no compression (**DEADLOCKS** - known bug) |
| `compression_incr_by_works` | INCR works on compressed values |
| `compression_append_works` | APPEND works on compressed values |

### resp/frame.rs (7 unit tests)

Serialization of RESP frames: simple string, error, integer, bulk, null, array, empty bulk.

### resp/parse.rs (12 unit tests)

Parsing of RESP wire format: simple/error/integer/bulk/null/array, empty array, incomplete input, invalid type byte, buffer consumption.

### tests/resp_compat.rs (48 integration tests)

Full RESP protocol over TCP: PING, SET/GET (incl. binary, NX, XX, EX, PX, GET option), MSET/MGET, DEL, EXISTS, INCR/DECR/INCRBY/DECRBY, APPEND/STRLEN/GETSET, TTL/EXPIRE/PEXPIRE/PERSIST, TYPE, KEYS/DBSIZE/FLUSHDB/FLUSHALL, INFO, two-connection shared data, pipeline, SETNX, error handling.

---

## ephpm-db (5 tests)

| File | Tests | Coverage |
|------|:-----:|----------|
| `duration.rs` | 2 | Parse valid durations (ms/s/m/h), reject invalid |
| `url.rs` | 3 | MySQL URL, PostgreSQL URL with encoded password, default ports |

---

## ephpm (3 integration tests, php_linked only)

**File**: `crates/ephpm/tests/kv_sapi_integration.rs`

| Test | What it covers |
|------|----------------|
| `kv_sapi_set_get` | PHP KV set/get through SAPI |
| `kv_sapi_del` | PHP KV delete through SAPI |
| `kv_sapi_all` | Full KV SAPI integration |

---

## Coverage Matrix

| Area | Tests | Status |
|------|:-----:|:------:|
| Config: TOML, env vars, defaults | 19 | Good |
| PHP: $_SERVER variable mapping | 12 | Good |
| PHP: Stub mode init/execute/shutdown | 6 | Good |
| PHP: KV bridge (C FFI) | 22 | Good (php_linked) |
| Server: Routing fallback resolution | 9 | Good |
| Server: Security (hidden files, blocked paths, PHP allow) | 7 | Good |
| Server: Compression (gzip) | 4 | Good |
| Server: Trusted proxy / XFF | 2 | Good |
| Server: Port parsing | 2 | Good |
| Server: Glob matching | 3 | Good |
| Server: ETag caching | 6 | Good |
| Server: Static file serving | 6 | Good |
| KV: Redis commands | 67 | Good |
| KV: In-memory store + compression | 23 | Good |
| KV: RESP serialize/parse | 19 | Good |
| KV: RESP integration (TCP) | 48 | Good |
| DB: URL/duration parsing | 5 | Good |
| **Server: TLS cert loading** | **0** | **Gap** |
| **Server: Memory size parsing** | **0** | **Gap** |
| **Server: ACME provisioning** | **0** | **Gap** |
| **KV: Eviction policies (LRU, random, volatile)** | **0** | **Gap** |
| **KV: Memory tracking** | **0** | **Gap** |
| **Cluster: Config parsing** | **0** | **Gap** |
| **Cluster: Gossip protocol** | **0** | **Gap (crate not yet created)** |
| **Cluster: Clustered store routing** | **0** | **Gap (crate not yet created)** |
| **CLI: Argument parsing** | **0** | **Gap** |
| **PHP: Worker pool** | **0** | **Gap (php_linked only)** |
| **PHP: SAPI callbacks** | **0** | **Gap** |
| **Server: Graceful shutdown** | **0** | **Gap** |
| **Server: Connection handling** | **0** | **Gap** |

---

## Tests To Build

### High Priority — Testable Now

#### 1. TLS Certificate Loading (`ephpm-server/src/tls.rs`)

- [ ] Load valid PEM certificate (RSA)
- [ ] Load valid PEM certificate (EC)
- [ ] Reject invalid/malformed certificate
- [ ] Reject invalid/malformed key
- [ ] Reject mismatched cert/key pair
- [ ] Missing cert file returns clear error
- [ ] Missing key file returns clear error

*Requires openssl CLI for cert generation in tests.*

#### 2. Memory Size Parsing (`ephpm-server/src/lib.rs`)

- [ ] Parse "256MB" -> bytes
- [ ] Parse "1GB" -> bytes
- [ ] Parse "512KB" -> bytes
- [ ] Parse raw bytes (no suffix)
- [ ] Lowercase suffix ("256mb")
- [ ] Whitespace trimming (" 256MB ")
- [ ] Invalid input returns error
- [ ] Zero returns 0

#### 3. KV Eviction Policies (`ephpm-kv/src/store/mod.rs`)

- [ ] `EvictionPolicy` from-string parsing (all 4 variants + unknown fallback)
- [ ] `AllKeysLru`: evicts to make room for new write
- [ ] `AllKeysLru`: evicts least-recently-accessed key (not just oldest)
- [ ] `VolatileLru`: only evicts keys with TTL set
- [ ] `VolatileLru`: fails when only persistent keys exist
- [ ] `AllKeysRandom`: random eviction succeeds
- [ ] `NoEviction`: rejects writes when at memory limit
- [ ] Eviction frees enough space for large writes (multiple keys)
- [ ] Eviction on empty store fails (nothing to evict)
- [ ] Unlimited memory (limit=0) accepts any size

#### 4. KV Memory Tracking (`ephpm-kv/src/store/mod.rs`)

- [ ] `mem_used` increases on insert
- [ ] `mem_used` decreases on remove
- [ ] `mem_used` adjusts on overwrite (larger and smaller values)

#### 5. CLI Argument Parsing (`ephpm/src/main.rs`)

- [ ] No subcommand defaults to serve
- [ ] `serve` with defaults
- [ ] `serve` with all flags (--config, --listen, --document-root, -vv)
- [ ] `kv get <key>` parses
- [ ] `kv set <key> <value> --ttl 60` parses
- [ ] `kv del` without keys fails
- [ ] `kv del a b c` multiple keys
- [ ] `kv incr` defaults by=1
- [ ] `kv --host --port` custom connection
- [ ] `kv keys` defaults pattern="*"
- [ ] Invalid flag returns error
- [ ] `--version` flag

*Requires openssl-sys to link; can verify with `cargo check --tests` only.*

#### 6. Cluster Config Parsing (`ephpm-config/src/lib.rs`)

- [ ] `ClusterConfig` defaults (bind, cluster_id)
- [ ] `ClusterConfig` from TOML with all fields
- [ ] `ClusterKvConfig` defaults
- [ ] `ClusterKvConfig` from TOML
- [ ] Env var overrides for cluster settings
- [ ] Partial TOML fills defaults

*Depends on `ClusterConfig` struct being added to the config crate.*

#### 7. Router Edge Cases (`ephpm-server/src/router.rs`)

- [ ] `is_compressible()`: text types compressible, binary types not
- [ ] `segment_match()`: exact, prefix-star, suffix-star, prefix-star-suffix
- [ ] `has_hidden_segment()`: dot-only not hidden, deep nesting
- [ ] `is_php_file()`: case insensitive, non-PHP returns false
- [ ] `gzip_compress()`: JSON, SVG, disabled for binary
- [ ] `etag_matches_value()`: empty If-None-Match, strong ETag
- [ ] Blocked paths: empty list blocks nothing, multiple patterns
- [ ] IPv6 listen address parsing
- [ ] `glob_match()`: directory prefix, no-wildcard exact

#### 8. Static File Edge Cases (`ephpm-server/src/static_files.rs`)

- [ ] JavaScript file content-type
- [ ] PNG image content-type
- [ ] Empty file (Content-Length: 0)
- [ ] Nested directory paths
- [ ] Binary file data integrity

### Medium Priority — Requires Infrastructure

#### 9. ACME Certificate Provisioning (`ephpm-server/src/acme.rs`)

- [ ] ACME config with staging/production directory
- [ ] Certificate cache directory creation
- [ ] Domain validation
- [ ] Certificate renewal threshold

*Requires mocking ACME directory or test server (pebble).*

#### 10. Server Connection Handling

- [ ] HTTP/1.1 connection served
- [ ] HTTP/2 via ALPN
- [ ] Connection timeout
- [ ] IPv6 listen

*Requires spawning real server.*

#### 11. Graceful Shutdown

- [ ] Shutdown signal sets flag
- [ ] New connections rejected
- [ ] In-flight requests complete
- [ ] Timeout respected

*Requires spawning real server.*

### Lower Priority — Requires php_linked

#### 12. PHP Worker Pool (`ephpm-php/src/lib.rs`)

- [ ] Pool creates configured workers
- [ ] Request dispatched to available worker
- [ ] Backpressure when all workers busy
- [ ] Graceful pool shutdown
- [ ] Concurrent dispatch returns correct results

#### 13. PHP SAPI Callbacks (`ephpm-php/src/sapi.rs`)

- [ ] `sapi_header_handler` sets response headers
- [ ] `sapi_send_headers` flushes buffer
- [ ] `sapi_read_post` reads request body
- [ ] `sapi_read_cookies` reads cookie string
- [ ] `log_message` routes to tracing

#### 14. PHP Runtime Edge Cases

- [ ] Request timeout (SIGPROF -> 504)
- [ ] Memory limit exceeded -> 500
- [ ] Concurrent requests get isolated state
- [ ] Binary response body passed through intact

### Known Bugs

#### KV Store DashMap Deadlock

`compression_below_min_size_not_compressed` test deadlocks: it holds a DashMap read guard via `s.data.get()` then calls `s.get()` which tries `get_mut()` on the same key. The test hangs and must be skipped (`--skip compression_below_min_size`).

Fix: drop the read guard before calling `s.get()`.
