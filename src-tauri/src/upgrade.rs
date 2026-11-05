//! `dalzzack upgrade`: replace this binary with the latest release, or build
//! from source with cargo when `--path` or `--git` asks for that.
//!
//! The release path changes nothing until the new binary is on disk and has
//! passed its hash and version checks. Daemons are stopped only after that, so a
//! failed download costs nothing.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// The repository URL from `Cargo.toml`.
const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The archive that `release.yml` attaches for this platform.
#[cfg(target_os = "macos")]
const RELEASE_ASSET: Option<&str> = Some("dalzzack-macos-universal.tar.gz");
#[cfg(target_os = "windows")]
const RELEASE_ASSET: Option<&str> = Some("dalzzack-windows-x64.zip");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const RELEASE_ASSET: Option<&str> = None;

/// What the user asked for.
#[derive(Debug, Default)]
pub struct Options {
    pub check: bool,
    pub force: bool,
    pub tag: Option<String>,
    pub keep_daemons: bool,
    pub path: Option<PathBuf>,
    pub git: Option<String>,
    pub rev: Option<String>,
    pub no_locked: bool,
}

impl Options {
    fn builds_from_source(&self) -> bool {
        self.path.is_some() || self.git.is_some() || self.rev.is_some() || self.no_locked
    }
}

/// Run `upgrade`. `Err` is the JSON report of a failure.
pub fn run(options: Options) -> Result<Value, Value> {
    if options.builds_from_source() {
        from_source(options)
    } else {
        from_release(&options)
    }
}

fn failure(error: &str, message: impl std::fmt::Display, hint: &str) -> Value {
    json!({
        "status": "failed",
        "error": error,
        "message": message.to_string(),
        "hint": hint,
    })
}

fn http_client(follow_redirects: bool) -> Result<reqwest::Client, Value> {
    let redirects = if follow_redirects {
        reqwest::redirect::Policy::default()
    } else {
        reqwest::redirect::Policy::none()
    };
    reqwest::Client::builder()
        .user_agent(concat!("dalzzack/", env!("CARGO_PKG_VERSION")))
        .redirect(redirects)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| failure("network_error", e, ""))
}

/// `https://…/releases/tag/v1.2.3` → `v1.2.3`.
fn tag_from_location(location: &str) -> Option<String> {
    let (_, tag) = location.rsplit_once("/releases/tag/")?;
    (!tag.is_empty() && !tag.contains('/')).then(|| tag.to_string())
}

/// The newest published release. `releases/latest` redirects to its tag, and
/// this needs no GitHub API call, so the API rate limit does not apply.
/// Pre-releases and drafts are never `latest`.
async fn latest_tag(repo: &str) -> Result<String, Value> {
    let hint = "Check the network and any proxy settings (HTTPS_PROXY).";
    let response = http_client(false)?
        .get(format!("{repo}/releases/latest"))
        .send()
        .await
        .map_err(|e| failure("network_error", e, hint))?;
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|location| location.to_str().ok());
    location.and_then(tag_from_location).ok_or_else(|| {
        let message = format!("{repo}/releases/latest answered {}", response.status());
        failure("no_latest_release", message, hint)
    })
}

fn version_of(tag: &str) -> Result<semver::Version, Value> {
    semver::Version::parse(tag.trim_start_matches('v')).map_err(|e| {
        failure(
            "invalid_tag",
            format!("{tag} is not a version: {e}"),
            "Use a tag such as v1.2.3.",
        )
    })
}

fn parse_sha256(text: &str) -> Option<String> {
    let hash = text.split_whitespace().next()?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
}

fn sha256_of(file: &Path) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut std::fs::File::open(file)?, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

async fn download(url: &str, file: &Path) -> Result<(), String> {
    let client = http_client(true).map_err(|e| e["message"].to_string())?;
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|e| e.to_string())?;
    let mut out = tokio::fs::File::create(file)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        out.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    out.flush().await.map_err(|e| e.to_string())
}

