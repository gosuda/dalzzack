//! `dalzzack uninstall`: undo what `install`, `install.sh`, and `install.ps1` did.
//!
//! Without `--yes` this only reports. The binary goes last, because it does the
//! work: if a step fails, it is still there, and a second run finishes the job.
//! Every step does nothing when its item is absent.

use crate::daemon::{
    daemon_process, force_end_hint, process_ended, unverified_hint, DaemonProcess,
};
use crate::guide_file::{global_files, without_docs, BLOCK_BEGIN, BLOCK_END};
use crate::state::{data_dir, home_dir, printable_path};
use crate::upgrade::{failure, is_dev_build, list_running_instance_ids, same_path, stop_or_kill};
use base64::Engine;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// `tauri.conf.json`'s identifier: the browser profile is in a folder of this name.
const BUNDLE_ID: &str = "org.gosuda.dalzzack";
/// What `install.sh` writes above its `export PATH` line.
const INSTALLER_COMMENT: &str = "# Added by the dalzzack installer";
/// `pause --download-symbols` copies these next to the binary. In `System32`
/// they have the same names and are not ours, so only a folder that belongs to
/// dalzzack loses them.
const DEBUG_DLLS: [&str; 2] = ["dbghelp.dll", "symsrv.dll"];

pub struct Options {
    pub yes: bool,
    pub purge: bool,
    pub project: bool,
}

/// A file and its text after the change. Empty text means: delete the file.
struct FileEdit {
    file: PathBuf,
    updated: String,
}

enum PathEdit {
    Profile(FileEdit),
    UserPath(String),
}

impl PathEdit {
    fn label(&self) -> String {
        match self {
            PathEdit::Profile(edit) => edit.file.display().to_string(),
            PathEdit::UserPath(_) => "user Path".to_string(),
        }
    }
}

