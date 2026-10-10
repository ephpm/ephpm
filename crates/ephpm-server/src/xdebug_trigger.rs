//! Router-side mirror of Xdebug's "should this request start a step-debugging
//! session?" decision, for worker mode's debug lane ([`crate::debug_lane`]).
//!
//! The lane gives a request a fresh one-request worker thread so that
//! Xdebug's RINIT sees it. That is only worth a framework boot when Xdebug
//! will actually open a session, and — because the lane is reachable by any
//! client that can reach the server — it should be no easier to reach than
//! Xdebug itself. So the router reproduces Xdebug 3's own check (verified
//! against Xdebug 3.5.3 `src/lib/lib.c` `trigger_enabled()` /
//! `xdebug_lib_find_in_globals()` and `src/debugger/com.c`
//! `xdebug_debug_init_if_requested_at_startup()`), over the same inputs PHP
//! builds `$_GET` / `$_POST` / `$_COOKIE` from:
//!
//! - **Mode**: `XDEBUG_MODE` (when it parses) else `xdebug.mode`; the lane is
//!   only armed when the effective mode includes `debug`.
//! - **`xdebug.start_with_request`**: `no` (or empty) never starts a session
//!   at request startup, so the lane is not armed. `yes` starts one for every
//!   request, so the lane routes any request that *names* a trigger (its
//!   value is irrelevant to Xdebug). `trigger`/`default` (default = trigger
//!   for step debugging) apply the trigger rules below.
//! - **`XDEBUG_IGNORE`** present (value other than `no` / `0`) → no session.
//! - **`XDEBUG_TRIGGER`**, else the legacy **`XDEBUG_SESSION`**, looked up in
//!   `$_GET`, then `$_POST`, then `$_COOKIE` — the *first* place a name is
//!   found decides, exactly like Xdebug. With **`xdebug.trigger_value`** set
//!   (a shared secret, optionally a comma-separated list) the whitespace-
//!   trimmed value must equal one of the trimmed entries; without it any
//!   value starts a session.
//! - **`XDEBUG_SESSION_START`** in `$_GET` or `$_POST` starts a session —
//!   unless `xdebug.trigger_value` is set, in which case Xdebug refuses this
//!   legacy method outright.
//!
//! Names and values are decoded the way PHP decodes them (query and form
//! body: `application/x-www-form-urlencoded` with `+` as space; cookies:
//! names raw, values raw-url-decoded; PHP's variable-name mangling of
//! leading spaces, `.`/space → `_` and `[...]` arrays; duplicate GET/POST
//! keys: last wins; duplicate cookies: first wins), so an encoding trick
//! cannot route a request Xdebug would refuse, or the reverse.
//!
//! Deliberately **not** mirrored: triggers in ePHPm's own process
//! environment (they would make every worker thread debug itself at boot —
//! don't set them), a `multipart/form-data` or streaming body (the router
//! does not parse those; send the trigger in the query string or a cookie),
//! `arg_separator.input` other than `&`, `max_input_vars` truncation, and
//! per-request/per-vhost changes to the three ini settings (the server-wide
//! startup values are used). Any disagreement in the remaining corner cases
//! costs at most one wasted boot or one undebugged request — never a wrong
//! response.

use ephpm_php::worker_bridge::is_form_urlencoded;
use ephpm_php::xdebug::XdebugIni;

/// The trigger names this module looks up. Everything else is ignored.
const XDEBUG_TRIGGER: &[u8] = b"XDEBUG_TRIGGER";
const XDEBUG_SESSION: &[u8] = b"XDEBUG_SESSION";
const XDEBUG_SESSION_START: &[u8] = b"XDEBUG_SESSION_START";
const XDEBUG_IGNORE: &[u8] = b"XDEBUG_IGNORE";
const NAMES: [&[u8]; 4] = [XDEBUG_TRIGGER, XDEBUG_SESSION, XDEBUG_SESSION_START, XDEBUG_IGNORE];

/// `xdebug.start_with_request`, as Xdebug parses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartWithRequest {
    /// `default` — for step debugging, the same as `trigger`.
    Default,
    /// `yes` / `1` — every request starts a session.
    Yes,
    /// `no` / empty — no request starts a session at startup.
    No,
    /// `trigger` — only requests carrying a trigger.
    Trigger,
}