/// The operating system's own tar. On Windows it reads zip files. Git Bash can
/// put GNU tar first on PATH, and that one cannot, so the path is absolute.
fn system_tar() -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(root).join("System32").join("tar.exe")
    } else {
        PathBuf::from("/usr/bin/tar")
    }
}

/// Download `tag`, check its hash, and extract the binary into `work`.
async fn fetch_release(repo: &str, tag: &str, asset: &str, work: &Path) -> Result<PathBuf, Value> {
    let url = format!("{repo}/releases/download/{tag}/{asset}");
    let hint = "Check that the release has this file, and check the network.";

    let archive = work.join(asset);
    let checksum = work.join(format!("{asset}.sha256"));
    download(&url, &archive)
        .await
        .map_err(|e| failure("download_failed", format!("{url}: {e}"), hint))?;
    download(&format!("{url}.sha256"), &checksum)
        .await
        .map_err(|e| failure("download_failed", format!("{url}.sha256: {e}"), hint))?;

    let expected = std::fs::read_to_string(&checksum)
        .ok()
        .and_then(|text| parse_sha256(&text));
    let actual = sha256_of(&archive).map_err(|e| failure("download_failed", e, hint))?;
    if expected.as_deref() != Some(actual.as_str()) {
        return Err(failure(
            "checksum_mismatch",
            format!("{asset} has hash {actual}, not {expected:?}"),
            "Nothing was changed. Try again. If this repeats, report it.",
        ));
    }

    let extracted = Command::new(system_tar())
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(work)
        .output()
        .map_err(|e| failure("extract_failed", e, "Nothing was changed."))?;
    let binary = work.join(format!("dalzzack{}", std::env::consts::EXE_SUFFIX));
    if !extracted.status.success() || !binary.exists() {
        let message = String::from_utf8_lossy(&extracted.stderr);
        return Err(failure(
            "extract_failed",
            format!("{asset} did not give {}: {message}", binary.display()),
            "Nothing was changed.",
        ));
    }
    Ok(binary)
}

/// What `--version` prints for a tag. A release candidate carries the version of
/// the release it leads to, so `v1.1.0-rc.1` is a build of 1.1.0.
fn version_line(tag: &str) -> Result<String, Value> {
    let version = version_of(tag)?;
    Ok(format!(
        "dalzzack {}.{}.{}",
        version.major, version.minor, version.patch
    ))
}

/// The new binary must run, and must be the version the tag promises.
fn check_binary(binary: &Path, tag: &str) -> Result<(), Value> {
    let hint = "Nothing was changed.";
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| failure("invalid_release", format!("it does not run: {e}"), hint))?;
    let printed = String::from_utf8_lossy(&output.stdout);
    if printed.trim() == version_line(tag)? {
        return Ok(());
    }
    Err(failure(
        "invalid_release",
        format!("{tag} printed {:?} for --version", printed.trim()),
        hint,
    ))
}