pub fn run(options: Options) -> Result<Value, Value> {
    if options.project {
        return remove_project_guide(options.yes);
    }
    let exe = std::env::current_exe().map_err(|e| failure("no_install_path", e, ""))?;
    let exe = PathBuf::from(printable_path(&exe));
    let dir = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    let dev_build = is_dev_build(&exe);
    let owned = !dev_build && is_owned_dir(&dir);

    let guides = guide_edits()?;
    let mut left = Vec::new();
    note_project_guide(&mut left);
    let (data, holds_binary): (Vec<_>, Vec<_>) = purge_targets(owned.then_some(dir.as_path()))
        .into_iter()
        .partition(|target| !exe.starts_with(printable_path(target)));
    for item in &holds_binary {
        note(
            &mut left,
            item.display(),
            "It holds this binary, so it stays.",
        );
    }
    if !options.purge {
        for item in &data {
            note(&mut left, item.display(), "User data. --purge deletes it.");
        }
    }
    let plan_path = |left: &mut Vec<Value>| {
        if dev_build {
            Vec::new()
        } else {
            path_edits(&dir, &exe, owned, left)
        }
    };
    let path_changes = plan_path(&mut left);
    if dev_build {
        note(&mut left, exe.display(), "This is a cargo build. It stays.");
    } else if owned && holds_other_files(&dir, &exe, true) {
        note(&mut left, dir.display(), "The folder holds other files.");
    }

    let daemons = list_running_instance_ids();
    let mut report = json!({
        "status": "planned",
        "daemons": daemons,
        "guides": file_labels(&guides),
        "path": path_labels(&path_changes),
        "binary": (!dev_build).then(|| exe.display().to_string()),
        "data": if options.purge { display_all(&data) } else { Vec::new() },
        "other_copies": other_copies(&exe),
    });
    if !options.yes {
        report["left"] = json!(left);
        report["hint"] = json!("Nothing changed. Add --yes to apply this report.");
        return Ok(report);
    }

    let running: Vec<(u32, String)> = daemons
        .iter()
        .filter_map(|id| match daemon_process(id) {
            DaemonProcess::Running { pid, identity } => Some((pid, identity)),
            _ => None,
        })
        .collect();
    for id in &daemons {
        stop_or_kill(id);
    }
    let alive = still_running(running);
    if !alive.is_empty() {
        let hints: Vec<String> = alive.iter().map(|pid| force_end_hint(*pid)).collect();
        return Err(json!({
            "status": "failed",
            "error": "daemons_running",
            "pids": alive,
            "message": "These daemons did not stop. No file was changed.",
            "hint": hints.join(" "),
        }));
    }
    for id in &daemons {
        if let DaemonProcess::Unverified(pid) = daemon_process(id) {
            let reason = format!(
                "Not stopped: its pid file has no process identity. {}",
                unverified_hint(pid)
            );
            note(&mut left, format!("daemon {id}"), reason);
        }
    }

    // A stop can take two minutes. Read the files again, so that edits from that
    // time stay.
    let guides = guide_edits()?;
    let mut notes = Vec::new();
    let path_changes = plan_path(&mut notes);
    for entry in notes {
        if !left.contains(&entry) {
            left.push(entry);
        }
    }
    report["guides"] = json!(file_labels(&guides));
    report["path"] = json!(path_labels(&path_changes));

    let mut failed = Vec::new();
    for edit in &guides {
        apply_file_edit(edit, &mut failed);
        // `install` made this folder if the guide was its only file. `remove_dir`
        // leaves a folder that holds anything else.
        if edit.updated.is_empty() {
            if let Some(folder) = edit.file.parent() {
                let _ = std::fs::remove_dir(folder);
            }
        }
    }
    if options.purge {
        for item in &data {
            if let Err(e) = delete(item) {
                note(&mut failed, item.display(), e);
            }
        }
    }
    for edit in path_changes {
        let label = edit.label();
        match edit {
            PathEdit::Profile(edit) => apply_file_edit(&edit, &mut failed),
            PathEdit::UserPath(updated) => {
                if let Err(e) = write_user_path(&updated) {
                    note(&mut failed, label, e);
                }
            }
        }
    }
    if !dev_build && failed.is_empty() {
        if let Err(e) = remove_binary(&exe, owned.then_some(dir.as_path())) {
            let reason = format!("{e}. {}", delete_by_hand(&exe));
            note(&mut failed, exe.display(), reason);
        }
    } else if !dev_build {
        note(
            &mut left,
            exe.display(),
            "Kept after the failure above. Run `dalzzack uninstall --yes` again.",
        );
    }

    let complete = failed.is_empty();
    report["status"] = json!(if complete { "completed" } else { "partial" });
    report["left"] = json!(left);
    report["failed"] = json!(failed);
    if complete {
        Ok(report)
    } else {
        Err(report)
    }
}

/// The port files cannot tell: `stop` and `kill` delete them for a daemon that
/// ended. A daemon that survives would hold the binary open. `stop` answers
/// before its process ends, so a live process gets a moment.
fn still_running(running: Vec<(u32, String)>) -> Vec<u32> {
    let mut alive = running;
    for _ in 0..12 {
        alive.retain(|(pid, identity)| !process_ended(*pid, identity));
        if alive.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    alive.into_iter().map(|(pid, _)| pid).collect()
}

fn note(list: &mut Vec<Value>, item: impl Display, reason: impl Display) {
    list.push(json!({ "item": item.to_string(), "reason": reason.to_string() }));
}

fn file_labels(edits: &[FileEdit]) -> Vec<String> {
    edits
        .iter()
        .map(|edit| edit.file.display().to_string())
        .collect()
}

fn path_labels(edits: &[PathEdit]) -> Vec<String> {
    edits.iter().map(PathEdit::label).collect()
}

fn display_all(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.display().to_string()).collect()
}

fn delete_by_hand(exe: &Path) -> String {
    if cfg!(windows) {
        format!(
            "From an administrator terminal, run: del \"{}\"",
            exe.display()
        )
    } else {
        format!("Run: sudo rm \"{}\"", exe.display())
    }
}