impl StartWithRequest {
    /// Parse the ini value. Xdebug's ini handler rejects anything else and
    /// keeps the default, so unknown values map to [`Self::Default`].
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "yes" | "1" => Self::Yes,
            "no" | "" => Self::No,
            "trigger" => Self::Trigger,
            _ => Self::Default,
        }
    }
}

/// Why the debug lane stays disarmed even though Xdebug is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inactive {
    /// The effective `xdebug.mode` does not include `debug`.
    NoStepDebug {
        /// The mode string Xdebug is using.
        effective_mode: String,
    },
    /// `xdebug.start_with_request = no`: sessions never start at request
    /// startup, so a fresh worker would only cost a boot.
    NeverStartWithRequest,
}

impl std::fmt::Display for Inactive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStepDebug { effective_mode } => write!(
                f,
                "xdebug.mode is {effective_mode:?}, which does not include \"debug\" (step \
                 debugging off)"
            ),
            Self::NeverStartWithRequest => f.write_str(
                "xdebug.start_with_request = no (sessions never start at request startup)",
            ),
        }
    }
}

/// Xdebug's step-debug trigger policy, frozen from the startup settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerPolicy {
    start_with_request: StartWithRequest,
    /// `None` = no shared secret (`xdebug.trigger_value` empty). Otherwise the
    /// trimmed accepted values.
    secrets: Option<Vec<Vec<u8>>>,
}

/// The parts of a request PHP builds `$_GET` / `$_POST` / `$_COOKIE` from.
#[derive(Debug, Clone, Copy)]
pub struct TriggerInput<'a> {
    /// HTTP method (`$_POST` is only built for `POST`).
    pub method: &'a str,
    /// Query string, without the leading `?`.
    pub query_string: &'a str,
    /// The `Cookie` header as PHP receives it.
    pub cookie_data: &'a str,
    /// `Content-Type`, if any.
    pub content_type: Option<&'a str>,
    /// The request body when it is fully buffered; `None` for a streaming
    /// body (never read here).
    pub buffered_body: Option<&'a [u8]>,
}

impl TriggerPolicy {
    /// Build the policy from the startup snapshot, or explain why step
    /// debugging cannot start at request startup at all.
    ///
    /// # Errors
    ///
    /// [`Inactive`] when the effective mode lacks `debug` or
    /// `start_with_request` is `no` — the lane must not be armed.
    pub fn from_ini(ini: &XdebugIni) -> Result<Self, Inactive> {
        let effective_mode = effective_mode(ini);
        if !mode_includes_debug(&effective_mode) {
            return Err(Inactive::NoStepDebug { effective_mode });
        }
        let start_with_request = StartWithRequest::parse(&ini.start_with_request);
        if start_with_request == StartWithRequest::No {
            return Err(Inactive::NeverStartWithRequest);
        }
        Ok(Self { start_with_request, secrets: parse_secrets(&ini.trigger_value) })
    }

    /// Whether a shared secret (`xdebug.trigger_value`) gates sessions.
    #[must_use]
    pub fn has_shared_secret(&self) -> bool {
        self.secrets.is_some()
    }

    /// The parsed `start_with_request` setting.
    #[must_use]
    pub fn start_with_request(&self) -> StartWithRequest {
        self.start_with_request
    }

    /// Whether Xdebug would open a step-debugging session for this request
    /// at RINIT — i.e. whether it should run on the debug lane.
    #[must_use]
    pub fn routes(&self, input: &TriggerInput<'_>) -> bool {
        // Cheap pre-filter for the overwhelmingly common case: every name we
        // care about contains "XDEBUG" in some spelling PHP would decode to
        // it, so a request mentioning none of the raw bytes cannot match.
        // (`%58DEBUG` etc. defeat it, so only skip when no `%` is present
        // either.)
        if !might_mention(input.query_string.as_bytes())
            && !might_mention(input.cookie_data.as_bytes())
            && !input.buffered_body.is_some_and(might_mention)
        {
            return false;
        }

        let lookup = Lookup::build(input);

        if let Some(value) = lookup.first_found(XDEBUG_IGNORE)
            && !matches!(value, Value::Str(v) if v == b"no" || v == b"0")
        {
            return false;
        }

        match self.start_with_request {
            // Lane disarmed for `No`; defensive.
            StartWithRequest::No => false,
            // Every request gets a session from Xdebug; route the ones that
            // ask for a fresh, per-request one.
            StartWithRequest::Yes => {
                lookup.first_found(XDEBUG_TRIGGER).is_some()
                    || lookup.first_found(XDEBUG_SESSION).is_some()
                    || lookup.session_start
            }
            StartWithRequest::Default | StartWithRequest::Trigger => {
                self.trigger_enabled(&lookup) || (self.secrets.is_none() && lookup.session_start)
            }
        }
    }