/// A folder next to the binary, so that the last rename stays on one volume.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new(install_dir: &Path) -> std::io::Result<Self> {
        let dir = install_dir.join(format!(".dalzzack-upgrade-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Put `new` at `target`. Windows cannot overwrite an exe that runs, and this
/// process is that exe, but it can rename it. The `.old` file goes at the next
/// start (`sweep_moved_aside`). macOS kills a signed binary that is rewritten in
/// place, so there the rename replaces the file as a whole.
fn replace_binary(new: &Path, target: &Path, move_aside: bool) -> std::io::Result<()> {
    if !move_aside {
        return std::fs::rename(new, target);
    }
    let aside = moved_aside_path(target);
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(target, &aside)?;
    std::fs::rename(new, target).inspect_err(|_| {
        let _ = std::fs::rename(&aside, target);
    })
}

/// True for a binary in a cargo `target` folder: `upgrade` would replace a build
/// that the next `cargo build` overwrites.
fn is_dev_build(exe: &Path) -> bool {
    let in_profile_dir = exe
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "debug" || name == "release");
    in_profile_dir
        && exe
            .ancestors()
            .skip(2)
            .take(2)
            .any(|dir| dir.file_name().is_some_and(|name| name == "target"))
}

/// Rewrite the guide with the new binary, so that it matches the new commands.
fn refresh_guides(report: &mut Value, binary: &Path) {
    let output = Command::new(binary)
        .args(["install", "--refresh"])
        .stderr(Stdio::null())
        .output();
    let refreshed = output
        .ok()
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
        .unwrap_or_default();
    match refreshed["status"].as_str() {
        Some("completed") => report["guides_refreshed"] = refreshed["refreshed"].clone(),
        _ => {
            report["warning"] = json!(format!(
                "The installed guides were not refreshed. Run `dalzzack install --refresh`. {}",
                refreshed["message"].as_str().unwrap_or_default()
            ))
        }
    }
}

fn from_release(options: &Options) -> Result<Value, Value> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| failure("network_error", e, ""))?;

    let tag = match &options.tag {
        Some(tag) if tag.starts_with('v') => tag.clone(),
        Some(tag) => format!("v{tag}"),
        None => runtime.block_on(latest_tag(REPO_URL))?,
    };
    let current = version_of(CURRENT_VERSION)?;
    let order = current.cmp(&version_of(&tag)?);
    let report =
        |status: &str| json!({ "status": status, "current": CURRENT_VERSION, "latest": tag });

    if options.check {
        return Ok(report(match order {
            Ordering::Less => "update_available",
            Ordering::Equal => "up_to_date",
            Ordering::Greater => "ahead",
        }));
    }
    // A tag is an explicit request, also for an older version: that is a rollback.
    let wanted = options.force
        || order == Ordering::Less
        || (options.tag.is_some() && order != Ordering::Equal);
    if !wanted {
        return Ok(report(if order == Ordering::Equal {
            "up_to_date"
        } else {
            "ahead"
        }));
    }

    let exe = std::env::current_exe().map_err(|e| failure("no_install_path", e, ""))?;
    let target = PathBuf::from(crate::state::printable_path(&exe));
    if is_dev_build(&target) {
        return Err(failure(
            "dev_build",
            format!("{} is a cargo build", target.display()),
            "Use `upgrade --path <repo>/src-tauri` to install from source.",
        ));
    }
    let Some(asset) = RELEASE_ASSET else {
        return Err(failure(
            "no_release_asset",
            "Releases have no binary for this platform.",
            "Use `upgrade --path <repo>/src-tauri` or `upgrade --git <url>` to build from source.",
        ));
    };
    let alive = list_running_instance_ids();
    if options.keep_daemons && !alive.is_empty() {
        return Err(daemons_running(alive));
    }
    let install_dir = target.parent().unwrap_or(Path::new("."));
    let work = WorkDir::new(install_dir).map_err(|e| {
        failure(
            "install_dir_not_writable",
            format!("{}: {e}", install_dir.display()),
            "Run the install command from the README: it installs to a user folder. Then \
             delete this copy, from an administrator terminal if it needs one.",
        )
    })?;

    let new_binary = runtime.block_on(fetch_release(REPO_URL, &tag, asset, &work.0))?;
    check_binary(&new_binary, &tag)?;

    for id in &alive {
        stop_or_kill(id);
    }
    replace_binary(&new_binary, &target, cfg!(windows)).map_err(|e| {
        failure(
            "replace_failed",
            format!("{}: {e}", target.display()),
            "The old binary is still in place. Something may have the file open: stop \
             daemons started from an older copy, or wait for an antivirus scan.",
        )
    })?;

    let mut out = json!({
        "status": "upgraded",
        "from": CURRENT_VERSION,
        "to": tag.trim_start_matches('v'),
        "installed_to": crate::state::printable_path(&target),
        "stopped_daemons": alive,
    });
    refresh_guides(&mut out, &target);
    Ok(out)
}

fn daemons_running(ids: Vec<String>) -> Value {
    json!({
        "status": "failed",
        "error": "daemons_running",
        "ids": ids,
        "message": "Refusing to upgrade with daemons running. Stop them or omit --keep-daemons.",
    })
}

