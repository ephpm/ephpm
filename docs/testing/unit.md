# Unit & Integration Test Coverage

Current state of test coverage and gaps to fill.

---

## Summary

| Crate | Unit (src/) | Integration (tests/) | Stub-mode | Total |
|-------|:-----------:|:--------------------:|:---------:|:-----:|
| ephpm-config | 19 | 0 | 19 | 19 |
| ephpm-php | 40 (18 + 22 php_linked) | 0 | 18 | 40 |
| ephpm-server | 84 (79 + 5 php_linked) | 0 | 84 | 84 |
| ephpm-kv | 125 | 49 (48 + 1 doctest) | 174 | 174 |
| ephpm-db | 5 | 0 | 5 | 5 |
| ephpm | 0 | 3 (php_linked) | 0 | 3 |
| **Total** | **273** | **52** | **300** | **325** |

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

## ephpm-server (84 tests: 62 router + 11 static + 7 tls + 8 lib, minus 5 php_linked = 79 stub-mode)

### router.rs (62 tests, 57 stub-mode + 5 php_linked)

| Group | Count | Coverage |
|-------|:-----:|----------|
| Fallback resolution | 9 | Static file, PHP file, directory index (php+html), permalink fallback, 404 fallback, missing php, no-index fallback, subdirectory index |
| Helpers | 3 | Variable expansion, path/query splitting, PHP file detection |
| Security: hidden files | 3 | Dotfile detection (.env, .git, .htaccess), dot-only not hidden, deep nesting |
| Security: blocked paths | 5 | Exact path, wildcard directory, extension wildcard, empty list, multiple patterns |
| Security: PHP allow | 3 | Empty=all allowed, exact match, wildcard directory |
| Compression | 7 | Small body skip, non-compressible type, HTML, custom min_size, JSON, SVG, binary not compressed |
| `is_compressible()` | 3 | Text types, application types (JS/JSON/XML/SVG), binary rejection |
| `segment_match()` | 5 | Exact, star-any, prefix-star, suffix-star, prefix-star-suffix |
| `is_php_file()` | 2 | Case insensitive, non-PHP extensions |
| Trusted proxies / XFF | 2 | Rightmost untrusted IP, all-trusted leftmost fallback |
| Port parsing | 3 | Listen address, default on invalid, IPv6 listen |
| Glob matching | 5 | Exact, single segment, directory catch, directory prefix, no-wildcard exact |
| ETag caching | 8 | Cache key (with/without query), exact/wildcard/comma/whitespace match, empty match, strong ETag |
| PHP-linked ETag | 5 | Store on first req, 304 on match, no-cache bypass, POST skip, content change (php_linked) |

### static_files.rs (11 tests)

| Test | What it covers |
|------|----------------|
| `test_serve_html_file` | HTML content-type |
| `test_serve_css_file` | CSS content-type |
| `test_serve_content_length_header` | Content-Length set |
| `test_serve_unknown_extension` | Falls back to `application/octet-stream` |
| `test_serve_missing_file_returns_404` | 404 for missing |
| `test_serve_path_traversal_blocked` | `../` blocked |
| `test_serve_javascript_file` | JavaScript content-type |
| `test_serve_png_image` | PNG image content-type |
| `test_serve_empty_file` | Empty file returns Content-Length: 0 |
| `test_serve_nested_path` | Nested directory paths |
| `test_serve_binary_file_intact` | Binary data integrity (all 256 bytes) |

### tls.rs (7 tests)

| Test | What it covers |
|------|----------------|
| `load_valid_rsa_cert_and_key` | Load RSA cert + key |
| `load_valid_ec_cert_and_key` | Load EC cert + key |
| `missing_cert_file_returns_error` | Missing cert file error |
| `missing_key_file_returns_error` | Missing key file error |
| `invalid_cert_pem_returns_error` | Malformed cert error |
| `invalid_key_pem_returns_error` | Malformed key error |
| `mismatched_cert_key_returns_error` | Cert/key mismatch error |

### lib.rs (8 tests)

| Test | What it covers |
|------|----------------|
| `parse_memory_size_megabytes` | "256MB" -> 268435456 |
| `parse_memory_size_gigabytes` | "1GB" -> 1073741824 |
| `parse_memory_size_kilobytes` | "512KB" -> 524288 |
| `parse_memory_size_bytes_no_suffix` | "1024" -> 1024 |
| `parse_memory_size_lowercase` | "256mb" -> 268435456 |
| `parse_memory_size_with_whitespace` | " 256MB " trimming |
| `parse_memory_size_invalid` | "notanumber" returns error |
| `parse_memory_size_zero` | "0" -> 0 |

---

## ephpm-kv (174 tests: 125 unit + 48 integration + 1 doctest)

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

### store/mod.rs (39 unit tests)

