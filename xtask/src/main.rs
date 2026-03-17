use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::{env, fs};

const PHP_EXTENSIONS: &str = "bcmath,calendar,ctype,curl,dom,exif,fileinfo,filter,\
    gd,hash,iconv,mbstring,mysqli,mysqlnd,openssl,pcntl,pcre,pdo,pdo_mysql,phar,\
    posix,session,simplexml,sodium,tokenizer,xml,xmlreader,xmlwriter,zip,zlib";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);

    match cmd {
        Some("release") => require_unix(|| release(&args[1..])),
        Some("php-sdk") => require_unix(|| php_sdk(&args[1..])),
        Some("e2e") => e2e(&args[1..]),
        Some("e2e-up") => e2e_up(&args[1..]),
        Some("e2e-down") => e2e_down(),
        Some("e2e-install") => e2e_install(),
        Some("help" | "--help" | "-h") | None => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!(
        "\
Usage: cargo xtask <command> [options]

Commands:
  release [8.5]              Build ephpm with PHP linked (default: 8.5)
  php-sdk [8.5]              Build only the PHP SDK (libphp.a + headers)
  e2e [--php-version 8.5]    Run E2E tests (creates Kind cluster, builds images, tilt ci)
  e2e-up [--php-version 8.5] Start E2E dev environment (tilt dashboard at localhost:10350)
  e2e-down                   Tear down Kind cluster and all resources
  e2e-install                Download kind, tilt, kubectl to ./bin (no global install needed)"
    );
}

/// Parse `--php-version <ver>` from args, defaulting to "8.5".
fn parse_php_version(args: &[String]) -> &str {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--php-version" {
            if let Some(ver) = args.get(i + 1) {
                return ver;
            }
        }
    }
    "8.5"
}

/// Build the PHP SDK and then compile the release binary.
///
/// static-php-cli compiles PHP with musl, so we must target musl for the
/// Rust binary too. This produces a fully static, self-contained binary.
fn release(args: &[String]) -> ExitCode {
    let spc_dir = spc_dir();
    let sdk_path = spc_dir.join("buildroot");

    if !sdk_path.join("lib").join("libphp.a").exists() {
        let code = php_sdk(args);
        if code != ExitCode::SUCCESS {
            return code;
        }
    } else {
        eprintln!("==> PHP SDK already built, skipping (delete {spc_dir:?} to rebuild)");
    }

    // static-php-cli builds PHP with musl. The Rust binary must target
    // musl too, otherwise we get linker errors (sigsetjmp, __flt_rounds, etc.)
    let target = musl_target_triple();

    // Ensure the musl target is installed
    eprintln!("==> Ensuring Rust target {target} is installed...");
    let status = Command::new("rustup")
        .args(["target", "add", &target])
        .status();
    if !ran_ok(&status) {
        eprintln!("error: failed to add Rust target {target}");
        return ExitCode::FAILURE;
    }

    eprintln!("==> Building ephpm (release, target: {target})...");
    let status = Command::new("cargo")
        .args(["build", "--release", "--package", "ephpm", "--target", &target])
        .env("PHP_SDK_PATH", &sdk_path)
        .status();

    if !ran_ok(&status) {
        eprintln!("error: cargo build failed");
        return ExitCode::FAILURE;
    }

    eprintln!("==> Binary ready: target/{target}/release/ephpm");
    ExitCode::SUCCESS
}

/// Return the musl target triple matching the current host architecture.
///
/// static-php-cli compiles PHP against musl, so our Rust binary must
/// use the same libc to avoid undefined-symbol errors at link time.
fn musl_target_triple() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