fn cargo_on_path() -> bool {
    Command::new("cargo")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Same file, also when one path goes through a symlink or differs in case
/// on a case-insensitive file system.
fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Build from source with `cargo install`.
fn from_source(options: Options) -> Result<Value, Value> {
    if !cargo_on_path() {
        return Err(failure(
            "cargo_not_found",
            "--path, --git, --rev, and --no-locked build from source with cargo, and cargo is \
             not on PATH.",
            "Run `dalzzack upgrade` with none of these options to install the latest release. \
             Or install Rust from https://rustup.rs and run this again.",
        ));
    }
    // Captured before step 3 renames the running image on Windows.
    let running_from = std::env::current_exe().ok();

    // 1. Discover and stop running daemons (unless --keep-daemons).
    let alive = list_running_instance_ids();
    if !alive.is_empty() {
        if options.keep_daemons {
            return Err(daemons_running(alive));
        }
        eprintln!(
            "[upgrade] stopping {} running daemon(s): {:?}",
            alive.len(),
            alive
        );
        for id in &alive {
            stop_or_kill(id);
        }
    }

    // 2. Pick source.
    let mut cmd = Command::new("cargo");
    cmd.arg("install");
    if !options.no_locked {
        cmd.arg("--locked");
    }
    cmd.arg("--force");

    if let Some(path) = options.path {
        eprintln!("[upgrade] cargo install --path {} --force", path.display());
        cmd.arg("--path").arg(path);
    } else {
        let url = options.git.unwrap_or_else(|| REPO_URL.to_string());
        if url.is_empty() {
            return Err(failure(
                "no_source",
                "No repository URL baked in. Pass --git <URL> or --path <DIR>.",
                "",
            ));
        }
        eprintln!("[upgrade] cargo install --git {} --force", url);
        cmd.arg("--git").arg(&url);
        if let Some(rev) = options.rev {
            cmd.arg("--rev").arg(rev);
        }
    }

    // 3. Windows: get the running image out of cargo's way.
    //
    // Windows refuses to overwrite or delete an executable that is currently
    // running — and the process running `dalzzack upgrade` IS the installed
    // `dalzzack.exe`, so `cargo install` built fine and then failed on the
    // final move with os error 5, with no daemon left to blame. A running image
    // may be *renamed*, though: move it aside to `.old`, let cargo drop the new
    // file into place, and sweep the `.old` on the next start (the rustup /
    // cargo-update pattern).
    let install_target = cargo_install_target();
    let moved_aside = moved_aside_path(&install_target);
    #[cfg(target_os = "windows")]
    if install_target.exists() {
        let _ = std::fs::remove_file(&moved_aside);
        match std::fs::rename(&install_target, &moved_aside) {
            Ok(()) => eprintln!(
                "[upgrade] moved running image aside: {} -> {}",
                install_target.display(),
                moved_aside.display()
            ),
            Err(e) => eprintln!(
                "[upgrade] could not move {} aside ({e}); cargo may fail to replace it",
                install_target.display()
            ),
        }
    }

    // 4. Run cargo install.
    let status = cmd.status().map_err(|e| {
        failure(
            "cargo_spawn_failed",
            format!("Failed to run cargo: {e}. Is cargo on PATH?"),
            "",
        )
    })?;

    if !status.success() {
        // Put the previous binary back if cargo never produced a new one, so a
        // failed upgrade leaves a working `dalzzack` rather than none.
        let restored = if moved_aside.exists() && !install_target.exists() {
            std::fs::rename(&moved_aside, &install_target).is_ok()
        } else {
            false
        };
        return Err(json!({
            "status": "failed",
            "error": "cargo_install_failed",
            "exit_code": status.code(),
            "install_target": install_target.display().to_string(),
            "previous_binary_restored": restored,
            "hint": "Read cargo's own output above for the failing step. If it is the final \
                     move onto the install target with 'os error 5', something still has that \
                     file open — a daemon started from THIS binary (`dalzzack list`), or an \
                     antivirus scan; the running upgrade process itself is moved aside first \
                     and is not the cause.",
        }));
    }
    // The old image is still mapped by this very process, so deleting it here
    // fails on Windows — it is swept on the next start (`sweep_moved_aside`).
    let _ = std::fs::remove_file(&moved_aside);

    // 5. macOS: ad-hoc re-codesign the new binary so Gatekeeper doesn't grump.
    //    The new binary is the one cargo installed, which is not this process's
    //    image when dalzzack was installed from a release archive.
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&install_target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    let mut out = json!({
        "status": "upgraded",
        "stopped_daemons": alive,
        "installed_to": install_target.display().to_string(),
    });
    // A binary from a release archive lives outside cargo's bin directory, so
    // the new build did not replace it and the old copy may still win on PATH.
    if let Some(exe) = running_from.filter(|exe| !same_path(exe, &install_target)) {
        out["note"] = format!(
            "This command ran from {}, which cargo did not replace. Delete that copy, \
             or put {} before it on PATH.",
            exe.display(),
            install_target
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )
        .into();
    }
    refresh_guides(&mut out, &install_target);
    Ok(out)
}

/// Where `cargo install` puts the binary, in cargo's own precedence:
/// `$CARGO_INSTALL_ROOT/bin`, else `$CARGO_HOME/bin`, else `~/.cargo/bin`.
fn cargo_install_target() -> PathBuf {
    let root = std::env::var_os("CARGO_INSTALL_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
        .unwrap_or_else(|| crate::state::home_dir().join(".cargo"));
    root.join("bin")
        .join(format!("dalzzack{}", std::env::consts::EXE_SUFFIX))
}

/// The name a running image is renamed to while it is replaced.
fn moved_aside_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Delete the `.old` image a previous `upgrade` left behind. The upgrade
/// itself cannot: on Windows that file was its own running image.
pub fn sweep_moved_aside() {
    // Both the configured install target and wherever this binary actually
    // lives — they differ when the exe was copied or a CARGO_INSTALL_ROOT
    // changed between the upgrade and now.
    let mut candidates = vec![moved_aside_path(&cargo_install_target())];
    if let Ok(exe) = std::env::current_exe() {
        candidates.push(moved_aside_path(&exe));
    }
    candidates.dedup();
    for old in candidates {
        if old.exists() {
            match std::fs::remove_file(&old) {
                Ok(()) => tracing::debug!("[upgrade] swept {}", old.display()),
                // Still mapped by another process (an older daemon, say); next time.
                Err(e) => tracing::debug!("[upgrade] could not sweep {}: {e}", old.display()),
            }
        }
    }
}

/// Scan the temp dir for `dalzzack-*.port` files and return the instance IDs.
/// Doesn't ping — `stop_or_kill` will figure out whether a graceful shutdown works.
fn list_running_instance_ids() -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix("dalzzack-") {
                if let Some(id) = rest.strip_suffix(".port") {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids.sort();
    ids
}

/// Try graceful shutdown; if it errors out (hung daemon), force-kill by PID.
/// Both calls are best-effort — we don't want a stuck daemon to block the upgrade.
fn stop_or_kill(id: &str) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dalzzack"));
    let stop = Command::new(&exe)
        .args(["stop", "--id", id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if matches!(stop, Ok(s) if s.success()) {
        eprintln!("[upgrade]   stopped {}", id);
        return;
    }
    eprintln!("[upgrade]   stop failed for {} — force-killing", id);
    let _ = Command::new(&exe)
        .args(["kill", "--id", id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// A folder for one test, removed at the end.
    fn scratch(name: &str) -> WorkDir {
        let dir = std::env::temp_dir().join(format!("dalzzack-t-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        WorkDir(dir)
    }

    /// A release server. `/releases/latest` redirects to `v9.9.9`; other paths
    /// come from `files`. Returns the base URL.
    async fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let location = format!("{base}/releases/tag/v9.9.9");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0; 4096];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let found = files.iter().find(|(name, _)| *name == path);
                let (head, body): (String, &[u8]) = match found {
                    Some((_, body)) => ("200 OK".to_string(), body.as_slice()),
                    None if path == "/releases/latest" => {
                        (format!("302 Found\r\nLocation: {location}"), &[][..])
                    }
                    None => ("404 Not Found".to_string(), &[][..]),
                };
                let response = format!(
                    "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(body).await;
            }
        });
        base
    }

    /// An archive of the same kind as a release, with a fake binary in it.
    fn release_archive(dir: &Path, asset: &str) -> Vec<u8> {
        let binary = format!("dalzzack{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(dir.join(&binary), b"new binary").unwrap();
        let status = Command::new(system_tar())
            .arg("-a")
            .arg("-cf")
            .arg(dir.join(asset))
            .arg("-C")
            .arg(dir)
            .arg(&binary)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(dir.join(asset)).unwrap()
    }

    fn asset_name() -> &'static str {
        if cfg!(windows) {
            "dalzzack-windows-x64.zip"
        } else {
            "dalzzack-macos-universal.tar.gz"
        }
    }

    #[tokio::test]
    async fn the_latest_tag_comes_from_the_redirect() {
        let base = serve(Vec::new()).await;
        assert_eq!(latest_tag(&base).await.unwrap(), "v9.9.9");
    }

    #[tokio::test]
    async fn a_release_downloads_verifies_and_extracts() {
        let source = scratch("source");
        let asset = asset_name();
        let archive = release_archive(&source.0, asset);
        let hash = format!("{:x}", Sha256::digest(&archive));
        let path = format!("/releases/download/v9.9.9/{asset}");
        let base = serve(vec![
            (path.clone(), archive),
            (
                format!("{path}.sha256"),
                format!("{hash}  {asset}\n").into_bytes(),
            ),
        ])
        .await;

        let work = scratch("work");
        let binary = fetch_release(&base, "v9.9.9", asset, &work.0)
            .await
            .unwrap();
        assert_eq!(std::fs::read(binary).unwrap(), b"new binary");
    }

    #[tokio::test]
    async fn a_wrong_hash_stops_before_anything_is_extracted() {
        let source = scratch("badhash");
        let asset = asset_name();
        let archive = release_archive(&source.0, asset);
        let path = format!("/releases/download/v9.9.9/{asset}");
        let wrong = format!("{}  {asset}\n", "0".repeat(64)).into_bytes();
        let base = serve(vec![
            (path.clone(), archive),
            (format!("{path}.sha256"), wrong),
        ])
        .await;

        let work = scratch("badhash-work");
        let error = fetch_release(&base, "v9.9.9", asset, &work.0)
            .await
            .unwrap_err();
        assert_eq!(error["error"], "checksum_mismatch");
        assert!(!work
            .0
            .join(format!("dalzzack{}", std::env::consts::EXE_SUFFIX))
            .exists());
    }

    #[tokio::test]
    async fn a_missing_asset_is_a_download_failure() {
        let base = serve(Vec::new()).await;
        let work = scratch("missing");
        let error = fetch_release(&base, "v9.9.9", "nope.zip", &work.0)
            .await
            .unwrap_err();
        assert_eq!(error["error"], "download_failed");
    }

    #[test]
    fn the_tag_is_read_from_the_redirect_url() {
        let tag = tag_from_location;
        assert_eq!(
            tag("https://github.com/gosuda/dalzzack/releases/tag/v1.2.3").as_deref(),
            Some("v1.2.3")
        );
        assert_eq!(tag("https://github.com/gosuda/dalzzack/releases"), None);
        assert_eq!(tag("https://x.test/releases/tag/"), None);
    }

    #[test]
    fn versions_compare_as_versions() {
        let v = |tag| version_of(tag).unwrap();
        assert!(v("v1.10.0") > v("v1.9.0"));
        assert!(v("1.1.0") == v("v1.1.0"));
        // A release candidate is older than its release, and newer than the last one.
        assert!(v("v1.1.0-rc.1") < v("v1.1.0"));
        assert!(v("v1.1.0-rc.1") > v("v1.0.0"));
        assert_eq!(version_of("latest").unwrap_err()["error"], "invalid_tag");
    }

    #[test]
    fn the_hash_file_is_read_in_both_formats() {
        let hash = "AB".repeat(32);
        assert_eq!(
            parse_sha256(&format!("{hash}  a.zip\n")).unwrap(),
            hash.to_lowercase()
        );
        assert!(parse_sha256(&hash).is_some());
        assert!(parse_sha256("short  a.zip").is_none());
        assert!(parse_sha256("").is_none());
        assert!(parse_sha256(&"z".repeat(64)).is_none());
    }

    #[test]
    fn the_hash_of_a_file_matches_a_known_value() {
        let dir = scratch("hash");
        let file = dir.0.join("abc");
        std::fs::write(&file, "abc").unwrap();
        assert_eq!(
            sha256_of(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_release_candidate_is_a_build_of_its_release() {
        assert_eq!(version_line("v1.1.0").unwrap(), "dalzzack 1.1.0");
        assert_eq!(version_line("v1.1.0-rc.1").unwrap(), "dalzzack 1.1.0");
    }

    #[test]
    fn a_binary_with_the_wrong_version_is_refused() {
        // This test binary answers --version with something else, or fails.
        let this = std::env::current_exe().unwrap();
        assert_eq!(
            check_binary(&this, "v9.9.9").unwrap_err()["error"],
            "invalid_release"
        );
        let missing = this.with_file_name("no-such-binary");
        assert_eq!(
            check_binary(&missing, "v9.9.9").unwrap_err()["error"],
            "invalid_release"
        );
    }

    #[test]
    fn replacing_moves_the_old_binary_aside_where_asked() {
        let dir = scratch("swap");
        let (new, target) = (dir.0.join("new"), dir.0.join("dalzzack.exe"));

        std::fs::write(&new, "2").unwrap();
        std::fs::write(&target, "1").unwrap();
        replace_binary(&new, &target, true).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "2");
        assert_eq!(
            std::fs::read_to_string(moved_aside_path(&target)).unwrap(),
            "1"
        );

        std::fs::write(&new, "3").unwrap();
        replace_binary(&new, &target, false).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "3");
    }

    #[test]
    fn a_failed_replace_restores_the_old_binary() {
        let dir = scratch("restore");
        let target = dir.0.join("dalzzack.exe");
        std::fs::write(&target, "1").unwrap();

        assert!(replace_binary(&dir.0.join("missing"), &target, true).is_err());

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "1");
        assert!(!moved_aside_path(&target).exists());
    }

    #[test]
    fn only_a_cargo_output_folder_is_a_dev_build() {
        let dev = |path: &str| is_dev_build(Path::new(path));
        assert!(dev("/repo/src-tauri/target/debug/dalzzack"));
        assert!(dev("/repo/target/release/dalzzack"));
        assert!(dev("/repo/target/universal-apple-darwin/release/dalzzack"));
        assert!(!dev("/usr/local/bin/dalzzack"));
        assert!(!dev("/home/a/.cargo/bin/dalzzack"));
        assert!(!dev("/home/a/release/dalzzack"));
    }

    #[test]
    fn source_options_choose_cargo() {
        let source = |options: Options| options.builds_from_source();
        assert!(!source(Options::default()));
        assert!(!source(Options {
            force: true,
            tag: Some("v1.0.0".into()),
            ..Default::default()
        }));
        assert!(source(Options {
            path: Some(".".into()),
            ..Default::default()
        }));
        assert!(source(Options {
            git: Some("u".into()),
            ..Default::default()
        }));
        assert!(source(Options {
            rev: Some("main".into()),
            ..Default::default()
        }));
    }

    #[test]
    fn moved_aside_name_appends_old_to_the_full_filename() {
        assert_eq!(
            moved_aside_path(Path::new("/x/dalzzack.exe")),
            PathBuf::from("/x/dalzzack.exe.old")
        );
        assert_eq!(
            moved_aside_path(Path::new("/x/dalzzack")),
            PathBuf::from("/x/dalzzack.old")
        );
    }
}