    /// Xdebug's `trigger_enabled()` for step debugging.
    fn trigger_enabled(&self, lookup: &Lookup) -> bool {
        let Some(value) =
            lookup.first_found(XDEBUG_TRIGGER).or_else(|| lookup.first_found(XDEBUG_SESSION))
        else {
            return false;
        };
        match &self.secrets {
            None => true,
            Some(secrets) => match value {
                Value::Str(v) => {
                    let v = c_trim(v);
                    secrets.iter().any(|s| s.as_slice() == v)
                }
                // An array-valued trigger has no string value to compare.
                Value::Array => false,
            },
        }
    }
}

/// `XDEBUG_MODE` when non-empty and valid, else `xdebug.mode` when valid,
/// else Xdebug's fallback `develop`.
fn effective_mode(ini: &XdebugIni) -> String {
    if let Some(env) = ini.mode_env.as_deref().filter(|m| !m.is_empty())
        && parse_mode(env).is_some()
    {
        return env.to_string();
    }
    if parse_mode(&ini.mode).is_some() { ini.mode.clone() } else { "develop".to_string() }
}

fn mode_includes_debug(mode: &str) -> bool {
    parse_mode(mode).is_some_and(|items| items.contains(&"debug"))
}

/// Xdebug's mode-list parser: items separated by `,` (spaces skipped after
/// a comma only), each item selecting the first known mode it is a prefix
/// of (`strncmp(item, name, strlen(item)) == 0` — so `""` is `off` and `d`
/// is `develop`). `None` if any item is unknown.
fn parse_mode(mode: &str) -> Option<Vec<&'static str>> {
    const MODES: [&str; 7] = ["off", "develop", "coverage", "debug", "gcstats", "profile", "trace"];
    let mut items = Vec::new();
    for (i, raw) in mode.split(',').enumerate() {
        let item = if i == 0 { raw } else { raw.trim_start_matches(' ') };
        items.push(*MODES.iter().find(|name| name.starts_with(item))?);
    }
    Some(items)
}

/// `xdebug.trigger_value` → accepted values. Empty → no secret. With a comma
/// the value is a list (each entry trimmed); otherwise the whole value,
/// trimmed. Matches Xdebug's `does_shared_secret_match()`.
fn parse_secrets(raw: &str) -> Option<Vec<Vec<u8>>> {
    if raw.is_empty() {
        return None;
    }
    let raw = raw.as_bytes();
    let list = if raw.contains(&b',') {
        raw.split(|&b| b == b',').map(|s| c_trim(s).to_vec()).collect()
    } else {
        vec![c_trim(raw).to_vec()]
    };
    Some(list)
}

/// C `isspace()` (the "C" locale): space, `\t`, `\n`, `\v`, `\f`, `\r`.
fn c_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Xdebug's `xdebug_trim()`.
fn c_trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| !c_isspace(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|&b| !c_isspace(b)).map_or(start, |i| i + 1);
    &s[start..end.max(start)]
}

/// Whether raw input could decode to one of our names: it contains
/// `XDEBUG` verbatim, or a `%` escape that could spell it.
fn might_mention(raw: &[u8]) -> bool {
    raw.windows(6).any(|w| w == b"XDEBUG") || raw.contains(&b'%')
}

/// A superglobal entry: a string, or an array (`NAME[...]=`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Str(Vec<u8>),
    Array,
}

/// The four trigger names, looked up per source the way Xdebug looks them
/// up in the superglobals.
#[derive(Debug, Default)]
struct Lookup {
    get: [Option<Value>; 4],
    post: [Option<Value>; 4],
    cookie: [Option<Value>; 4],
    /// `XDEBUG_SESSION_START` present in `$_GET` or `$_POST`.
    session_start: bool,
}

