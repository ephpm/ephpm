//! `ephpm ext` — extension management commands.

use std::process::Command;
use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtKind {
    BuiltIn,
    ZendDev,
}

#[derive(Debug, Clone)]
pub struct ExtInfo {
    pub name: String,
    pub version: String,
    pub kind: ExtKind,
}

pub fn get_extensions() -> Result<(String, Vec<ExtInfo>)> {
    let binary = std::env::current_exe().context("failed to get current binary path")?;

    // Run `<self> php -m`
    let modules_out = Command::new(&binary)
        .args(["php", "-m"])
        .output()
        .context("failed to run `ephpm php -m`")?;
    let modules_text = String::from_utf8_lossy(&modules_out.stdout).into_owned();

    // Run `<self> php -r` to get versions
    let ver_script = r#"foreach(get_loaded_extensions() as $e) { echo $e.'::'.( phpversion($e) ?: 'n/a')."\n"; }"#;
    let ver_out = Command::new(&binary)
        .args(["php", "-r", ver_script])
        .output()
        .context("failed to query PHP extension versions")?;

    // Build name -> version map
    let versions: std::collections::HashMap<String, String> =
        String::from_utf8_lossy(&ver_out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, "::");
                let name = parts.next()?.trim().to_lowercase();
                let ver = parts.next().unwrap_or("n/a").trim().to_string();
                // mysqlnd reports "mysqlnd 8.5.2" — grab just the version number
                let ver = ver.split_whitespace().last().unwrap_or("n/a").to_string();
                Some((name, ver))
            })
            .collect();

    // Parse `php -m` output
    let mut extensions = Vec::new();
    let mut in_zend = false;
    let mut seen = std::collections::HashSet::new();

    for line in modules_text.lines() {
        let line = line.trim();
        match line {
            "[PHP Modules]"  => { in_zend = false; continue; }
            "[Zend Modules]" => { in_zend = true;  continue; }
            "" => continue,
            _ if line.starts_with('[') => { in_zend = false; continue; }
            name => {
                let key = name.to_lowercase();
                if !seen.insert(key.clone()) {
                    continue; // skip duplicates (OPcache appears twice)
                }
                let version = versions.get(&key).cloned().unwrap_or_else(|| "n/a".into());
                // OPcache is a Zend ext but treat as built-in — it's always present
                let kind = if in_zend && key != "zend opcache" {
                    ExtKind::ZendDev
                } else {
                    ExtKind::BuiltIn
                };
                extensions.push(ExtInfo { name: name.to_string(), version, kind });
            }
        }
    }

    // Get PHP version from Core
    let php_version = extensions
        .iter()
        .find(|e| e.name == "Core")
        .map(|e| e.version.clone())
        .unwrap_or_else(|| "unknown".into());

    Ok((php_version, extensions))
}