/// Build libphp.a via static-php-cli.
fn php_sdk(args: &[String]) -> ExitCode {
    let php_version = args.first().map_or("8.5", String::as_str);

    // Check for the minimum tools needed to bootstrap static-php-cli.
    // spc doctor --auto-fix handles the rest (musl toolchain, bison, flex, etc.)
    let missing: Vec<&str> = ["php", "composer", "git"]
        .into_iter()
        .filter(|cmd| !has_command(cmd))
        .collect();

    if !missing.is_empty() {
        eprintln!("error: missing required tools: {}", missing.join(", "));
        eprintln!();
        eprintln!("Install them:");
        eprintln!("  sudo apt update && sudo apt install -y php-cli composer git");
        return ExitCode::FAILURE;
    }

    let spc_dir = spc_dir();

    // Clone static-php-cli if not present
    if !spc_dir.exists() {
        eprintln!("==> Cloning static-php-cli...");
        let status = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "https://github.com/crazywhalecc/static-php-cli.git",
                spc_dir.to_str().unwrap(),
            ])
            .status();

        if !ran_ok(&status) {
            eprintln!("error: git clone failed");
            return ExitCode::FAILURE;
        }
    }

    // Install composer deps if needed.
    // Use `composer update` instead of `install` because static-php-cli's
    // lock file may require a newer PHP than what's available on the system.
    if !spc_dir.join("vendor").exists() {
        eprintln!("==> Installing static-php-cli dependencies...");
        let status = Command::new("composer")
            .args([
                "update",
                "--no-dev",
                "--no-interaction",
                "--ignore-platform-reqs",
            ])
            .current_dir(&spc_dir)
            .status();

        if !ran_ok(&status) {
            eprintln!("error: composer install failed");
            return ExitCode::FAILURE;
        }
    }

    let spc_bin = spc_dir.join("bin").join("spc");

    // Let spc install its own toolchain (musl cross-compiler, missing system
    // packages, etc.). This may prompt for sudo password.
    eprintln!("==> Checking build environment (may prompt for sudo)...");
    let status = Command::new("php")
        .args([spc_bin.to_str().unwrap(), "doctor", "--auto-fix"])
        .current_dir(&spc_dir)
        .status();

    if !ran_ok(&status) {
        eprintln!("error: spc doctor failed — check output above");
        return ExitCode::FAILURE;
    }

    // Download PHP source + extension dependencies
    eprintln!("==> Downloading PHP {php_version} sources...");
    let status = Command::new("php")
        .args([
            spc_bin.to_str().unwrap(),
            "download",
            &format!("--with-php={php_version}"),
            &format!("--for-extensions={PHP_EXTENSIONS}"),
            "--prefer-pre-built",
        ])
        .current_dir(&spc_dir)
        .status();

    if !ran_ok(&status) {
        eprintln!("error: spc download failed");
        return ExitCode::FAILURE;
    }

    // static-php-cli looks for pkg-config in its own buildroot/bin/, not system PATH
    let buildroot_bin = spc_dir.join("buildroot").join("bin");
    if !buildroot_bin.join("pkg-config").exists() {
        fs::create_dir_all(&buildroot_bin).ok();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/usr/bin/pkg-config", buildroot_bin.join("pkg-config"))
                .ok();
        }
    }

    // Build libphp.a with embed SAPI
    eprintln!("==> Building libphp.a (this takes ~15 min the first time)...");
    let status = Command::new("php")
        .args([
            spc_bin.to_str().unwrap(),
            "build",
            PHP_EXTENSIONS,
            "--build-embed",
            "--no-strip",
        ])
        .current_dir(&spc_dir)
        .status();

    if !ran_ok(&status) {
        eprintln!("error: spc build failed");
        return ExitCode::FAILURE;
    }

    eprintln!("==> PHP SDK ready at {}", spc_dir.join("buildroot").display());
    ExitCode::SUCCESS
}

// ── E2E testing (Kind + Tilt) ────────────────────────────────────────────────

const KIND_CLUSTER_NAME: &str = "ephpm-dev";
const KIND_VERSION: &str = "0.27.0";
const TILT_VERSION: &str = "0.33.21";
const KUBECTL_VERSION: &str = "1.32.0";

/// Resolve the path for a tool: check `<workspace>/bin/<name>` first, then PATH.
fn find_tool(name: &str) -> String {
    let local = workspace_root().join("bin").join(name);
    if local.exists() {
        return local.to_str().unwrap().to_string();
    }
    // On unix, also check without extension; on windows check .exe
    #[cfg(windows)]
    {
        let local_exe = workspace_root().join("bin").join(format!("{name}.exe"));
        if local_exe.exists() {
            return local_exe.to_str().unwrap().to_string();
        }
    }
    name.to_string()
}

/// Check if a tool is available (local bin or PATH).
fn has_e2e_tool(name: &str) -> bool {
    let path = find_tool(name);
    Command::new(&path)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Detect OS and architecture for download URLs.
fn platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };

    (os, arch)
}