/// `--project` undoes `install codex-project` in this folder and nothing else,
/// so it can run in each project before the binary goes.
fn remove_project_guide(yes: bool) -> Result<Value, Value> {
    let edit = guide_edit(PathBuf::from("AGENTS.md"))?;
    let mut report = json!({
        "status": "planned",
        "guides": edit.iter().map(|edit| printable_path(&edit.file)).collect::<Vec<_>>(),
    });
    if !yes {
        report["hint"] = json!("Nothing changed. Add --yes to apply this report.");
        return Ok(report);
    }
    let mut failed = Vec::new();
    if let Some(edit) = &edit {
        apply_file_edit(edit, &mut failed);
    }
    if !failed.is_empty() {
        report["status"] = json!("partial");
        report["failed"] = json!(failed);
        return Err(report);
    }
    report["status"] = json!("completed");
    Ok(report)
}

/// Read every global guide before any change: one file with broken markers stops
/// the run while there is still nothing to undo.
fn guide_edits() -> Result<Vec<FileEdit>, Value> {
    let mut edits = Vec::new();
    for file in global_files() {
        edits.extend(guide_edit(file)?);
    }
    Ok(edits)
}

fn guide_edit(file: PathBuf) -> Result<Option<FileEdit>, Value> {
    let hint = "No file was changed. Fix the file, then run this again.";
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            let message = format!("{}: {e}", file.display());
            return Err(failure("guide_not_readable", message, hint));
        }
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) if has_marker(e.as_bytes()) => {
            let message = format!("{}: it has a guide but it is not UTF-8", file.display());
            return Err(failure("guide_not_readable", message, hint));
        }
        // `install` refuses such a file. A guide gets into one only when the
        // file is saved in another encoding later, and the check above sees that.
        Err(_) => return Ok(None),
    };
    match without_docs(&text) {
        Ok(updated) => Ok(updated.map(|updated| FileEdit { file, updated })),
        Err(message) => {
            let message = format!("{}: {message}", file.display());
            Err(failure("guide_markers_broken", message, hint))
        }
    }
}

/// In ASCII-based encodings, or in the UTF-16 of Windows PowerShell's `>`.
fn has_marker(bytes: &[u8]) -> bool {
    let holds = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
    [BLOCK_BEGIN, BLOCK_END].iter().any(|marker| {
        let utf16: Vec<u8> = marker.encode_utf16().flat_map(u16::to_le_bytes).collect();
        holds(marker.as_bytes()) || holds(&utf16)
    })
}

fn note_project_guide(left: &mut Vec<Value>) {
    let here = PathBuf::from("AGENTS.md");
    let is_global = global_files().iter().any(|file| same_path(file, &here));
    let has_block = std::fs::read_to_string(&here).is_ok_and(|text| text.contains(BLOCK_BEGIN));
    if has_block && !is_global {
        note(
            left,
            printable_path(&here),
            "A project file. `dalzzack uninstall --project --yes` removes the guide.",
        );
    }
}

/// Written in place, as `install` does: a rename would turn a symlinked dotfile
/// into a plain copy.
fn apply_file_edit(edit: &FileEdit, failed: &mut Vec<Value>) {
    let result = if edit.updated.is_empty() {
        std::fs::remove_file(&edit.file)
    } else {
        std::fs::write(&edit.file, &edit.updated)
    };
    if let Err(e) = result {
        note(failed, edit.file.display(), e);
    }
}

/// A folder that only dalzzack uses, such as `Programs\dalzzack`.
fn is_owned_dir(dir: &Path) -> bool {
    dir.file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("dalzzack"))
}

/// The last check before a delete: the path names dalzzack, or it is in a
/// folder of that name.
fn may_delete(path: &Path) -> bool {
    let names_dalzzack = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().to_lowercase().contains("dalzzack"));
    names_dalzzack || path.parent().is_some_and(is_owned_dir)
}

