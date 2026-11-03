//! Minidump reading and native symbolization (Windows).
//!
//! `pause` writes a minidump that works on a fully wedged renderer, but a path
//! to a `.dmp` is not an answer — it's homework. Turning it into
//! `v8::internal::Factory::AllocateRaw+0xf1` previously meant hand-writing a
//! minidump parser, a thread filter, a module-path extractor and a dbghelp
//! P/Invoke wrapper. That work lives here instead.
//!
//! Two things this module refuses to get wrong, both learned from doing it the
//! hard way:
//!
//! 1. **Waiting threads are excluded.** Most threads sit in `ntdll` waiting for
//!    work. Counting them produces a histogram whose top entry is "the process
//!    was idle", which says nothing. They're reported as a count, not as
//!    samples.
//! 2. **Module paths come from the dump.** WebView2 auto-updates, so a path
//!    cached from a previous run (`…\150.0.4078.105\msedge.dll`) silently stops
//!    existing and symbol loading fails wholesale. The dump records the paths
//!    that were actually mapped.
//!
//! Structures are read through dbghelp's own `MiniDumpReadDumpStream` and the
//! `windows` crate's struct definitions rather than hand-rolled offsets — the
//! offsets are exactly where this goes quietly wrong (a mis-read RVA yields
//! `Rip = 0` with no error at all).

use serde_json::{json, Value};
use std::collections::HashMap;

/// x86-64 `CONTEXT.Rip` offset. The `CONTEXT` layout is architecture-specific
/// and this module only claims to read AMD64 dumps.
const CONTEXT_AMD64_RIP_OFFSET: usize = 0xF8;