| Group | Count | Coverage |
|-------|:-----:|----------|
| Basic operations | 6 | set/get, missing key, overwrite, delete, exists, ttl_expiry |
| TTL / PTTL | 2 | PTTL no-expiry (-1), PTTL missing (None) |
| Increment | 2 | Counter increment, non-integer error |
| Append | 2 | Create key, concatenate existing |
| Flush | 1 | Clear all keys + mem_used resets |
| Pattern matching | 2 | KEYS with pattern, glob matching |
| Expiry pass (GC) | 1 | Cleanup removes expired entries |
| Compression | 6 | Gzip/Brotli/Zstd round-trip, below-min-size, INCR+APPEND on compressed |
| Eviction: policy parsing | 1 | All 4 variants + unknown fallback to AllKeysLru |
| Eviction: AllKeysLru | 3 | Evicts to make room, evicts oldest-accessed key, frees multiple keys |
| Eviction: VolatileLru | 2 | Only evicts TTL keys, fails with only persistent keys |
| Eviction: AllKeysRandom | 1 | Random eviction makes room |
| Eviction: NoEviction | 2 | Rejects writes when full, rejects at limit |
| Eviction: edge cases | 2 | Empty store fails, unlimited memory accepts any |
| Memory tracking | 3 | Insert/remove tracking, overwrite tracking, flush resets |
| Glob matching | 3 | `?` wildcard, combined `*`/`?`, empty pattern |

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
| Server: Security (hidden files, blocked paths, PHP allow) | 11 | Good |
| Server: Compression (gzip + is_compressible) | 10 | Good |
| Server: Trusted proxy / XFF | 2 | Good |
| Server: Port parsing | 3 | Good |
| Server: Glob matching | 5 | Good |
| Server: ETag caching | 10 | Good |
| Server: segment_match() | 5 | Good |
| Server: is_php_file() | 2 | Good |
| Server: Static file serving | 11 | Good |
| Server: TLS cert loading | 7 | Good |
| Server: Memory size parsing | 8 | Good |
| KV: Redis commands | 67 | Good |
| KV: In-memory store + compression | 23 | Good |
| KV: Eviction policies | 11 | Good |
| KV: Memory tracking | 3 | Good |
| KV: Glob matching | 3 | Good |
| KV: RESP serialize/parse | 19 | Good |
| KV: RESP integration (TCP) | 48 | Good |
| DB: URL/duration parsing | 5 | Good |
| **Server: ACME provisioning** | **0** | **Gap** |
| **Cluster: Config parsing** | **0** | **Gap (struct not yet added)** |
| **Cluster: Gossip protocol** | **0** | **Gap (crate not yet created)** |
| **Cluster: Clustered store routing** | **0** | **Gap (crate not yet created)** |
| **CLI: Argument parsing** | **0** | **Gap (needs openssl-sys)** |
| **PHP: Thread pool (ZTS)** | **0** | **Gap (php_linked only)** |
| **PHP: SAPI callbacks** | **0** | **Gap** |
| **Server: Graceful shutdown** | **0** | **Gap** |
| **Server: Connection handling** | **0** | **Gap** |

---

## Tests To Build

### High Priority — Testable Now

#### 1. TLS Certificate Loading (`ephpm-server/src/tls.rs`) -- DONE

- [x] Load valid PEM certificate (RSA)
- [x] Load valid PEM certificate (EC)
- [x] Reject invalid/malformed certificate
- [x] Reject invalid/malformed key
- [x] Reject mismatched cert/key pair
- [x] Missing cert file returns clear error
- [x] Missing key file returns clear error

#### 2. Memory Size Parsing (`ephpm-server/src/lib.rs`) -- DONE

- [x] Parse "256MB" -> bytes
- [x] Parse "1GB" -> bytes
- [x] Parse "512KB" -> bytes
- [x] Parse raw bytes (no suffix)
- [x] Lowercase suffix ("256mb")
- [x] Whitespace trimming (" 256MB ")
- [x] Invalid input returns error
- [x] Zero returns 0

#### 3. KV Eviction Policies (`ephpm-kv/src/store/mod.rs`) -- DONE

- [x] `EvictionPolicy` from-string parsing (all 4 variants + unknown fallback)
- [x] `AllKeysLru`: evicts to make room for new write
- [x] `AllKeysLru`: evicts least-recently-accessed key (not just oldest)
- [x] `VolatileLru`: only evicts keys with TTL set
- [x] `VolatileLru`: fails when only persistent keys exist
- [x] `AllKeysRandom`: random eviction succeeds
- [x] `NoEviction`: rejects writes when at memory limit
- [x] Eviction frees enough space for large writes (multiple keys)
- [x] Eviction on empty store fails (nothing to evict)
- [x] Unlimited memory (limit=0) accepts any size

#### 4. KV Memory Tracking (`ephpm-kv/src/store/mod.rs`) -- DONE

- [x] `mem_used` increases on insert, decreases on remove
- [x] `mem_used` adjusts on overwrite (larger and smaller values)
- [x] `flush()` resets `mem_used` to 0

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

#### 7. Router Edge Cases (`ephpm-server/src/router.rs`) -- DONE

- [x] `is_compressible()`: text types compressible, binary types not
- [x] `segment_match()`: exact, prefix-star, suffix-star, prefix-star-suffix
- [x] `has_hidden_segment()`: dot-only not hidden, deep nesting
- [x] `is_php_file()`: case insensitive, non-PHP returns false
- [x] `gzip_compress()`: JSON, SVG, disabled for binary
- [x] `etag_matches_value()`: empty If-None-Match, strong ETag
- [x] Blocked paths: empty list blocks nothing, multiple patterns
- [x] IPv6 listen address parsing
- [x] `glob_match()`: directory prefix, no-wildcard exact

#### 8. Static File Edge Cases (`ephpm-server/src/static_files.rs`) -- DONE

- [x] JavaScript file content-type
- [x] PNG image content-type
- [x] Empty file (Content-Length: 0)
- [x] Nested directory paths
- [x] Binary file data integrity

### Medium Priority -- Requires Infrastructure

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

### Lower Priority -- Requires php_linked

#### 12. PHP Thread Pool / ZTS (`ephpm-php/src/lib.rs`)

- [ ] TSRM thread registration on first `spawn_blocking` use
- [ ] Concurrent PHP execution across multiple threads
- [ ] AtomicBool fast-path check for initialization
- [ ] Graceful shutdown (mutex-protected)
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

### Resolved Bugs

#### KV Store DashMap Deadlock -- FIXED

`compression_below_min_size_not_compressed` previously deadlocked because it held a DashMap read guard via `s.data.get()` then called `s.get()` which tries `get_mut()` on the same key. Fixed by dropping the guard before calling `s.get()`.