/// What `--purge` deletes, as far as it exists. `owned_dir` adds what 1.0 wrote
/// next to the binary.
fn purge_targets(owned_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut targets = vec![data_dir()];
    if let Some(local) = dirs::data_local_dir() {
        targets.push(local.join(BUNDLE_ID));
    }
    if cfg!(target_os = "macos") {
        let library = home_dir().join("Library");
        for folder in ["WebKit", "Caches", "HTTPStorages"] {
            for name in ["dalzzack", BUNDLE_ID] {
                targets.push(library.join(folder).join(name));
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Profiles, dumps, and the symbol cache. Port and pid files go with
            // their daemons.
            let ours = name == "dalzzack" || name.starts_with("dalzzack-");
            if ours && !name.ends_with(".port") && !name.ends_with(".pid") {
                targets.push(entry.path());
            }
        }
    }
    if let Some(dir) = owned_dir {
        targets.push(dir.join("logs"));
        targets.push(dir.join("sites"));
    }
    targets.retain(|target| target.exists() && may_delete(target));
    targets
}

/// A browser process can keep its files open for a moment after its daemon stops.
fn delete(path: &Path) -> std::io::Result<()> {
    let mut result = Ok(());
    for attempt in 0..6 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        result = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match &result {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {}
        }
    }
    result
}

/// True when `dir` holds something that this uninstall does not remove.
fn holds_other_files(dir: &Path, exe: &Path, owned: bool) -> bool {
    let mut ours: Vec<OsString> = Vec::new();
    if let Some(name) = exe.file_name() {
        let mut aside = name.to_os_string();
        aside.push(".old");
        ours.extend([name.to_os_string(), aside]);
    }
    if owned {
        ours.extend(DEBUG_DLLS.map(OsString::from));
        ours.extend(["logs", "sites"].map(OsString::from));
    }
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| !ours.contains(&entry.file_name()))
    })
}

/// Other programs in a shared folder, such as `~/.local/bin`, need the PATH
/// entry. An extra entry does no harm, so the entry stays and the report says so.
fn path_edits(dir: &Path, exe: &Path, owned: bool, left: &mut Vec<Value>) -> Vec<PathEdit> {
    let edits: Vec<PathEdit> = if cfg!(windows) {
        user_path_edit(dir, left).into_iter().collect()
    } else {
        profile_edits(dir, left)
    };
    if !owned && !edits.is_empty() && holds_other_files(dir, exe, false) {
        for edit in &edits {
            note(
                left,
                edit.label(),
                format!(
                    "{} holds other programs, so its PATH entry stays.",
                    dir.display()
                ),
            );
        }
        return Vec::new();
    }
    edits
}

fn profile_files() -> Vec<PathBuf> {
    let home = home_dir();
    let zsh = std::env::var_os("ZDOTDIR")
        .filter(|dir| !dir.is_empty())
        .map_or_else(|| home.clone(), PathBuf::from);
    vec![
        zsh.join(".zshenv"),
        home.join(".bash_profile"),
        home.join(".profile"),
    ]
}

fn profile_edits(dir: &Path, left: &mut Vec<Value>) -> Vec<PathEdit> {
    let mut edits = Vec::new();
    for file in profile_files() {
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                note(left, file.display(), format!("Could not read it: {e}"));
                continue;
            }
        };
        let is_ours = |folder: &str| same_path(Path::new(folder), dir);
        let updated = without_installer_path(&text, is_ours);
        if updated
            .as_deref()
            .unwrap_or(&text)
            .contains(INSTALLER_COMMENT)
        {
            note(
                left,
                file.display(),
                "It has an installer line for another folder.",
            );
        }
        if let Some(updated) = updated {
            edits.push(PathEdit::Profile(FileEdit { file, updated }));
        }
    }
    edits
}