pub fn cmd_list(json: bool) -> Result<()> {
    let (php_version, mut extensions) = get_extensions()?;

    // Sort: built-in alpha first, zend-dev alpha after
    extensions.sort_by(|a, b| {
        let kind_ord = |k: &ExtKind| match k {
            ExtKind::BuiltIn => 0,
            ExtKind::ZendDev => 1,
        };
        kind_ord(&a.kind)
            .cmp(&kind_ord(&b.kind))
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    if json {
        print_json(&php_version, &extensions);
    } else {
        print_table(&php_version, &extensions);
    }
    Ok(())
}

fn print_table(php_version: &str, extensions: &[ExtInfo]) {
    let builtin_count = extensions.iter().filter(|e| e.kind == ExtKind::BuiltIn).count();
    let zend_count    = extensions.iter().filter(|e| e.kind == ExtKind::ZendDev).count();

    let name_w = extensions.iter().map(|e| e.name.len()).max().unwrap_or(9).max(9);
    let ver_w  = extensions.iter().map(|e| e.version.len()).max().unwrap_or(7).max(7);

    println!("PHP: {php_version}");
    println!();
    println!("{:<name_w$}  {:<ver_w$}  STATUS", "EXTENSION", "VERSION", name_w = name_w, ver_w = ver_w);
    println!("{}", "\u{2500}".repeat(name_w + ver_w + 10));

    for ext in extensions {
        let status = match ext.kind {
            ExtKind::BuiltIn => "built-in",
            ExtKind::ZendDev => "zend/dev",
        };
        println!("{:<name_w$}  {:<ver_w$}  {status}", ext.name, ext.version, name_w = name_w, ver_w = ver_w);
    }

    println!("{}", "\u{2500}".repeat(name_w + ver_w + 10));
    let mut parts = Vec::new();
    if builtin_count > 0 { parts.push(format!("{builtin_count} built-in")); }
    if zend_count    > 0 { parts.push(format!("{zend_count} zend/dev")); }
    println!("{} = {} extensions total", parts.join(" + "), extensions.len());
}

fn print_json(php_version: &str, extensions: &[ExtInfo]) {
    println!("{{");
    println!("  \"php_version\": {php_version:?},");
    println!("  \"count\": {},", extensions.len());
    println!("  \"extensions\": [");
    for (i, ext) in extensions.iter().enumerate() {
        let kind_str = match ext.kind {
            ExtKind::BuiltIn => "built-in",
            ExtKind::ZendDev => "zend-dev",
        };
        let comma = if i + 1 < extensions.len() { "," } else { "" };
        println!("    {{\"name\": {:?}, \"version\": {:?}, \"kind\": {:?}}}{comma}", ext.name, ext.version, kind_str);
    }
    println!("  ]");
    println!("}}");
}

pub fn cmd_info(name: &str) -> Result<()> {
    let (_, extensions) = get_extensions()?;
    let needle = name.to_lowercase().trim_start_matches("ext-").to_string();

    match extensions.iter().find(|e| e.name.to_lowercase() == needle) {
        Some(ext) => {
            let kind_label = match ext.kind {
                ExtKind::BuiltIn => "built-in (part of this build)",
                ExtKind::ZendDev => "zend extension (dev builds only)",
            };
            println!("Name:     {}", ext.name);
            println!("Version:  {}", ext.version);
            println!("Type:     {kind_label}");
        }
        None => {
            eprintln!("ext-{name} is NOT in this binary.");
            eprintln!();
            eprintln!("  Add it:  ephpm ext build --add {name}");
            std::process::exit(1);
        }
    }
    Ok(())
}

// ── Static extension registry (sourced from static-php-cli) ──────────────────

struct SpcExt {
    name: &'static str,
    linux: bool,
    macos: bool,
    windows: bool,
    url: &'static str,
}

const SPC_REGISTRY: &[SpcExt] = &[
    SpcExt { name: "amqp",         linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/amqp" },
    SpcExt { name: "apcu",         linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/APCu" },
    SpcExt { name: "ast",          linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/ast" },
    SpcExt { name: "bcmath",       linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "brotli",       linux: true,  macos: true,  windows: true,  url: "https://github.com/kjdev/php-ext-brotli" },
    SpcExt { name: "bz2",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "calendar",     linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "ctype",        linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "curl",         linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "dba",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "decimal",      linux: true,  macos: true,  windows: true,  url: "https://github.com/php-decimal/ext-decimal" },
    SpcExt { name: "dom",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "ds",           linux: true,  macos: true,  windows: true,  url: "https://github.com/php-ds/ext-ds" },
    SpcExt { name: "ev",           linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/ev" },
    SpcExt { name: "event",        linux: true,  macos: true,  windows: false, url: "" },
    SpcExt { name: "excimer",      linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/excimer" },
    SpcExt { name: "exif",         linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "ffi",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "fileinfo",     linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "filter",       linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "ftp",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "gd",           linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "gearman",      linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/gearman" },
    SpcExt { name: "gettext",      linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "gmp",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "grpc",         linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/grpc" },
    SpcExt { name: "iconv",        linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "igbinary",     linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/igbinary" },
    SpcExt { name: "imagick",      linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/imagick" },
    SpcExt { name: "imap",         linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/imap" },
    SpcExt { name: "inotify",      linux: true,  macos: false, windows: false, url: "https://pecl.php.net/package/inotify" },
    SpcExt { name: "intl",         linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "ldap",         linux: true,  macos: true,  windows: false, url: "" },
    SpcExt { name: "libxml",       linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "lz4",          linux: true,  macos: true,  windows: true,  url: "https://github.com/kjdev/php-ext-lz4" },
    SpcExt { name: "maxminddb",    linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/maxminddb" },
    SpcExt { name: "mbstring",     linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "memcache",     linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/memcache" },
    SpcExt { name: "memcached",    linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/memcached" },
    SpcExt { name: "mongodb",      linux: true,  macos: true,  windows: true,  url: "https://github.com/mongodb/mongo-php-driver" },
    SpcExt { name: "msgpack",      linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/msgpack" },
    SpcExt { name: "mysqli",       linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "mysqlnd",      linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "odbc",         linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "opcache",      linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "openssl",      linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "opentelemetry",linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/opentelemetry" },
    SpcExt { name: "parallel",     linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/parallel" },
    SpcExt { name: "pcntl",        linux: true,  macos: true,  windows: false, url: "" },
    SpcExt { name: "pcov",         linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/pcov" },
    SpcExt { name: "pdo",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "pdo_mysql",    linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "pdo_odbc",     linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "pdo_pgsql",    linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "pdo_sqlite",   linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "pdo_sqlsrv",   linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/pdo_sqlsrv" },
    SpcExt { name: "pgsql",        linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "phar",         linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "posix",        linux: true,  macos: true,  windows: false, url: "" },
    SpcExt { name: "protobuf",     linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/protobuf" },
    SpcExt { name: "rar",          linux: true,  macos: true,  windows: true,  url: "https://github.com/static-php/php-rar" },
    SpcExt { name: "rdkafka",      linux: true,  macos: true,  windows: false, url: "https://github.com/php-rdkafka/php-rdkafka" },
    SpcExt { name: "readline",     linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "redis",        linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/redis" },
    SpcExt { name: "session",      linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "shmop",        linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "simdjson",     linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/simdjson" },
    SpcExt { name: "simplexml",    linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "snappy",       linux: true,  macos: true,  windows: true,  url: "https://github.com/kjdev/php-ext-snappy" },
    SpcExt { name: "snmp",         linux: true,  macos: true,  windows: false, url: "" },
    SpcExt { name: "soap",         linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "sockets",      linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "sodium",       linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "spx",          linux: true,  macos: true,  windows: false, url: "https://github.com/noisebynorthwest/php-spx" },
    SpcExt { name: "sqlite3",      linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "sqlsrv",       linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/sqlsrv" },
    SpcExt { name: "ssh2",         linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/ssh2" },
    SpcExt { name: "swoole",       linux: true,  macos: true,  windows: false, url: "https://github.com/swoole/swoole-src" },
    SpcExt { name: "sysvmsg",      linux: true,  macos: true,  windows: false, url: "" },
    SpcExt { name: "sysvsem",      linux: true,  macos: true,  windows: false, url: "" },
    SpcExt { name: "sysvshm",      linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "tidy",         linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "tokenizer",    linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "uuid",         linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/uuid" },
    SpcExt { name: "uv",           linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/uv" },
    SpcExt { name: "xdebug",       linux: true,  macos: true,  windows: false, url: "https://github.com/xdebug/xdebug" },
    SpcExt { name: "xhprof",       linux: true,  macos: true,  windows: false, url: "https://pecl.php.net/package/xhprof" },
    SpcExt { name: "xlswriter",    linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/xlswriter" },
    SpcExt { name: "xml",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "xmlreader",    linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "xmlwriter",    linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "xsl",          linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "yaml",         linux: true,  macos: true,  windows: true,  url: "https://github.com/php/pecl-file_formats-yaml" },
    SpcExt { name: "zip",          linux: true,  macos: true,  windows: true,  url: "https://pecl.php.net/package/zip" },
    SpcExt { name: "zlib",         linux: true,  macos: true,  windows: true,  url: "" },
    SpcExt { name: "zstd",         linux: true,  macos: true,  windows: true,  url: "https://github.com/kjdev/php-ext-zstd" },
];

pub fn cmd_search(query: &str) -> Result<()> {
    let (_, installed) = get_extensions()?;
    let installed_names: std::collections::HashSet<String> =
        installed.iter().map(|e| e.name.to_lowercase()).collect();

    let q = query.to_lowercase();
    let results: Vec<&SpcExt> = SPC_REGISTRY
        .iter()
        .filter(|e| e.name.to_lowercase().contains(&q))
        .collect();

    if results.is_empty() {
        println!("No extensions matching {:?}", query);
        println!("Browse all: https://static-php.dev/en/guide/extensions.html");
        return Ok(());
    }

    let name_w = results.iter().map(|e| e.name.len()).max().unwrap_or(4).max(4);

    println!(
        "{:<name_w$}  {:^8}  {:^5}  {:^7}  {}",
        "NAME", "INSTALLED", "LINUX", "WINDOWS", "URL",
        name_w = name_w,
    );
    println!("{}", "\u{2500}".repeat(name_w + 40));

    for ext in results {
        let installed = if installed_names.contains(ext.name) { "✓" } else { "—" };
        let linux   = if ext.linux   { "✓" } else { "—" };
        let windows = if ext.windows { "✓" } else { "—" };
        let url = if ext.url.is_empty() { "(bundled)" } else { ext.url };
        println!(
            "{:<name_w$}  {:^9}  {:^5}  {:^7}  {}",
            ext.name, installed, linux, windows, url,
            name_w = name_w,
        );
    }

    Ok(())
}

// ── `ephpm ext build` ─────────────────────────────────────────────────────────

pub struct BuildArgs {
    pub add: Vec<String>,
    pub suite: Option<String>,
    pub output: Option<String>,
}

pub fn cmd_build(args: &BuildArgs) -> Result<()> {
    // 1. Detect container engine
    let engine = detect_container_engine()?;
    println!("  ■ Using container engine: {engine}");

    // 2. Resolve base suite extensions
    let base = suite_extensions(args.suite.as_deref().unwrap_or("core"));

    // 3. Parse --add list
    let add: Vec<String> = args.add.iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // 4. Merge, deduplicate
    let mut all: Vec<String> = base.clone();
    for ext in &add {
        if !all.contains(ext) {
            all.push(ext.clone());
        }
    }

    let suite_name = args.suite.as_deref().unwrap_or("core");
    println!("  ■ Suite:  {} ({} extensions)", suite_name, base.len());
    if !add.is_empty() {
        println!("  ■ Adding: {}", add.join(", "));
    }
    println!("  ■ Total:  {} extensions", all.len());
    println!("  ■ List:   {}", all.join(","));

    // 5. Output path
    let output = args.output.as_deref().unwrap_or("./ephpm-custom");
    let output_abs = std::fs::canonicalize(".")
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(output.trim_start_matches("./"));
    let output_dir = output_abs.parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_string_lossy()
        .to_string();

    // 6. Determine builder image — prefer local build, fall back to registry
    let version = env!("CARGO_PKG_VERSION");
    let registry_image = format!("ghcr.io/ephpm/builder:{version}");
    let local_image = "ephpm-builder:local".to_string();
    let image = {
        // Check if local image exists
        let has_local = std::process::Command::new(&engine)
            .args(["image", "inspect", &local_image])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if has_local { local_image } else { registry_image }
    };
    println!("  ■ Image:  {image}");
    println!("  ■ Output: {output}");
    println!();
    println!("  Pulling builder image (this may take a minute on first run)...");

    // 7. Run the container
    // Pass GITHUB_TOKEN if set — spc needs it to avoid rate limiting
    let token_arg: Vec<String> = if let Ok(tok) = std::env::var("GITHUB_TOKEN") {
        vec!["-e".into(), format!("GITHUB_TOKEN={tok}")]
    } else {
        vec![]
    };

    let mut docker_cmd = std::process::Command::new(&engine);
    docker_cmd.arg("run").arg("--rm");
    for arg in &token_arg {
        docker_cmd.arg(arg);
    }
    docker_cmd
        .arg("-e").arg(format!("EXTENSIONS={}", all.join(",")))
        .arg("-e").arg(format!("OUTPUT=/output/{}", std::path::Path::new(output)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("ephpm"))
            .to_string_lossy()))
        .arg("-v").arg(format!("{}:/output", output_dir))
        .arg("-v").arg(format!("{}:/src/ephpm", std::env::current_dir().unwrap().display()))
        .arg("-v").arg("ephpm-spc-cache:/build")
        .arg(&image);

    let status = docker_cmd
        .status()
        .context("failed to run builder container — is Docker/Podman running?")?;

    if !status.success() {
        anyhow::bail!("builder container failed with status: {status}");
    }

    println!();
    println!("  ✓ Build complete");
    println!("    Verify: {output} ext list");
    Ok(())
}

fn detect_container_engine() -> Result<String> {
    if let Ok(e) = std::env::var("CONTAINER_ENGINE") {
        if !e.is_empty() { return Ok(e); }
    }
    for candidate in ["podman", "docker"] {
        if std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Ok(candidate.to_string());
        }
    }
    anyhow::bail!(
        "No container engine found.\n  Install Docker: https://docs.docker.com/get-docker/\n  Or set CONTAINER_ENGINE=docker"
    )
}

fn suite_extensions(suite: &str) -> Vec<String> {
    let exts = match suite {
        "wordpress" => "bcmath,calendar,curl,dom,exif,fileinfo,filter,gd,hash,iconv,json,libxml,mbstring,mysqli,mysqlnd,openssl,pcre,pdo,pdo_mysql,pdo_sqlite,phar,session,simplexml,sodium,sqlite3,tokenizer,xml,xmlreader,xmlwriter,zip,zlib",
        "laravel"   => "bcmath,curl,dom,fileinfo,filter,gd,hash,iconv,igbinary,intl,json,libxml,mbstring,openssl,pcre,pdo,pdo_mysql,pdo_pgsql,pdo_sqlite,phar,posix,redis,session,simplexml,sodium,sqlite3,tokenizer,xml,xmlreader,xmlwriter,zip,zlib",
        "full"      => "amqp,apcu,bcmath,brotli,bz2,calendar,curl,dba,decimal,dom,ds,ev,exif,fileinfo,filter,ftp,gd,gmp,iconv,igbinary,imagick,intl,json,ldap,libxml,lz4,mbstring,memcached,mongodb,msgpack,mysqli,mysqlnd,openssl,pcntl,pdo,pdo_mysql,pdo_pgsql,pdo_sqlite,pgsql,phar,posix,rdkafka,readline,redis,session,simdjson,simplexml,snappy,soap,sockets,sodium,sqlite3,ssh2,tokenizer,uuid,xml,xmlreader,xmlwriter,xsl,yaml,zip,zlib,zstd",
        _ /* core */ => "bcmath,curl,dom,fileinfo,filter,libxml,mbstring,openssl,phar,session,simplexml,sodium,tokenizer,xml,xmlreader,xmlwriter,zip,zlib",
    };
    exts.split(',').map(|s| s.to_string()).collect()
}