/// Download kind, tilt, and kubectl to `<workspace>/bin/`.
fn e2e_install() -> ExitCode {
    let bin_dir = workspace_root().join("bin");
    fs::create_dir_all(&bin_dir).expect("failed to create bin directory");

    let (os, arch) = platform();

    eprintln!("==> Installing E2E tools to {}", bin_dir.display());
    eprintln!("    Platform: {os}/{arch}");

    // ── kind ─────────────────────────────────────────────────
    let kind_path = bin_dir.join("kind");
    if kind_path.exists() {
        eprintln!("==> kind already installed, skipping (delete bin/kind to reinstall)");
    } else {
        let url = format!(
            "https://github.com/kubernetes-sigs/kind/releases/download/v{KIND_VERSION}/kind-{os}-{arch}"
        );
        eprintln!("==> Downloading kind v{KIND_VERSION}...");
        if !download_binary(&url, &kind_path) {
            eprintln!("error: failed to download kind");
            return ExitCode::FAILURE;
        }
    }

    // ── kubectl ──────────────────────────────────────────────
    let kubectl_path = bin_dir.join("kubectl");
    if kubectl_path.exists() {
        eprintln!("==> kubectl already installed, skipping (delete bin/kubectl to reinstall)");
    } else {
        let url = format!(
            "https://dl.k8s.io/release/v{KUBECTL_VERSION}/bin/{os}/{arch}/kubectl"
        );
        eprintln!("==> Downloading kubectl v{KUBECTL_VERSION}...");
        if !download_binary(&url, &kubectl_path) {
            eprintln!("error: failed to download kubectl");
            return ExitCode::FAILURE;
        }
    }

    // ── tilt (tarball) ───────────────────────────────────────
    let tilt_path = bin_dir.join("tilt");
    if tilt_path.exists() {
        eprintln!("==> tilt already installed, skipping (delete bin/tilt to reinstall)");
    } else {
        // Tilt uses different naming: tilt.0.33.21.linux.x86_64.tar.gz
        let tilt_arch = if arch == "amd64" { "x86_64" } else { "arm64" };
        let url = format!(
            "https://github.com/tilt-dev/tilt/releases/download/v{TILT_VERSION}/tilt.{TILT_VERSION}.{os}.{tilt_arch}.tar.gz"
        );
        eprintln!("==> Downloading tilt v{TILT_VERSION}...");
        if !download_and_extract_tarball(&url, &bin_dir, "tilt") {
            eprintln!("error: failed to download/extract tilt");
            return ExitCode::FAILURE;
        }
    }

    eprintln!();
    eprintln!("==> E2E tools installed to {}", bin_dir.display());
    eprintln!("    kind:    {}", bin_dir.join("kind").display());
    eprintln!("    kubectl: {}", bin_dir.join("kubectl").display());
    eprintln!("    tilt:    {}", bin_dir.join("tilt").display());
    eprintln!();
    eprintln!("    The e2e/e2e-up/e2e-down commands will use these automatically.");
    ExitCode::SUCCESS
}

/// Download a single binary file via curl.
fn download_binary(url: &str, dest: &PathBuf) -> bool {
    let status = Command::new("curl")
        .args(["-fSL", "-o"])
        .arg(dest)
        .arg(url)
        .status();

    if !ran_ok(&status) {
        return false;
    }

    make_executable(dest);
    true
}

/// Download a tarball via curl, pipe through tar, extract a specific binary.
fn download_and_extract_tarball(url: &str, dest_dir: &PathBuf, binary_name: &str) -> bool {
    // curl -fSL <url> | tar xz -C <dest_dir> <binary_name>
    let curl = Command::new("curl")
        .args(["-fSL", url])
        .stdout(std::process::Stdio::piped())
        .spawn();

    let Ok(curl) = curl else {
        return false;
    };

    let status = Command::new("tar")
        .args(["xz", "-C"])
        .arg(dest_dir)
        .arg(binary_name)
        .stdin(curl.stdout.unwrap())
        .status();

    if !ran_ok(&status) {
        return false;
    }

    make_executable(&dest_dir.join(binary_name));
    true
}

/// chmod +x on unix, no-op on windows.
fn make_executable(_path: &PathBuf) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(_path) {
            let mut perms = metadata.permissions();
            perms.set_mode(perms.mode() | 0o755);
            fs::set_permissions(_path, perms).ok();
        }
    }
}

/// Run E2E tests headless: ensure cluster, build images, `tilt ci`, teardown.
fn e2e(args: &[String]) -> ExitCode {
    let php_version = parse_php_version(args);

    // Check prerequisites
    for tool in ["kind", "tilt", "kubectl"] {
        if !has_e2e_tool(tool) {
            eprintln!("error: {tool} not found. Run `cargo xtask e2e-install` to download it.");
            return ExitCode::FAILURE;
        }
    }

    let ce = container_engine();

    if ensure_kind_cluster() != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }

    if build_and_load_images(&ce, php_version) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }

    let k8s_dir = workspace_root().join("k8s");

    eprintln!("==> Running E2E tests (tilt ci, PHP {php_version})...");
    let status = Command::new(find_tool("tilt"))
        .args(["ci"])
        .env("EXPECTED_PHP_VERSION", php_version)
        .current_dir(&k8s_dir)
        .status();

    if ran_ok(&status) {
        eprintln!("==> E2E tests passed");
        ExitCode::SUCCESS
    } else {
        eprintln!("==> E2E tests failed — dumping pod logs...");
        dump_pod_logs();
        ExitCode::FAILURE
    }
}