impl Lookup {
    fn build(input: &TriggerInput<'_>) -> Self {
        let mut lookup = Self::default();
        parse_form(input.query_string.as_bytes(), &mut lookup.get);
        // `$_POST` is only built for a POST whose type PHP parses as a form;
        // multipart bodies are not inspected (see the module docs).
        if input.method == "POST"
            && input.content_type.is_some_and(is_form_urlencoded)
            && let Some(body) = input.buffered_body
        {
            parse_form(body, &mut lookup.post);
        }
        parse_cookies(input.cookie_data.as_bytes(), &mut lookup.cookie);
        let idx = name_index(XDEBUG_SESSION_START).expect("known name");
        lookup.session_start = lookup.get[idx].is_some() || lookup.post[idx].is_some();
        lookup
    }

    /// `$_GET`, then `$_POST`, then `$_COOKIE`: the first source holding the
    /// name decides (environment variables are not consulted).
    fn first_found(&self, name: &[u8]) -> Option<&Value> {
        let idx = name_index(name)?;
        self.get[idx].as_ref().or(self.post[idx].as_ref()).or(self.cookie[idx].as_ref())
    }
}

fn name_index(name: &[u8]) -> Option<usize> {
    NAMES.iter().position(|n| *n == name)
}

/// Parse `a=1&b=2` (query string / urlencoded body) into `slots`; later
/// duplicates overwrite earlier ones, as `$_GET`/`$_POST` do.
fn parse_form(raw: &[u8], slots: &mut [Option<Value>; 4]) {
    for pair in raw.split(|&b| b == b'&').filter(|p| !p.is_empty()) {
        let (name, value) = match pair.iter().position(|&b| b == b'=') {
            Some(eq) => (&pair[..eq], Some(&pair[eq + 1..])),
            None => (pair, None),
        };
        let name = url_decode(name, true);
        if let Some((idx, value)) = register(&name, value.map(|v| url_decode(v, true))) {
            slots[idx] = Some(value);
        }
    }
}

/// Parse a `Cookie` header into `slots`; the first occurrence of a name
/// wins, as PHP keeps the first cookie of a name. Names are not url-decoded
/// (PHP 8), values are raw-url-decoded (`+` stays `+`).
fn parse_cookies(raw: &[u8], slots: &mut [Option<Value>; 4]) {
    for pair in raw.split(|&b| b == b';') {
        let start = pair.iter().position(|&b| !c_isspace(b)).unwrap_or(pair.len());
        let pair = &pair[start..];
        if pair.is_empty() || pair[0] == b'=' {
            continue;
        }
        let (name, value) = match pair.iter().position(|&b| b == b'=') {
            Some(eq) => (&pair[..eq], Some(&pair[eq + 1..])),
            None => (pair, None),
        };
        if let Some((idx, value)) = register(name, value.map(|v| url_decode(v, false)))
            && slots[idx].is_none()
        {
            slots[idx] = Some(value);
        }
    }
}

/// PHP's `php_register_variable_ex()` name handling, reduced to "which of
/// our four names (if any) does this register, and as what?".
fn register(raw_name: &[u8], value: Option<Vec<u8>>) -> Option<(usize, Value)> {
    // C string: a decoded NUL ends the name.
    let name = raw_name.split(|&b| b == 0).next().unwrap_or_default();
    // Leading spaces are dropped.
    let name = &name[name.iter().position(|&b| b != b' ').unwrap_or(name.len())..];
    let mut mangled = Vec::with_capacity(name.len());
    let mut is_array = false;
    for (i, &b) in name.iter().enumerate() {
        match b {
            b' ' | b'.' => mangled.push(b'_'),
            b'[' => {
                if name[i + 1..].contains(&b']') {
                    // `NAME[...]` registers an array under NAME.
                    is_array = true;
                } else {
                    // Unterminated: the `[` becomes `_` and the rest of the
                    // name is kept, with ` `, `.` and `[` also mangled.
                    mangled.push(b'_');
                    mangled.extend(
                        name[i + 1..]
                            .iter()
                            .map(|&c| if matches!(c, b' ' | b'.' | b'[') { b'_' } else { c }),
                    );
                }
                break;
            }
            _ => mangled.push(b),
        }
    }
    let idx = name_index(&mangled)?;
    let value = if is_array {
        Value::Array
    } else {
        // Xdebug reads the value as a C string: a decoded NUL ends it.
        let v = value.unwrap_or_default();
        Value::Str(v.split(|&b| b == 0).next().unwrap_or_default().to_vec())
    };
    Some((idx, value))
}