/// dbghelp is explicitly single-threaded: every call for a given process handle
/// has to be serialized.
static DBGHELP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `(which dbghelp.dll got loaded, whether symsrv.dll came with it)`.
static DBGHELP_SOURCE: std::sync::OnceLock<(&'static str, bool)> = std::sync::OnceLock::new();

/// Module-load failures from the most recent [`symbolize`] call, surfaced in the
/// response so "no symbols" always comes with a reason.
static LOAD_ERRORS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn take_load_errors() -> Vec<String> {
    LOAD_ERRORS
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

/// dbghelp's own running commentary, captured via `SymRegisterCallbackW64`.
///
/// symsrv reports why it did or didn't fetch a PDB *only* through this channel.
/// Without it a failed download is completely silent — no error, no file, no
/// symbol — which is how "downloading is set up correctly and still does
/// nothing" became unanswerable.
static DBGHELP_LOG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn take_dbghelp_log() -> Vec<String> {
    DBGHELP_LOG
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

fn push_dbghelp_log(s: String) {
    if let Ok(mut v) = DBGHELP_LOG.lock() {
        // Bounded: SYMOPT_DEBUG is chatty and a stuck loop shouldn't eat memory.
        if v.len() < 400 {
            v.push(s);
        }
    }
}

/// Read a NUL-terminated UTF-16 string from dbghelp callback data.
unsafe fn wide_from_ptr(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while len < 4096 && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// `SymRegisterCallbackW64` handler: turns dbghelp/symsrv internals into text.
unsafe extern "system" fn sym_callback(
    _process: windows::Win32::Foundation::HANDLE,
    action: u32,
    callback_data: u64,
    _user_context: u64,
) -> windows_core::BOOL {
    // Values from dbghelp.h; windows-rs doesn't export them as constants.
    const CBA_DEBUG_INFO: u32 = 0x1000_0000;
    const CBA_EVENT: u32 = 0x0000_0010;
    const CBA_DEFERRED_SYMBOL_LOAD_FAILURE: u32 = 0x0000_0004;

    match action {
        CBA_DEBUG_INFO => {
            let msg = unsafe { wide_from_ptr(callback_data as *const u16) };
            let msg = msg.trim().to_string();
            if !msg.is_empty() {
                push_dbghelp_log(msg);
            }
        }
        CBA_EVENT => {
            // IMAGEHLP_CBA_EVENTW: severity u32, code u32, desc PWSTR at +8
            // (pointer-aligned), object PVOID.
            let base = callback_data as *const u8;
            if !base.is_null() {
                let desc_ptr = unsafe { *(base.add(8) as *const *const u16) };
                let desc = unsafe { wide_from_ptr(desc_ptr) };
                if !desc.trim().is_empty() {
                    push_dbghelp_log(format!("event: {}", desc.trim()));
                }
            }
        }
        CBA_DEFERRED_SYMBOL_LOAD_FAILURE => {
            push_dbghelp_log("deferred symbol load FAILED".to_string());
        }
        _ => {}
    }
    // FALSE = "not handled", which keeps dbghelp's default behaviour.
    windows_core::BOOL(0)
}

/// Load the Debugging Tools for Windows copy of `dbghelp.dll` if it exists.
///
/// **The system copy in System32 cannot download symbols.** Symbol-server
/// support lives in `symsrv.dll`, which ships next to the Windows Kits
/// `dbghelp.dll` and is absent from System32 entirely — so with the system copy
/// every `srv*…*https://msdl…` path silently resolves nothing and every symbol
/// comes back null, which looks identical to "this frame has no symbol".
///
/// Full path of the `dbghelp.dll` this process is actually running.
///
/// The loader matches DLLs by base name, so `LoadLibraryEx` on the Kits copy
/// returns whatever `dbghelp.dll` is already resident — asking it to load a
/// file is not the same as getting that file. This is the only honest check.
fn loaded_dbghelp_path() -> Option<String> {
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
    use windows_core::HSTRING;

    unsafe {
        let h = GetModuleHandleW(&HSTRING::from("dbghelp.dll")).ok()?;
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(Some(h), &mut buf);
        if n == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

fn ensure_dbghelp() -> (&'static str, bool) {
    *DBGHELP_SOURCE.get_or_init(|| {
        use windows::Win32::System::LibraryLoader::{
            LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH,
        };
        use windows_core::HSTRING;

        // Best effort: if `dbghelp.dll` is not already bound (rare, but it
        // happens when nothing has touched it yet) this gets the Kits copy in
        // first. When it IS already bound these calls are no-ops — which is the
        // whole reason the check below exists.
        if let Some(dir) = kits_debuggers_dir() {
            for name in ["symsrv.dll", "dbghelp.dll"] {
                let path = dir.join(name);
                if path.exists() {
                    let _ = unsafe {
                        LoadLibraryExW(
                            &HSTRING::from(path.to_string_lossy().as_ref()),
                            None,
                            LOAD_WITH_ALTERED_SEARCH_PATH,
                        )
                    };
                }
            }
        }

        // What matters is which `dbghelp.dll` the process actually ended up
        // with, not which one we asked for: the loader keys modules by base
        // name, and our own static imports bind `dbghelp.dll` from System32
        // before any of this code runs. Believing `LoadLibraryEx` here is how
        // this once reported "windows_kits" while running the System32 copy,
        // which has no symsrv and silently downloads nothing.
        let actual = loaded_dbghelp_path();
        let is_system = actual
            .as_deref()
            .map(|p| p.to_lowercase().contains("\\system32\\"))
            .unwrap_or(true);
        let symsrv_present = module_loaded("symsrv.dll");

        if is_system {
            tracing::warn!(
                "[symbolize] running the System32 dbghelp ({}) — it has no symsrv.dll, so \
                 symbol-server downloads are impossible in this process",
                actual.as_deref().unwrap_or("<unknown>")
            );
            ("system", false)
        } else {
            tracing::info!(
                "[symbolize] using dbghelp at {} (symsrv loaded: {})",
                actual.as_deref().unwrap_or("<unknown>"),
                symsrv_present
            );
            ("external", symsrv_present)
        }
    })
}

/// Is a module with this base name resident in the process?
fn module_loaded(name: &str) -> bool {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_core::HSTRING;
    unsafe { GetModuleHandleW(&HSTRING::from(name)).is_ok() }
}

/// Put the Debugging Tools `dbghelp.dll` + `symsrv.dll` next to our executable.
///
/// This is the only arrangement that actually enables symbol downloads: the
/// application directory outranks System32 in the loader's search order, so the
/// static import binds to these copies at process start. It cannot take effect
/// in the process that performs the copy — `dbghelp.dll` is bound long before
/// `main` — hence the "re-run" wording wherever this is reported.
///
/// Returns the message to show, or `None` if there was nothing to do.
pub fn provision_symbol_dlls() -> Option<String> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    if exe_dir.join("dbghelp.dll").exists() {
        return None;
    }
    let src_dir = kits_debuggers_dir()?;

    let mut copied = Vec::new();
    for name in ["dbghelp.dll", "symsrv.dll"] {
        let src = src_dir.join(name);
        let dst = exe_dir.join(name);
        if src.exists() && !dst.exists() && std::fs::copy(&src, &dst).is_ok() {
            copied.push(name);
        }
    }
    if copied.is_empty() {
        return None;
    }
    Some(format!(
        "Installed {} from {} next to the executable. Symbol downloads need the loader to bind \
         these at process start, so RE-RUN this command to use them.",
        copied.join(" + "),
        src_dir.display()
    ))
}

/// Are the symbol DLLs already sitting next to the executable?
fn symbol_dlls_installed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("dbghelp.dll")))
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// The Windows Kits debuggers directory, if one is installed.
fn kits_debuggers_dir() -> Option<std::path::PathBuf> {
    for root in [
        std::env::var("ProgramFiles(x86)").ok(),
        std::env::var("ProgramFiles").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let dir = std::path::Path::new(&root)
            .join("Windows Kits")
            .join("10")
            .join("Debuggers")
            .join("x64");
        if dir.join("dbghelp.dll").exists() {
            return Some(dir);
        }
    }
    None
}

/// Can this dbghelp reach a symbol server at all? Needs a non-System32 dbghelp
/// (the System32 one ships without symbol-server support) and a resident
/// `symsrv.dll`.
pub fn symbol_download_supported() -> bool {
    let (source, symsrv) = ensure_dbghelp();
    source == "external" && symsrv
}

pub fn dbghelp_source() -> &'static str {
    ensure_dbghelp().0
}

pub fn symsrv_loaded() -> bool {
    ensure_dbghelp().1
}

#[derive(Debug, Clone)]
pub struct ModuleInfo {
    pub base: u64,
    pub size: u32,
    /// Full path as recorded in the dump.
    pub path: String,
}

impl ModuleInfo {
    pub fn name(&self) -> &str {
        self.path.rsplit(['\\', '/']).next().unwrap_or(&self.path)
    }

    fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.base + self.size as u64
    }
}

#[derive(Debug, Clone)]
pub struct ThreadSample {
    pub tid: u32,
    pub rip: u64,
}

/// Modules that are pure syscall stubs — a RIP here means "blocked in the
/// kernel", never "running code worth looking at".
fn is_wait_module(name: &str) -> bool {
    name.eq_ignore_ascii_case("ntdll.dll") || name.eq_ignore_ascii_case("win32u.dll")
}

/// Blocking waits that live outside the syscall stub modules (kernel32 and
/// friends forward to them, and a stack can be caught in the forwarder).
fn is_wait_symbol(symbol: &str) -> bool {
    const WAIT_PREFIXES: &[&str] = &[
        "WaitForSingleObject",
        "WaitForMultipleObjects",
        "SleepConditionVariable",
        "RtlSleepConditionVariable",
        "RtlWaitOn",
        "MsgWaitForMultipleObjects",
        // Unambiguous waits by name, so they hold wherever they resolve — a
        // message loop is a message loop even if the frame lands in a
        // forwarder rather than in win32u.
        "NtUserMsgWait",
        "NtUserWaitMessage",
        "NtUserGetMessage",
        "NtWaitFor",
        "ZwWaitFor",
    ];
    WAIT_PREFIXES.iter().any(|p| symbol.starts_with(p))
}

/// Syscalls that mean the thread is *working*, not parked.
///
/// This is the exception list that matters: a thread allocating hard spends a
/// large share of its samples inside the memory syscalls, and calling those
/// "waiting" hides precisely the allocation storm someone is hunting. Everything
/// else entering the kernel from a stub module is treated as a block.
fn is_work_syscall(symbol: &str) -> bool {
    const WORK_PREFIXES: &[&str] = &[
        "NtAllocateVirtualMemory",
        "ZwAllocateVirtualMemory",
        "NtFreeVirtualMemory",
        "ZwFreeVirtualMemory",
        "NtProtectVirtualMemory",
        "ZwProtectVirtualMemory",
        "NtMapViewOfSection",
        "ZwMapViewOfSection",
        "NtUnmapViewOfSection",
        "ZwUnmapViewOfSection",
        "NtFlushInstructionCache",
        "ZwFlushInstructionCache",
    ];
    WORK_PREFIXES.iter().any(|p| symbol.starts_with(p))
}

#[derive(Debug, Default)]
pub struct DumpContents {
    pub threads: Vec<ThreadSample>,
    pub modules: Vec<ModuleInfo>,
}

impl DumpContents {
    pub fn module_for(&self, addr: u64) -> Option<&ModuleInfo> {
        self.modules.iter().find(|m| m.contains(addr))
    }

    /// Is this thread parked in the kernel rather than doing work?
    ///
    /// Decided by **symbol first**, module only as a fallback. Classifying by
    /// module alone (`ntdll`/`win32u` are syscall stub layers) looks right until
    /// you meet the thread you actually care about: a thread allocating hard
    /// sits inside `NtAllocateVirtualMemory` a good fraction of the time, and a
    /// module-based filter throws it away — hiding exactly the allocation storm
    /// someone is trying to find. `NtWaitForSingleObject` is a wait;
    /// `NtAllocateVirtualMemory` is work, and both live in `ntdll`.
    ///
    /// With no symbol at all we fall back to the module, because an unnamed
    /// frame in a stub layer is more likely a wait than not — but an unmapped
    /// address (JIT code) is always kept.
    pub fn is_waiting(&self, t: &ThreadSample, symbol: Option<&str>) -> bool {
        let in_stub_module = self
            .module_for(t.rip)
            .map(|m| is_wait_module(m.name()))
            .unwrap_or(false);

        match symbol {
            Some(sym) => {
                if is_work_syscall(sym) {
                    return false;
                }
                if is_wait_symbol(sym) {
                    return true;
                }
                // Any other `Nt*`/`Zw*` entry in a stub module is a transition
                // into the kernel that isn't on the work list — sleeping, I/O,
                // message loops, worker-factory parking. Enumerating them by
                // name never converges; this rule does.
                in_stub_module && (sym.starts_with("Nt") || sym.starts_with("Zw"))
            }
            // No name: the module is all we have. Unmapped (JIT) counts as work.
            None => in_stub_module,
        }
    }
}

/// Read the thread and module lists out of a minidump file.
pub fn read_dump(path: &std::path::Path) -> Result<DumpContents, String> {
    use windows::Win32::System::Diagnostics::Debug::{
        MiniDumpReadDumpStream, ModuleListStream, ThreadListStream, MINIDUMP_DIRECTORY,
        MINIDUMP_MODULE, MINIDUMP_THREAD,
    };

    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if bytes.len() < 32 {
        return Err(format!("{} is too small to be a minidump", path.display()));
    }
    let base = bytes.as_ptr() as *mut std::ffi::c_void;

    let mut out = DumpContents::default();

    unsafe {
        // --- modules -------------------------------------------------------
        let mut dir: *mut MINIDUMP_DIRECTORY = std::ptr::null_mut();
        let mut stream: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut stream_size: u32 = 0;
        if MiniDumpReadDumpStream(
            base,
            ModuleListStream.0 as u32,
            &mut dir,
            &mut stream,
            Some(&mut stream_size),
        )
        .as_bool()
            && !stream.is_null()
        {
            // MINIDUMP_MODULE_LIST: u32 count, then packed MINIDUMP_MODULE[].
            let count = *(stream as *const u32);
            let first = (stream as *const u8).add(4) as *const MINIDUMP_MODULE;
            for i in 0..count as usize {
                let m = &*first.add(i);
                let path = read_minidump_string(&bytes, m.ModuleNameRva).unwrap_or_default();
                out.modules.push(ModuleInfo {
                    base: m.BaseOfImage,
                    size: m.SizeOfImage,
                    path,
                });
            }
        }

        // --- threads -------------------------------------------------------
        let mut dir: *mut MINIDUMP_DIRECTORY = std::ptr::null_mut();
        let mut stream: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut stream_size: u32 = 0;
        if MiniDumpReadDumpStream(
            base,
            ThreadListStream.0 as u32,
            &mut dir,
            &mut stream,
            Some(&mut stream_size),
        )
        .as_bool()
            && !stream.is_null()
        {
            let count = *(stream as *const u32);
            let first = (stream as *const u8).add(4) as *const MINIDUMP_THREAD;
            for i in 0..count as usize {
                let t = &*first.add(i);
                let ctx_rva = t.ThreadContext.Rva as usize;
                let ctx_size = t.ThreadContext.DataSize as usize;
                if ctx_size <= CONTEXT_AMD64_RIP_OFFSET
                    || ctx_rva + CONTEXT_AMD64_RIP_OFFSET + 8 > bytes.len()
                {
                    continue;
                }
                let rip = u64::from_le_bytes(
                    bytes[ctx_rva + CONTEXT_AMD64_RIP_OFFSET
                        ..ctx_rva + CONTEXT_AMD64_RIP_OFFSET + 8]
                        .try_into()
                        .map_err(|_| "short CONTEXT record")?,
                );
                out.threads.push(ThreadSample {
                    tid: t.ThreadId,
                    rip,
                });
            }
        }
    }

    if out.threads.is_empty() && out.modules.is_empty() {
        return Err(format!(
            "{} has neither a thread nor a module list — not a minidump?",
            path.display()
        ));
    }
    Ok(out)
}

/// `MINIDUMP_STRING` at `rva`: u32 byte length, then UTF-16.
fn read_minidump_string(bytes: &[u8], rva: u32) -> Option<String> {
    let at = rva as usize;
    if at + 4 > bytes.len() {
        return None;
    }
    let len_bytes = u32::from_le_bytes(bytes[at..at + 4].try_into().ok()?) as usize;
    let start = at + 4;
    let end = start.checked_add(len_bytes)?;
    if end > bytes.len() {
        return None;
    }
    // Indexed rather than `chunks_exact(2)`: newer clippy wants `as_chunks`,
    // which older stable toolchains do not have.
    let units: Vec<u16> = (0..(end - start) / 2)
        .map(|i| u16::from_le_bytes([bytes[start + 2 * i], bytes[start + 2 * i + 1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// How to resolve symbols.
pub struct SymbolOptions {
    /// Fetch PDBs from the Microsoft symbol server. Off by default: msedge.pdb
    /// is hundreds of MB and the first resolve would otherwise stall `pause`
    /// for minutes with no warning.
    pub download: bool,
    pub cache_dir: std::path::PathBuf,
}

impl SymbolOptions {
    pub fn default_cache() -> std::path::PathBuf {
        std::env::temp_dir().join("dalzzack-symcache")
    }

    fn search_path(&self) -> String {
        let cache = self.cache_dir.display();
        if self.download {
            format!("srv*{cache}*https://msdl.microsoft.com/download/symbols")
        } else {
            // Cache only — resolves anything already downloaded, and quietly
            // resolves nothing otherwise (callers fall back to module+offset).
            format!("srv*{cache}")
        }
    }
}

/// One resolved (or unresolved) address.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub module: Option<String>,
    /// `None` when no PDB was available — the caller still gets module+offset.
    pub symbol: Option<String>,
    pub displacement: u64,
    /// Offset from the module base, i.e. the RVA to feed to an external tool.
    pub rva: Option<u64>,
}

impl Resolved {
    pub fn display(&self) -> String {
        match (&self.symbol, &self.module, self.rva) {
            (Some(sym), _, _) if self.displacement > 0 => {
                format!("{sym}+0x{:x}", self.displacement)
            }
            (Some(sym), _, _) => sym.clone(),
            (None, Some(m), Some(rva)) => format!("{m}+0x{rva:x}"),
            (None, Some(m), None) => m.clone(),
            _ => "<unmapped>".to_string(),
        }
    }
}

/// Resolve addresses against the modules recorded in the dump.
///
/// Loads each module by the path the dump recorded, so a WebView2 update that
/// moved `msedge.dll` doesn't break resolution.
pub fn symbolize(
    dump: &DumpContents,
    addrs: &[u64],
    opts: &SymbolOptions,
) -> Result<Vec<Resolved>, String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Diagnostics::Debug::{
        SymCleanup, SymFromAddrW, SymGetSearchPathW, SymInitializeW, SymLoadModuleExW,
        SymRegisterCallbackW64, SymSetOptions, SYMBOL_INFOW, SYMOPT_DEBUG, SYMOPT_DEFERRED_LOADS,
        SYMOPT_UNDNAME, SYM_LOAD_FLAGS,
    };
    use windows_core::{HSTRING, PCWSTR};

    // MUST happen before the first dbghelp import binds.
    ensure_dbghelp();

    let _guard = DBGHELP_LOCK
        .lock()
        .map_err(|_| "dbghelp lock poisoned".to_string())?;
    let _ = take_load_errors();

    let _ = std::fs::create_dir_all(&opts.cache_dir);

    // symsrv refuses to contact a symbol server until its EULA is accepted, and
    // in a non-interactive process it refuses *silently*: no download, no error,
    // no symbols — indistinguishable from "this PDB doesn't exist". The
    // documented non-interactive acceptance is an empty `symsrv.yes` in the
    // downstream store, which is a directory we own.
    if opts.download {
        let marker = opts.cache_dir.join("symsrv.yes");
        if !marker.exists() {
            let _ = std::fs::write(&marker, b"");
        }
    }

    // A synthetic handle: we never invade a live process, we load modules by
    // path ourselves, so this is only an identity for dbghelp's bookkeeping.
    let handle = HANDLE(0x5457 as *mut std::ffi::c_void);
    let search = HSTRING::from(opts.search_path());

    let mut out = Vec::with_capacity(addrs.len());
    unsafe {
        // Deferred loading keeps the cache-only path fast: it never touches a
        // PDB it doesn't end up needing. But when downloading is explicitly
        // requested, deferral is what stops the fetch from ever being
        // triggered — so load eagerly and let `SymLoadModuleExW` do the work
        // (and report the error) at a predictable point.
        let mut sym_options = SYMOPT_UNDNAME;
        if !opts.download {
            sym_options |= SYMOPT_DEFERRED_LOADS;
        }
        if opts.download {
            // Makes dbghelp narrate what symsrv is doing through the callback
            // below — the only way to see why a fetch didn't happen.
            sym_options |= SYMOPT_DEBUG;
        }
        SymSetOptions(sym_options);
        SymInitializeW(handle, PCWSTR(search.as_ptr()), false)
            .map_err(|e| format!("SymInitializeW failed: {e:?}"))?;

        if opts.download {
            let _ = take_dbghelp_log();
            if let Err(e) = SymRegisterCallbackW64(handle, Some(sym_callback), 0) {
                push_dbghelp_log(format!("SymRegisterCallbackW64 failed: {e:?}"));
            }
            // Read the search path back. dbghelp silently ignoring the one we
            // passed would explain a fetch that never happens, and there is no
            // other way to tell.
            let mut buf = vec![0u16; 4096];
            if SymGetSearchPathW(handle, &mut buf).is_ok() {
                let live = wide_from_ptr(buf.as_ptr());
                push_dbghelp_log(format!("search path in effect: {live}"));
                if live != opts.search_path() {
                    push_dbghelp_log(format!("requested search path: {}", opts.search_path()));
                }
            } else {
                push_dbghelp_log("SymGetSearchPathW failed".to_string());
            }
        }

        // Only load the modules we actually need addresses from — loading all
        // of them would fetch PDBs nobody asked about.
        //
        // Failures here are recorded, not swallowed: a module that fails to load
        // resolves nothing, and without the reason that is indistinguishable
        // from "these frames genuinely have no symbols".
        let mut loaded: HashMap<u64, bool> = HashMap::new();
        for addr in addrs {
            if let Some(m) = dump.module_for(*addr) {
                loaded.entry(m.base).or_insert_with(|| {
                    let path = HSTRING::from(m.path.as_str());
                    let rc = SymLoadModuleExW(
                        handle,
                        None,
                        PCWSTR(path.as_ptr()),
                        PCWSTR::null(),
                        m.base,
                        m.size,
                        None,
                        Some(SYM_LOAD_FLAGS(0)),
                    );
                    let err = windows::Win32::Foundation::GetLastError();
                    if rc == 0 && err.0 != 0 {
                        // rc == 0 with a zero error means "already loaded".
                        let msg = format!(
                            "SymLoadModuleExW({}) failed: win32 error {} ({:#x})",
                            m.path, err.0, err.0
                        );
                        tracing::warn!("[symbolize] {}", msg);
                        LOAD_ERRORS.lock().map(|mut v| v.push(msg)).ok();
                        return false;
                    }
                    // A module loaded at a base other than the one requested
                    // resolves nothing for our addresses, and says so nowhere.
                    if rc != 0 && rc != m.base {
                        let msg = format!(
                            "SymLoadModuleExW({}) loaded at {:#x} but the dump has it at {:#x}",
                            m.path, rc, m.base
                        );
                        tracing::warn!("[symbolize] {}", msg);
                        LOAD_ERRORS.lock().map(|mut v| v.push(msg)).ok();
                    }
                    tracing::debug!(
                        "[symbolize] loaded {} at {:#x} (size {}), rc={:#x}",
                        m.path,
                        m.base,
                        m.size,
                        rc
                    );
                    true
                });
            }
        }

        for addr in addrs {
            let module = dump.module_for(*addr);
            let rva = module.map(|m| addr - m.base);

            // SYMBOL_INFOW is variable-length: the name follows the struct, so
            // the buffer has to be over-allocated and MaxNameLen set. Getting
            // this wrong is the classic failure where SymFromAddr "succeeds"
            // but every name comes back empty.
            const MAX_SYM_NAME: usize = 2000;
            let mut buf = vec![0u8; std::mem::size_of::<SYMBOL_INFOW>() + MAX_SYM_NAME * 2];
            let info = buf.as_mut_ptr() as *mut SYMBOL_INFOW;
            (*info).SizeOfStruct = std::mem::size_of::<SYMBOL_INFOW>() as u32;
            (*info).MaxNameLen = MAX_SYM_NAME as u32;

            let mut disp: u64 = 0;
            let symbol = if SymFromAddrW(handle, *addr, Some(&mut disp), info).is_ok() {
                let name_ptr = std::ptr::addr_of!((*info).Name) as *const u16;
                let len = (*info).NameLen as usize;
                let name = if len == 0 {
                    String::new()
                } else {
                    let slice = std::slice::from_raw_parts(name_ptr, len.min(MAX_SYM_NAME));
                    String::from_utf16_lossy(slice)
                };
                if name.is_empty() {
                    None
                } else {
                    Some(name)
                }
            } else {
                let err = windows::Win32::Foundation::GetLastError();
                // ERROR_MOD_NOT_FOUND (126) / ERROR_INVALID_ADDRESS mean the
                // module never loaded; anything else usually means no PDB.
                LOAD_ERRORS
                    .lock()
                    .map(|mut v| {
                        let msg = format!(
                            "SymFromAddrW({:#x} in {}) failed: win32 error {}",
                            addr,
                            module.map(|m| m.name()).unwrap_or("<unmapped>"),
                            err.0
                        );
                        if !v.contains(&msg) {
                            v.push(msg);
                        }
                    })
                    .ok();
                disp = 0;
                None
            };

            out.push(Resolved {
                module: module.map(|m| m.name().to_string()),
                symbol,
                displacement: disp,
                rva,
            });
        }

        let _ = SymCleanup(handle);
    }

    Ok(out)
}

/// Symbolize one dump and shape it for the `pause` response.
pub fn analyze_dump(
    path: &std::path::Path,
    include_all_threads: bool,
    opts: &SymbolOptions,
) -> Result<Value, String> {
    let dump = read_dump(path)?;

    // Symbolize EVERY thread, then classify. Filtering first (by module) and
    // symbolizing second is what let an allocating thread disappear: its RIP is
    // in `ntdll` at the moment of the dump, so it never reached the symbolizer
    // that would have shown it doing work. Resolving first costs a handful of
    // export lookups and no PDBs.
    let all_threads: Vec<ThreadSample> = dump.threads.clone();
    let addrs: Vec<u64> = all_threads.iter().map(|t| t.rip).collect();
    let all_resolved = symbolize(&dump, &addrs, opts)?;

    let kept: Vec<(ThreadSample, Resolved)> = all_threads
        .iter()
        .cloned()
        .zip(all_resolved.iter().cloned())
        .filter(|(t, r)| include_all_threads || !dump.is_waiting(t, r.symbol.as_deref()))
        .collect();
    let waiting = all_threads.len() - kept.len();
    let (busy, resolved): (Vec<ThreadSample>, Vec<Resolved>) = kept.into_iter().unzip();

    let threads: Vec<Value> = busy
        .iter()
        .zip(resolved.iter())
        .map(|(t, r)| {
            json!({
                "tid": t.tid,
                "rip": format!("0x{:x}", t.rip),
                "module": r.module,
                "rva": r.rva.map(|v| format!("0x{v:x}")),
                "symbol": r.symbol,
                "frame": r.display(),
            })
        })
        .collect();

    let resolved_count = resolved.iter().filter(|r| r.symbol.is_some()).count();

    // Downloading was asked for but this dbghelp cannot do it: say so, or the
    // nulls read as "these frames have no symbols".
    // Downloading is wired up but has never been observed to actually fetch a
    // PDB in testing (dbghelp+symsrv load, the EULA marker is written, the PDB
    // is confirmed present on the server, and still nothing arrives). Say that
    // out loud rather than let a caller read unresolved frames as "no symbols
    // exist for this code".
    let download_ineffective =
        opts.download && symbol_download_supported() && resolved_count == 0 && !resolved.is_empty();

    let download_warning = if opts.download && !symbol_download_supported() {
        // Try to fix it for next time rather than just complaining: the copy
        // cannot help the process that makes it (dbghelp is bound before main),
        // but one re-run then works.
        let installed = provision_symbol_dlls();
        Some(format!(
            "Symbol download is unavailable in this process: it is running {}, and the System32 \
             dbghelp has no symsrv.dll — the loader binds `dbghelp.dll` by base name at process \
             start, before anything can redirect it. {} Alternatively pre-populate {} with an \
             external symbol tool and run without --download-symbols.",
            loaded_dbghelp_path().unwrap_or_else(|| "the System32 dbghelp".into()),
            installed.unwrap_or_else(|| {
                if symbol_dlls_installed() {
                    "dbghelp.dll and symsrv.dll are already in place next to the executable — \
                     RE-RUN this command and they will be used."
                        .to_string()
                } else {
                    "Copy dbghelp.dll AND symsrv.dll from the Windows Kits Debuggers\\x64 folder \
                     next to dalzzack.exe (the application directory outranks System32), then \
                     re-run."
                        .to_string()
                }
            }),
            opts.cache_dir.display()
        ))
    } else if download_ineffective {
        Some(format!(
            "--download-symbols is set up correctly (dbghelp: {}, symsrv loaded) but nothing \
             resolved and the cache at {} stayed empty — the PDB fetch is NOT happening. Treat \
             these frames as UNSYMBOLIZED, not as code without symbols. Workaround: fetch the \
             PDB into the cache with a symbol tool (WinDbg/symchk against the same directory), \
             then re-run without --download-symbols.",
            dbghelp_source(),
            opts.cache_dir.display()
        ))
    } else {
        None
    };

    Ok(json!({
        "dump": path.display().to_string(),
        "thread_count": dump.threads.len(),
        "busy_threads": threads,
        "waiting_threads": waiting,
        "module_count": dump.modules.len(),
        "symbols_resolved": resolved_count,
        "symbols_unresolved": resolved.len() - resolved_count,
        "dbghelp": dbghelp_source(),
        "symsrv_loaded": symsrv_loaded(),
        "download_warning": download_warning,
        "symbol_errors": take_load_errors(),
        // dbghelp/symsrv's own account of the symbol search, when downloading.
        "dbghelp_log": take_dbghelp_log(),
    }))
}

/// Collapse frames from several samples into a histogram, commonest first.
///
/// One sample cannot distinguish "spinning here" from "happened to be here";
/// this is what makes a spin claim checkable.
pub fn histogram(samples: &[Value]) -> Vec<Value> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for s in samples {
        if let Some(threads) = s.get("busy_threads").and_then(|t| t.as_array()) {
            for t in threads {
                if let Some(frame) = t.get("frame").and_then(|f| f.as_str()) {
                    *counts.entry(frame.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows.into_iter()
        .map(|(frame, count)| json!({ "frame": frame, "count": count }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(base: u64, size: u32, path: &str) -> ModuleInfo {
        ModuleInfo {
            base,
            size,
            path: path.to_string(),
        }
    }

    #[test]
    fn waiting_threads_are_excluded_by_default() {
        let dump = DumpContents {
            modules: vec![
                module(0x1000, 0x1000, r"C:\Windows\System32\ntdll.dll"),
                module(0x9000, 0x1000, r"C:\Edge\151.0.4129.72\msedge.dll"),
            ],
            threads: vec![
                ThreadSample {
                    tid: 1,
                    rip: 0x1100,
                },
                ThreadSample {
                    tid: 2,
                    rip: 0x9100,
                },
                ThreadSample {
                    tid: 3,
                    rip: 0x1200,
                },
            ],
        };
        // No symbols available → module decides.
        assert!(dump.is_waiting(&dump.threads[0], None));
        assert!(!dump.is_waiting(&dump.threads[1], None), "msedge is work");
        assert!(dump.is_waiting(&dump.threads[2], None));
    }

    /// The case that made module-based filtering wrong: a thread hammering the
    /// allocator sits in `ntdll` a good share of the time, and dropping it hides
    /// the allocation storm you're looking for.
    #[test]
    fn allocating_threads_in_ntdll_count_as_busy() {
        let dump = DumpContents {
            modules: vec![module(0x1000, 0x1000, r"C:\Windows\System32\ntdll.dll")],
            threads: vec![ThreadSample {
                tid: 1,
                rip: 0x1100,
            }],
        };
        assert!(
            !dump.is_waiting(&dump.threads[0], Some("NtAllocateVirtualMemory")),
            "allocation is work, not a wait, even though it lives in ntdll"
        );
        assert!(dump.is_waiting(&dump.threads[0], Some("NtWaitForSingleObject")));
    }

    #[test]
    fn ui_message_loops_count_as_waiting_too() {
        // win32u is a syscall stub layer just like ntdll; leaving it in put
        // `NtUserMsgWaitForMultipleObjectsEx` at the top of every histogram.
        let dump = DumpContents {
            modules: vec![
                module(0x2000, 0x1000, r"C:\Windows\System32\win32u.dll"),
                module(0x9000, 0x1000, r"C:\Edge\151.0.4129.72\msedge.dll"),
            ],
            threads: vec![
                ThreadSample {
                    tid: 1,
                    rip: 0x2100,
                },
                ThreadSample {
                    tid: 2,
                    rip: 0x9100,
                },
            ],
        };
        assert!(dump.is_waiting(&dump.threads[0], None));
        assert!(!dump.is_waiting(&dump.threads[1], None));
        // And by name, wherever it lives.
        assert!(dump.is_waiting(&dump.threads[1], Some("NtUserMsgWaitForMultipleObjectsEx")));
    }

    /// Enumerating wait syscalls by name never converged — `ZwFsControlFile`
    /// and `ZwDelayExecution` kept turning up at the top of histograms. The
    /// rule is now "a syscall from a stub module is a block unless it's on the
    /// work list", which needs no maintenance as new ones appear.
    #[test]
    fn unlisted_syscalls_in_stub_modules_count_as_waiting() {
        let dump = DumpContents {
            modules: vec![module(0x1000, 0x1000, r"C:\Windows\System32\ntdll.dll")],
            threads: vec![ThreadSample {
                tid: 1,
                rip: 0x1100,
            }],
        };
        let t = &dump.threads[0];
        for sym in [
            "NtWaitForSingleObject",
            "ZwDelayExecution",
            "ZwFsControlFile",
            "ZwWaitForWorkViaWorkerFactory",
            "NtRemoveIoCompletion",
        ] {
            assert!(
                dump.is_waiting(t, Some(sym)),
                "{sym} should read as waiting"
            );
        }
        for sym in ["NtAllocateVirtualMemory", "ZwMapViewOfSection"] {
            assert!(!dump.is_waiting(t, Some(sym)), "{sym} is work");
        }
    }

    #[test]
    fn real_code_is_never_mistaken_for_a_wait() {
        let dump = DumpContents {
            modules: vec![module(0x9000, 0x1000, r"C:\Edge\151.0.4129.72\msedge.dll")],
            threads: vec![ThreadSample {
                tid: 1,
                rip: 0x9100,
            }],
        };
        assert!(!dump.is_waiting(&dump.threads[0], Some("v8::internal::Factory::AllocateRaw")));
        // A blocking wait forwarded through kernel32 still counts.
        assert!(is_wait_symbol("WaitForSingleObjectEx"));
    }

    #[test]
    fn unmapped_addresses_are_kept_not_silently_dropped() {
        let dump = DumpContents {
            modules: vec![module(0x1000, 0x1000, r"C:\Windows\System32\ntdll.dll")],
            threads: vec![ThreadSample {
                tid: 7,
                rip: 0xdead_beef,
            }],
        };
        // JIT code belongs to no module — never treat that as a wait.
        assert!(!dump.is_waiting(&dump.threads[0], None));
    }

    #[test]
    fn unresolved_frames_fall_back_to_module_plus_rva() {
        let r = Resolved {
            module: Some("msedge.dll".into()),
            symbol: None,
            displacement: 0,
            rva: Some(0x24b901),
        };
        assert_eq!(r.display(), "msedge.dll+0x24b901");
    }

    #[test]
    fn resolved_frames_carry_the_displacement() {
        let r = Resolved {
            module: Some("msedge.dll".into()),
            symbol: Some("v8::internal::Factory::AllocateRaw".into()),
            displacement: 0xf1,
            rva: Some(0x24b901),
        };
        assert_eq!(r.display(), "v8::internal::Factory::AllocateRaw+0xf1");
    }

    #[test]
    fn histogram_counts_repeated_frames_across_samples() {
        let sample = |frame: &str| json!({ "busy_threads": [ { "frame": frame } ] });
        let rows = histogram(&[sample("A"), sample("B"), sample("A")]);
        assert_eq!(rows[0]["frame"], "A");
        assert_eq!(rows[0]["count"], 2);
        assert_eq!(rows[1]["count"], 1);
    }
}