/// Start E2E dev environment: ensure cluster, build images, `tilt up --stream`.
/// The web dashboard is available at http://localhost:10350.
/// Ctrl+C to stop.
fn e2e_up(args: &[String]) -> ExitCode {
    let php_version = parse_php_version(args);

    for tool in ["kind", "tilt", "kubectl"] {
        if !has_e2e_tool(tool) {
            eprintln!("error: {tool} not found. Run `cargo xtask e2e-install` to download it.");
            return ExitCode::FAILURE;
        }
    }

    let ce = container_engine();

    if ensure_kind_cluster() != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }

    if build_and_load_images(&ce, php_version) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }

    let k8s_dir = workspace_root().join("k8s");

    eprintln!("==> Starting Tilt (dashboard at http://localhost:10350, PHP {php_version})...");
    eprintln!("    Press Ctrl+C to stop.");
    let status = Command::new(find_tool("tilt"))
        .args(["up", "--stream"])
        .env("EXPECTED_PHP_VERSION", php_version)
        .current_dir(&k8s_dir)
        .status();

    if ran_ok(&status) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Tear down Tilt resources and delete the Kind cluster.
fn e2e_down() -> ExitCode {
    let k8s_dir = workspace_root().join("k8s");

    // tilt down (ignore errors — cluster may already be gone)
    if has_e2e_tool("tilt") {
        eprintln!("==> Removing Tilt resources...");
        Command::new(find_tool("tilt"))
            .args(["down"])
            .current_dir(&k8s_dir)
            .status()
            .ok();
    }

    if has_e2e_tool("kind") {
        eprintln!("==> Deleting Kind cluster '{KIND_CLUSTER_NAME}'...");
        let status = Command::new(find_tool("kind"))
            .args(["delete", "cluster", "--name", KIND_CLUSTER_NAME])
            .status();

        if !ran_ok(&status) {
            eprintln!("warning: kind delete failed (cluster may not exist)");
        }
    }

    eprintln!("==> E2E environment torn down");
    ExitCode::SUCCESS
}

/// Create the Kind cluster if it doesn't already exist.
fn ensure_kind_cluster() -> ExitCode {
    let kind = find_tool("kind");

    // Check if cluster already exists
    let output = Command::new(&kind)
        .args(["get", "clusters"])
        .output();

    if let Ok(output) = output {
        let clusters = String::from_utf8_lossy(&output.stdout);
        if clusters.lines().any(|line| line.trim() == KIND_CLUSTER_NAME) {
            eprintln!("==> Kind cluster '{KIND_CLUSTER_NAME}' already exists");
            return ExitCode::SUCCESS;
        }
    }

    eprintln!("==> Creating Kind cluster '{KIND_CLUSTER_NAME}'...");

    let root = workspace_root();
    let config_path = root.join("k8s").join("kind-config.yaml");

    let mut cmd = Command::new(&kind);
    cmd.args(["create", "cluster", "--name", KIND_CLUSTER_NAME]);

    if config_path.exists() {
        cmd.arg("--config").arg(&config_path);
    }

    let status = cmd.status();

    if ran_ok(&status) {
        eprintln!("==> Kind cluster ready");
        ExitCode::SUCCESS
    } else {
        eprintln!("error: failed to create Kind cluster");
        ExitCode::FAILURE
    }
}