/// `text` without the lines that `install.sh` added for a folder that `is_ours`
/// accepts. The script appends `\n<comment>\n<export>\n`, so taking those bytes
/// out restores the file.
fn without_installer_path(text: &str, is_ours: impl Fn(&str) -> bool) -> Option<String> {
    let comment = format!("\n{INSTALLER_COMMENT}\n");
    text.match_indices(&comment).find_map(|(at, _)| {
        let rest = &text[at + comment.len()..];
        let line = rest.split('\n').next().unwrap_or_default();
        let folder = line
            .strip_prefix("export PATH=\"")?
            .strip_suffix(":$PATH\"")?;
        if !is_ours(folder) {
            return None;
        }
        let end = at + comment.len() + (line.len() + 1).min(rest.len());
        // The first newline is the script's own, unless it ends the line above
        // and text follows: taking it out then joins those two lines.
        let joins = at > 0 && text.as_bytes()[at - 1] != b'\n' && end < text.len();
        let keep = if joins { at + 1 } else { at };
        Some(format!("{}{}", &text[..keep], &text[end..]))
    })
}

/// True when a `Path` entry names `dir`, also through a junction or a short name.
fn same_folder(entry: &str, dir: &Path) -> bool {
    let bare = |path: &str| path.trim_end_matches(['\\', '/']).to_lowercase();
    let entry_path = Path::new(entry);
    bare(entry) == bare(&dir.to_string_lossy())
        || (entry_path.is_absolute() && same_path(entry_path, dir))
}

/// `raw` without the entries that `is_ours` accepts. Other entries keep their
/// text, so a `%VAR%` entry stays a `%VAR%` entry.
fn path_without_entry(raw: &str, is_ours: impl Fn(&str) -> bool) -> Option<String> {
    let entries: Vec<&str> = raw.split(';').collect();
    let kept: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|entry| !is_ours(entry))
        .collect();
    (kept.len() != entries.len()).then(|| kept.join(";"))
}

fn user_path_edit(dir: &Path, left: &mut Vec<Value>) -> Option<PathEdit> {
    match read_user_path() {
        Ok(raw) => {
            path_without_entry(&raw, |entry| same_folder(entry, dir)).map(PathEdit::UserPath)
        }
        Err(e) => {
            note(left, "user Path", format!("Could not read it: {e}"));
            None
        }
    }
}

fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn system32() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32")
}

/// The registry calls of `install.ps1`, so both sides read and write the value
/// the same way. Keep the script free of double quotes: `-Command` takes it as
/// one argument.
fn powershell(script: &str, value: Option<&str>) -> Result<String, String> {
    let exe = system32()
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let mut command = Command::new(exe);
    command
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("$ErrorActionPreference='Stop'; {script}"))
        .stdin(Stdio::null());
    if let Some(value) = value {
        command.env("DZ_VALUE", value);
    }
    hide_window(&mut command);
    let output = command.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The raw value, with `%VAR%` entries not expanded. It comes back as base64,
