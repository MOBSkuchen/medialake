//! C ABI over searchnow-core. Deliberately coarse: a handful of entry points, JSON across the boundary.
//!
//! Strings returned by `sn_call` / `sn_version` / `sn_last_error` must be released with `sn_free_string`.
//! The event callback is invoked on a Rust thread; the host must marshal to its UI thread.

mod dispatch;

use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use searchnow_core::config::Paths;
use searchnow_core::core::{Core, StartOptions};
use tokio::runtime::Runtime;

pub struct Handle {
    rt: Runtime,
    core: Arc<Core>,
    cb: Arc<Mutex<Option<EventCallback>>>,
    _log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

pub type EventCallback = extern "C" fn(*const c_char);

static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

fn set_error(e: impl std::fmt::Display) {
    *LAST_ERROR.lock().unwrap() = e.to_string();
}

fn to_c(s: String) -> *mut c_char {
    CString::new(s.replace('\0', " ")).unwrap().into_raw()
}

unsafe fn from_c(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

#[unsafe(no_mangle)]
pub extern "C" fn sn_version() -> *mut c_char {
    to_c(env!("CARGO_PKG_VERSION").to_string())
}

#[unsafe(no_mangle)]
pub extern "C" fn sn_last_error() -> *mut c_char {
    to_c(LAST_ERROR.lock().unwrap().clone())
}

/// `options_json`: `{ "server_exe": "...", "model_dir": "...", "gpu_layers": N }` (all optional).
/// Returns an opaque handle, or null on failure (see `sn_last_error`).
///
/// # Safety
/// Both pointers must be null or valid NUL-terminated UTF-8 strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sn_init(config_dir: *const c_char, options_json: *const c_char) -> *mut Handle {
    let (dir, opts) = unsafe { (from_c(config_dir), from_c(options_json)) };
    let r = catch_unwind(AssertUnwindSafe(|| -> Result<Handle, String> {
        let root = if dir.is_empty() { Paths::default_root() } else { PathBuf::from(dir) };
        let paths = Paths::new(root);
        paths.ensure().map_err(|e| e.to_string())?;

        let guard = init_logging(&paths);
        let o: serde_json::Value = serde_json::from_str(&opts).unwrap_or_default();
        let options = StartOptions {
            server_exe: o["server_exe"].as_str().map(PathBuf::from),
            model_dir: o["model_dir"].as_str().map(PathBuf::from),
            gpu_layers: o["gpu_layers"].as_i64().map(|v| v as i32),
            no_watch: false,
            workers: None,
        };
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().thread_name("searchnow").build().map_err(|e| e.to_string())?;
        let core = rt.block_on(Core::start(paths, options)).map_err(|e| e.to_string())?;

        let cb: Arc<Mutex<Option<EventCallback>>> = Arc::new(Mutex::new(None));
        let mut rx = core.subscribe();
        let cb2 = cb.clone();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let f = *cb2.lock().unwrap();
                        if let (Some(f), Ok(json)) = (f, serde_json::to_string(&ev)) {
                            if let Ok(c) = CString::new(json) {
                                f(c.as_ptr());
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        Ok(Handle { rt, core, cb, _log_guard: guard })
    }));
    match r {
        Ok(Ok(h)) => Box::into_raw(Box::new(h)),
        Ok(Err(e)) => {
            set_error(e);
            std::ptr::null_mut()
        }
        Err(_) => {
            set_error("panic during sn_init");
            std::ptr::null_mut()
        }
    }
}

fn init_logging(paths: &Paths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let app = tracing_appender::rolling::daily(&paths.logs_dir, "client.log");
    let (nb, guard) = tracing_appender::non_blocking(app);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,searchnow_core=info,searchnow_core_ffi=info"));
    tracing_subscriber::registry().with(filter).with(fmt::layer().with_writer(nb).with_ansi(false)).try_init().ok()?;
    Some(guard)
}

/// # Safety
/// `h` must come from `sn_init` and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sn_shutdown(h: *mut Handle) {
    if h.is_null() {
        return;
    }
    let h = unsafe { Box::from_raw(h) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        *h.cb.lock().unwrap() = None;
        h.rt.block_on(h.core.shutdown());
    }));
    h.rt.shutdown_timeout(std::time::Duration::from_secs(3));
}

/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sn_set_event_callback(h: *mut Handle, cb: Option<EventCallback>) {
    if let Some(h) = unsafe { h.as_ref() } {
        *h.cb.lock().unwrap() = cb;
    }
}

/// Synchronous call; run it on a background thread. Returns `{"ok":...}` or `{"error":"..."}`.
///
/// # Safety
/// `h` must be a live handle; `method` / `args_json` valid NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sn_call(h: *mut Handle, method: *const c_char, args_json: *const c_char) -> *mut c_char {
    let Some(h) = (unsafe { h.as_ref() }) else {
        return to_c(r#"{"error":"not initialised"}"#.into());
    };
    let (method, args) = unsafe { (from_c(method), from_c(args_json)) };
    let out = catch_unwind(AssertUnwindSafe(|| {
        let args: serde_json::Value = if args.is_empty() { serde_json::Value::Null } else { serde_json::from_str(&args).unwrap_or(serde_json::Value::Null) };
        match h.rt.block_on(dispatch::call(&h.core, &method, args)) {
            Ok(v) => serde_json::json!({ "ok": v }),
            Err(e) => serde_json::json!({ "error": e }),
        }
    }))
    .unwrap_or_else(|_| serde_json::json!({ "error": "internal panic" }));
    to_c(out.to_string())
}

/// # Safety
/// `p` must come from this library (or be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sn_free_string(p: *mut c_char) {
    if !p.is_null() {
        drop(unsafe { CString::from_raw(p) });
    }
}