/// `php_url_decode` (`plus_is_space`) / `php_raw_url_decode`: `%XX` with two
/// hex digits decodes, anything else is literal.
fn url_decode(raw: &[u8], plus_is_space: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' if plus_is_space => out.push(b' '),
            b'%' if i + 2 < raw.len()
                && raw[i + 1].is_ascii_hexdigit()
                && raw[i + 2].is_ascii_hexdigit() =>
            {
                out.push((hex(raw[i + 1]) << 4) | hex(raw[i + 2]));
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    out
}

fn hex(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ini(mode: &str, swr: &str, secret: &str) -> XdebugIni {
        XdebugIni {
            mode: mode.into(),
            mode_env: None,
            start_with_request: swr.into(),
            trigger_value: secret.into(),
            discover_client_host: String::new(),
        }
    }

    fn policy(secret: &str) -> TriggerPolicy {
        TriggerPolicy::from_ini(&ini("debug", "trigger", secret)).unwrap()
    }

    fn get(query: &str) -> TriggerInput<'_> {
        TriggerInput {
            method: "GET",
            query_string: query,
            cookie_data: "",
            content_type: None,
            buffered_body: None,
        }
    }

    fn cookie(cookie: &str) -> TriggerInput<'_> {
        TriggerInput { cookie_data: cookie, ..get("") }
    }

    fn post<'a>(content_type: &'a str, body: &'a [u8]) -> TriggerInput<'a> {
        TriggerInput {
            method: "POST",
            content_type: Some(content_type),
            buffered_body: Some(body),
            ..get("")
        }
    }

    #[test]
    fn arming_follows_mode_and_start_with_request() {
        assert!(TriggerPolicy::from_ini(&ini("debug", "trigger", "")).is_ok());
        assert!(TriggerPolicy::from_ini(&ini("develop,debug", "default", "")).is_ok());
        assert!(TriggerPolicy::from_ini(&ini("debug, profile", "yes", "")).is_ok());
        // Xdebug's default mode is `develop`: no step debugging, no lane.
        assert!(matches!(
            TriggerPolicy::from_ini(&ini("develop", "trigger", "")),
            Err(Inactive::NoStepDebug { .. })
        ));
        assert!(matches!(
            TriggerPolicy::from_ini(&ini("debug", "no", "")),
            Err(Inactive::NeverStartWithRequest)
        ));
        // An invalid mode list makes Xdebug fall back to `develop`.
        assert!(TriggerPolicy::from_ini(&ini("debug,bogus", "trigger", "")).is_err());
        // Xdebug's prefix matching: `deb` selects debug, `d` selects develop.
        assert!(TriggerPolicy::from_ini(&ini("deb", "trigger", "")).is_ok());
        assert!(TriggerPolicy::from_ini(&ini("d", "trigger", "")).is_err());
    }

    #[test]
    fn xdebug_mode_env_wins_when_valid() {
        let mut settings = ini("develop", "trigger", "");
        settings.mode_env = Some("debug".into());
        assert!(TriggerPolicy::from_ini(&settings).is_ok());
        // Invalid env value: Xdebug falls back to the ini setting.
        settings.mode_env = Some("nonsense".into());
        assert!(TriggerPolicy::from_ini(&settings).is_err());
        // Env can also turn step debugging off.
        let mut settings = ini("debug", "trigger", "");
        settings.mode_env = Some("off".into());
        assert!(TriggerPolicy::from_ini(&settings).is_err());
        // Empty env is ignored.
        settings.mode_env = Some(String::new());
        assert!(TriggerPolicy::from_ini(&settings).is_ok());
    }

    #[test]
    fn query_triggers_without_a_secret() {
        let p = policy("");
        assert!(p.routes(&get("XDEBUG_TRIGGER=1")));
        assert!(p.routes(&get("a=1&XDEBUG_TRIGGER=phpstorm&b=2")));
        assert!(p.routes(&get("XDEBUG_TRIGGER")), "bare key, empty value");
        assert!(p.routes(&get("XDEBUG_SESSION=PHPSTORM")), "legacy name");
        assert!(p.routes(&get("XDEBUG_SESSION_START=1")), "legacy start");
        assert!(!p.routes(&get("")));
        assert!(!p.routes(&get("page=2")));
        assert!(!p.routes(&get("xdebug_trigger=1")), "names are case-sensitive");
        assert!(!p.routes(&get("XDEBUG_SESSION_STOP=1")), "a stop is not a start");
        assert!(!p.routes(&get("NOT_XDEBUG_TRIGGER=1")));
        assert!(!p.routes(&get("q=XDEBUG_TRIGGER")), "value, not name");
    }

    #[test]
    fn php_name_decoding_and_mangling() {
        let p = policy("");
        assert!(p.routes(&get("%58DEBUG_TRIGGER=1")), "percent-encoded name");
        assert!(p.routes(&get("XDEBUG.TRIGGER=1")), "`.` becomes `_`");
        assert!(p.routes(&get("XDEBUG+TRIGGER=1")), "`+` is a space, which becomes `_`");
        assert!(p.routes(&get("+XDEBUG_TRIGGER=1")), "leading spaces are dropped");
        assert!(p.routes(&get("XDEBUG[TRIGGER=1")), "unterminated `[` becomes `_`");
        assert!(p.routes(&get("XDEBUG_TRIGGER%00junk=1")), "a NUL ends the name");
        assert!(!p.routes(&get("XDEBUG_TRIGGERS=1")));
    }

    #[test]
    fn cookie_triggers() {
        let p = policy("");
        assert!(p.routes(&cookie("XDEBUG_SESSION=PHPSTORM")));
        assert!(p.routes(&cookie("a=b; XDEBUG_SESSION=1; c=d")));
        assert!(p.routes(&cookie("XDEBUG_TRIGGER=1")));
        assert!(!p.routes(&cookie("XDEBUG_SESSION_START=1")), "legacy start is GET/POST only");
        assert!(!p.routes(&cookie("PHPSESSID=XDEBUG_SESSION")), "value, not name");
        assert!(!p.routes(&cookie("%58DEBUG_SESSION=1")), "cookie names are not url-decoded");
        assert!(!p.routes(&cookie("=XDEBUG_SESSION")));
    }

    #[test]
    fn post_body_triggers() {
        let p = policy("");
        let form = "application/x-www-form-urlencoded";
        assert!(p.routes(&post(form, b"name=x&XDEBUG_TRIGGER=1")));
        assert!(p.routes(&post(
            "Application/X-WWW-Form-Urlencoded; charset=utf-8",
            b"XDEBUG_TRIGGER=1"
        )));
        assert!(p.routes(&post(form, b"XDEBUG_SESSION_START=vscode")));
        // Only forms PHP itself parses into $_POST at startup.
        assert!(!p.routes(&post("application/json", b"XDEBUG_TRIGGER=1")));
        assert!(!p.routes(&post("multipart/form-data; boundary=x", b"XDEBUG_TRIGGER=1")));
        let mut put = post(form, b"XDEBUG_TRIGGER=1");
        put.method = "PUT";
        assert!(!p.routes(&put), "PHP builds $_POST for POST only");
        let mut streaming = post(form, b"");
        streaming.buffered_body = None;
        assert!(!p.routes(&streaming), "a streaming body is never inspected");
    }

    #[test]
    fn shared_secret_is_enforced_like_xdebug() {
        let p = policy("s3cret");
        assert!(p.has_shared_secret());
        assert!(p.routes(&get("XDEBUG_TRIGGER=s3cret")));
        assert!(p.routes(&get("XDEBUG_TRIGGER=%20s3cret%09")), "value is trimmed");
        assert!(p.routes(&cookie("XDEBUG_SESSION=s3cret")));
        assert!(p.routes(&post("application/x-www-form-urlencoded", b"XDEBUG_TRIGGER=s3cret")));
        assert!(!p.routes(&get("XDEBUG_TRIGGER=1")));
        assert!(!p.routes(&get("XDEBUG_TRIGGER")));
        assert!(!p.routes(&get("XDEBUG_TRIGGER[]=s3cret")), "an array has no string value");
        // The legacy start is refused outright when a secret is set.
        assert!(!p.routes(&get("XDEBUG_SESSION_START=s3cret")));

        let list = policy(" alice , bob ");
        assert!(list.routes(&get("XDEBUG_TRIGGER=alice")));
        assert!(list.routes(&get("XDEBUG_TRIGGER=bob")));
        assert!(!list.routes(&get("XDEBUG_TRIGGER=carol")));
    }

    #[test]
    fn first_source_found_decides_like_xdebug() {
        let p = policy("good");
        // GET is consulted before COOKIE: a wrong GET value wins.
        let input =
            TriggerInput { cookie_data: "XDEBUG_TRIGGER=good", ..get("XDEBUG_TRIGGER=bad") };
        assert!(!p.routes(&input));
        let input =
            TriggerInput { cookie_data: "XDEBUG_TRIGGER=bad", ..get("XDEBUG_TRIGGER=good") };
        assert!(p.routes(&input));
        // XDEBUG_TRIGGER anywhere shadows the XDEBUG_SESSION fallback.
        let input =
            TriggerInput { cookie_data: "XDEBUG_SESSION=good", ..get("XDEBUG_TRIGGER=bad") };
        assert!(!p.routes(&input));
        // Duplicate GET keys: the last one wins.
        assert!(p.routes(&get("XDEBUG_TRIGGER=bad&XDEBUG_TRIGGER=good")));
        assert!(!p.routes(&get("XDEBUG_TRIGGER=good&XDEBUG_TRIGGER=bad")));
        // Duplicate cookies: the first one wins.
        assert!(p.routes(&cookie("XDEBUG_TRIGGER=good; XDEBUG_TRIGGER=bad")));
        assert!(!p.routes(&cookie("XDEBUG_TRIGGER=bad; XDEBUG_TRIGGER=good")));
        // Cookie values are raw-url-decoded: `+` stays `+`.
        let plus = policy("a+b");
        assert!(plus.routes(&cookie("XDEBUG_TRIGGER=a+b")));
        assert!(!plus.routes(&get("XDEBUG_TRIGGER=a+b")), "in a query `+` is a space");
        assert!(plus.routes(&get("XDEBUG_TRIGGER=a%2Bb")));
    }

    #[test]
    fn xdebug_ignore_vetoes() {
        let p = policy("");
        assert!(!p.routes(&get("XDEBUG_TRIGGER=1&XDEBUG_IGNORE=1")));
        let input = TriggerInput { cookie_data: "XDEBUG_IGNORE=yes", ..get("XDEBUG_TRIGGER=1") };
        assert!(!p.routes(&input));
        assert!(p.routes(&get("XDEBUG_TRIGGER=1&XDEBUG_IGNORE=no")));
        assert!(p.routes(&get("XDEBUG_TRIGGER=1&XDEBUG_IGNORE=0")));
    }

    #[test]
    fn start_with_request_yes_routes_any_named_trigger() {
        let p = TriggerPolicy::from_ini(&ini("debug", "yes", "s3cret")).unwrap();
        // Xdebug ignores the value (and the secret) under `yes`.
        assert!(p.routes(&get("XDEBUG_TRIGGER=anything")));
        assert!(p.routes(&get("XDEBUG_SESSION_START=1")));
        assert!(!p.routes(&get("page=1")));
    }

    #[test]
    fn decoders() {
        assert_eq!(url_decode(b"a%20b+c%2", true), b"a b c%2");
        assert_eq!(url_decode(b"a+b%zz", false), b"a+b%zz");
        assert_eq!(c_trim(b" \t\x0bx y\r\n"), b"x y");
        assert_eq!(c_trim(b"   "), b"");
        assert_eq!(StartWithRequest::parse("1"), StartWithRequest::Yes);
        assert_eq!(StartWithRequest::parse(""), StartWithRequest::No);
        assert_eq!(StartWithRequest::parse("bogus"), StartWithRequest::Default);
    }
}