/// Build the ephpm and E2E test runner container images, then load them into Kind.
fn build_and_load_images(ce: &str, php_version: &str) -> ExitCode {
    let root = workspace_root();
    let kind = find_tool("kind");
    let dockerfile = root.join("docker").join("Dockerfile");
    let dockerfile_e2e = root.join("docker").join("Dockerfile.e2e");

    // Build ephpm image with the specified PHP version
    if dockerfile.exists() {
        eprintln!("==> Building ephpm container image (PHP {php_version})...");
        let status = Command::new(ce)
            .args(["build", "-f"])
            .arg(&dockerfile)
            .args([
                "--build-arg",
                &format!("PHP_VERSION={php_version}"),
                "-t",
                "ephpm:dev",
                ".",
            ])
            .current_dir(&root)
            .status();

        if !ran_ok(&status) {
            eprintln!("error: failed to build ephpm image");
            return ExitCode::FAILURE;
        }
    } else {
        eprintln!("warning: docker/Dockerfile not found, skipping ephpm image build");
    }

    // Build E2E test runner image
    if dockerfile_e2e.exists() {
        eprintln!("==> Building E2E test runner image...");
        let status = Command::new(ce)
            .args(["build", "-f"])
            .arg(&dockerfile_e2e)
            .args(["-t", "ephpm-e2e:dev", "."])
            .current_dir(&root)
            .status();

        if !ran_ok(&status) {
            eprintln!("error: failed to build ephpm-e2e image");
            return ExitCode::FAILURE;
        }
    } else {
        eprintln!("warning: docker/Dockerfile.e2e not found, skipping E2E image build");
    }

    // Load images into Kind
    eprintln!("==> Loading images into Kind cluster...");
    for image in ["ephpm:dev", "ephpm-e2e:dev"] {
        let status = Command::new(&kind)
            .args(["load", "docker-image", image, "--name", KIND_CLUSTER_NAME])
            .status();

        if !ran_ok(&status) {
            eprintln!("warning: failed to load {image} into Kind (image may not exist yet)");
        }
    }

    ExitCode::SUCCESS
}

/// Dump pod logs for debugging failed E2E tests.
fn dump_pod_logs() {
    let kubectl = find_tool("kubectl");

    eprintln!("--- ephpm pod logs ---");
    Command::new(&kubectl)
        .args(["logs", "-l", "app=ephpm", "--tail=100"])
        .status()
        .ok();

    eprintln!("--- e2e job logs ---");
    Command::new(&kubectl)
        .args(["logs", "job/ephpm-e2e", "--tail=200"])
        .status()
        .ok();

    eprintln!("--- pod status ---");
    Command::new(&kubectl)
        .args(["get", "pods", "-o", "wide"])
        .status()
        .ok();
}

/// Determine which container engine to use (podman or docker).
fn container_engine() -> String {
    env::var("CONTAINER_ENGINE").unwrap_or_else(|_| {
        if has_command("podman") {
            "podman".into()
        } else {
            "docker".into()
        }
    })
}

// ── PHP SDK ─────────────────────────────────────────────────────────────────

/// Directory where static-php-cli lives.
///
/// If `SPC_DIR` is set (e.g. in the CI container image), use that directly.
/// Otherwise, clone into `<workspace>/php-sdk/static-php-cli`.
fn spc_dir() -> PathBuf {
    env::var_os("SPC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("php-sdk").join("static-php-cli"))
}

/// Find the workspace root (directory containing the root Cargo.toml).
fn workspace_root() -> PathBuf {
    let mut dir = env::current_dir().expect("cannot read current directory");
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("crates").exists() {
            return dir;
        }
        if !dir.pop() {
            // Fallback to current directory
            return env::current_dir().unwrap();
        }
    }
}

/// Building libphp.a requires a Unix C toolchain (autoconf, make, gcc).
/// On Windows, re-execute the same command inside WSL automatically.
fn require_unix(f: impl FnOnce() -> ExitCode) -> ExitCode {
    if !cfg!(windows) {
        return f();
    }

    // Check WSL is available
    if !has_command("wsl") {
        eprintln!("error: PHP SDK build requires a Unix toolchain (autoconf, make, gcc).");
        eprintln!("Install WSL: wsl --install");
        return ExitCode::FAILURE;
    }

    // Re-invoke the same xtask command inside WSL.
    // WSL auto-maps the Windows CWD to /mnt/c/... so no path conversion needed.
    let args: Vec<String> = env::args().skip(1).collect();
    // Source cargo env since bash -c doesn't run login profile
    let xtask_cmd = format!(
        "source \"$HOME/.cargo/env\" 2>/dev/null; cargo xtask {}",
        args.join(" "),
    );

    eprintln!("==> Windows detected, running via WSL...");
    let status = Command::new("wsl")
        .args(["--", "bash", "-c", &xtask_cmd])
        .status();

    if ran_ok(&status) {
        ExitCode::SUCCESS
    } else {
        eprintln!();
        eprintln!("WSL build failed. Make sure WSL has the required tools:");
        eprintln!("  wsl -- bash -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh'");
        eprintln!("  wsl -- bash -c 'sudo apt update && sudo apt install -y php-cli composer build-essential autoconf cmake pkg-config re2c libclang-dev musl-tools'");
        ExitCode::FAILURE
    }
}

fn has_command(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn ran_ok(result: &Result<std::process::ExitStatus, std::io::Error>) -> bool {
    matches!(result, Ok(s) if s.success())
}