/// because a console code page would damage a path that is not ASCII.
fn read_user_path() -> Result<String, String> {
    let encoded = powershell(
        "$k=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment'); \
         $v=$k.GetValue('Path','',[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames); \
         $k.Close(); [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($v))",
        None,
    )?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// The second half tells running programs: a registry write alone does not.
fn write_user_path(value: &str) -> Result<(), String> {
    powershell(
        "$k=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment',$true); \
         $k.SetValue('Path',$env:DZ_VALUE,[Microsoft.Win32.RegistryValueKind]::ExpandString); \
         $k.Close(); \
         [Environment]::SetEnvironmentVariable('DALZZACK_PATH_REFRESH','1','User'); \
         [Environment]::SetEnvironmentVariable('DALZZACK_PATH_REFRESH',[NullString]::Value,'User')",
        Some(value),
    )
    .map(|_| ())
}

/// Other `dalzzack` binaries on `PATH`: this run removes only itself.
fn other_copies(exe: &Path) -> Vec<String> {
    let (Some(name), Some(path)) = (exe.file_name(), std::env::var_os("PATH")) else {
        return Vec::new();
    };
    let mut copies: Vec<String> = Vec::new();
    for dir in std::env::split_paths(&path) {
        let copy = dir.join(name);
        let text = copy.display().to_string();
        if copy.is_file() && !same_path(&copy, exe) && !copies.contains(&text) {
            copies.push(text);
        }
    }
    copies
}

#[cfg(not(windows))]
fn remove_binary(exe: &Path, owned_dir: Option<&Path>) -> std::io::Result<()> {
    std::fs::remove_file(exe)?;
    if let Some(dir) = owned_dir {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// Windows cannot delete a running exe, but it can rename one. A detached
/// `cmd.exe` deletes the renamed file after this process ends. `rmdir` removes
/// only an empty folder. Paths go through the environment, so no path is parsed
/// as a command.
#[cfg(windows)]
fn remove_binary(exe: &Path, owned_dir: Option<&Path>) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    let aside = crate::upgrade::moved_aside_path(exe);
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(exe, &aside)?;

    let mut script =
        String::from(r#"/d /c ""%DZ_PING%" -n 3 127.0.0.1 >nul & del /f /q "%DZ_OLD%""#);
    if owned_dir.is_some() {
        for dll in DEBUG_DLLS {
            script.push_str(&format!(r#" "%DZ_DIR%\{dll}""#));
        }
        script.push_str(r#" & rmdir "%DZ_DIR%""#);
    }
    script.push('"');

    let mut command = Command::new(system32().join("cmd.exe"));
    command
        .raw_arg(script)
        .env("DZ_PING", system32().join("PING.EXE"))
        .env("DZ_OLD", &aside)
        .env("DZ_DIR", owned_dir.unwrap_or(Path::new("")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_window(&mut command);
    command.spawn().map(|_| ()).inspect_err(|_| {
        let _ = std::fs::rename(&aside, exe);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = "export PATH=\"/Users/a/.local/bin:$PATH\"";

    fn installed(original: &str) -> String {
        format!("{original}\n{INSTALLER_COMMENT}\n{EXPORT}\n")
    }

    fn is_local_bin(folder: &str) -> bool {
        folder == "/Users/a/.local/bin"
    }

    #[test]
    fn the_installer_lines_come_out_and_the_file_is_restored() {
        for original in ["", "alias a=b\n", "alias a=b"] {
            let text = installed(original);
            assert_eq!(
                without_installer_path(&text, is_local_bin).as_deref(),
                Some(original)
            );
        }
    }

    #[test]
    fn lines_added_after_the_installer_stay() {
        let text = format!("{}alias c=d\n", installed("alias a=b\n"));
        assert_eq!(
            without_installer_path(&text, is_local_bin).as_deref(),
            Some("alias a=b\nalias c=d\n")
        );
    }

    #[test]
    fn the_lines_around_the_installer_lines_stay_apart() {
        let text = format!("export FOO=1\n{INSTALLER_COMMENT}\n{EXPORT}\nexport BAR=2\n");
        assert_eq!(
            without_installer_path(&text, is_local_bin).as_deref(),
            Some("export FOO=1\nexport BAR=2\n")
        );
    }

    #[test]
    fn only_the_lines_of_the_installer_for_this_folder_match() {
        let other = installed("").replace("/Users/a/.local/bin", "/opt/tools");
        assert_eq!(without_installer_path(&other, is_local_bin), None);

        let no_export = format!("\n{INSTALLER_COMMENT}\necho hello\n");
        assert_eq!(without_installer_path(&no_export, is_local_bin), None);

        let no_comment = format!("alias a=b\n{EXPORT}\n");
        assert_eq!(without_installer_path(&no_comment, is_local_bin), None);
    }

    #[test]
    fn the_right_block_goes_when_two_folders_were_installed() {
        let other = installed("").replace("/Users/a/.local/bin", "/opt/tools");
        let text = format!("{other}{}", installed(""));
        assert_eq!(
            without_installer_path(&text, is_local_bin).as_deref(),
            Some(other.as_str())
        );
    }

    fn without(raw: &str, dir: &str) -> Option<String> {
        path_without_entry(raw, |entry| same_folder(entry, Path::new(dir)))
    }

    #[test]
    fn a_path_entry_matches_without_regard_to_case_or_a_trailing_slash() {
        let dir = r"C:\Users\a\AppData\Local\Programs\dalzzack";
        let raw = r"C:\One;c:\users\a\appdata\local\programs\DALZZACK\;C:\Two";
        assert_eq!(without(raw, dir).as_deref(), Some(r"C:\One;C:\Two"));
    }

    #[test]
    fn the_other_path_entries_keep_their_text() {
        let raw = r"%USERPROFILE%\bin;;C:\Tools\;C:\dalzzack";
        assert_eq!(
            without(raw, r"C:\dalzzack").as_deref(),
            Some(r"%USERPROFILE%\bin;;C:\Tools\")
        );
    }

    #[test]
    fn a_path_without_the_entry_needs_no_change() {
        assert_eq!(without(r"C:\One;C:\Two", r"C:\dalzzack"), None);
        assert_eq!(without("", r"C:\dalzzack"), None);
        assert_eq!(without(r"C:\dalzzack-tools", r"C:\dalzzack"), None);
    }

    #[test]
    fn a_relative_path_entry_is_not_this_folder() {
        let here = std::env::current_dir().unwrap();
        assert!(!same_folder(".", &here));
    }

    #[test]
    fn a_path_entry_can_reach_the_folder_another_way() {
        let here = std::env::current_dir().unwrap();
        let other = here.join(".");
        assert!(same_folder(&other.display().to_string(), &here));
    }

    #[test]
    fn a_marker_shows_in_a_file_that_is_not_utf8() {
        let mut bytes = vec![0xC7, 0xD1];
        assert!(!has_marker(&bytes));
        bytes.extend_from_slice(BLOCK_BEGIN.as_bytes());
        assert!(has_marker(&bytes));

        let utf16: Vec<u8> = BLOCK_END
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(has_marker(&utf16));
    }

    #[test]
    fn only_a_folder_named_dalzzack_belongs_to_dalzzack() {
        assert!(is_owned_dir(Path::new("/Users/a/Programs/dalzzack")));
        assert!(is_owned_dir(Path::new("/Users/a/Programs/DALZZACK")));
        assert!(!is_owned_dir(Path::new("/Windows/System32")));
        assert!(!is_owned_dir(Path::new("/Users/a/.local/bin")));
        assert!(!is_owned_dir(Path::new("/Users/a/dalzzack-tools")));
    }

    #[test]
    fn a_delete_needs_a_path_that_names_dalzzack() {
        assert!(may_delete(Path::new("/tmp/dalzzack-webview-a")));
        assert!(may_delete(Path::new(
            "/Users/a/Library/Caches/org.gosuda.dalzzack"
        )));
        assert!(may_delete(Path::new("/Users/a/Programs/dalzzack/logs")));
        for path in [
            "/",
            "/Users/a",
            "/Users/a/Library/Caches",
            "/Users/a/.local/bin/logs",
            "/Windows/System32/logs",
        ] {
            assert!(!may_delete(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn other_files_in_the_folder_are_noticed() {
        let dir = std::env::temp_dir().join(format!("dalzzack-uninstall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("dalzzack");
        for name in ["dalzzack", "dalzzack.old", "dbghelp.dll", "logs"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        assert!(!holds_other_files(&dir, &exe, true));
        assert!(holds_other_files(&dir, &exe, false));

        std::fs::write(dir.join("uv"), b"").unwrap();
        assert!(holds_other_files(&dir, &exe, true));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
