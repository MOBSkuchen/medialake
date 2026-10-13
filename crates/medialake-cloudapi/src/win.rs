//! The Windows implementation of [`CloudApi`] over cfapi. Everything here is unverified on a real machine:
//! it type-checks, and `docs/w0-cloud-files.md` lists what to confirm.

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use windows::Win32::Foundation::{CloseHandle, HANDLE, NTSTATUS, STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE, STATUS_CLOUD_FILE_UNSUCCESSFUL};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FindClose, FindFirstFileW, GetVolumeInformationW,
    OPEN_EXISTING, WIN32_FIND_DATAW,
};
use windows::core::{GUID, PCWSTR};

use crate::plan::rel_from_normalized;
use crate::types::*;

// ---- policy choices (see docs/w0-cloud-files.md for why, and what to try instead) ----

/// Partial hydration: only the ranges that are read are fetched (a thumbnail of a 2 GB video does not pull
/// 2 GB). The plan said "full"; this is the deviation to check first on a real machine.
const HYDRATION: CF_HYDRATION_POLICY_PRIMARY = CF_HYDRATION_POLICY_PARTIAL;
/// Largest piece of one TRANSFER_DATA.
pub const MAX_CHUNK: u64 = 4 << 20;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn fail(what: &str, e: windows::core::Error) -> CloudError {
    CloudError::Failed(format!("{what}: {e}"))
}

/// Milliseconds since the Unix epoch <-> FILETIME (100 ns since 1601).
const EPOCH_DELTA_MS: i64 = 11_644_473_600_000;

fn to_filetime(ms: i64) -> i64 {
    (ms.saturating_add(EPOCH_DELTA_MS)).max(0).saturating_mul(10_000)
}

/// A handle from `CfOpenFileWithOplock`: protected, only for Cf* calls.
struct CfHandle(HANDLE);

impl Drop for CfHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from CfOpenFileWithOplock and is closed once.
        unsafe { CfCloseHandle(self.0) }
    }
}

fn open_cf(path: &Path, write: bool) -> CloudResult<CfHandle> {
    let w = wide(path);
    let flags = if write { CF_OPEN_FILE_FLAG_WRITE_ACCESS } else { CF_OPEN_FILE_FLAG_NONE };
    // SAFETY: `w` is NUL-terminated and outlives the call.
    let h = unsafe { CfOpenFileWithOplock(PCWSTR(w.as_ptr()), flags) }.map_err(|e| fail(&format!("opening {}", path.display()), e))?;
    Ok(CfHandle(h))
}

/// An ordinary handle, for the calls that want one (`CfConvertToPlaceholder`).
struct PlainHandle(HANDLE);

impl Drop for PlainHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn open_plain(path: &Path) -> CloudResult<PlainHandle> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated and outlives the call.
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_WRITE_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|e| fail(&format!("opening {}", path.display()), e))?;
    Ok(PlainHandle(h))
}

fn metadata(info: &PlaceholderInfo) -> CF_FS_METADATA {
    let ft = to_filetime(info.mtime_ms);
    let mut attrs = if info.is_dir { FILE_ATTRIBUTE_DIRECTORY.0 } else { FILE_ATTRIBUTE_NORMAL.0 };
    if info.read_only && !info.is_dir {
        attrs = FILE_ATTRIBUTE_READONLY.0;
    }
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO { CreationTime: ft, LastAccessTime: ft, LastWriteTime: ft, ChangeTime: ft, FileAttributes: attrs },
        FileSize: if info.is_dir { 0 } else { info.size as i64 },
    }
}

fn find(path: &Path) -> Option<WIN32_FIND_DATAW> {
    let w = wide(path);
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `w` is NUL-terminated; `data` is a valid out parameter.
    let h = unsafe { FindFirstFileW(PCWSTR(w.as_ptr()), &mut data) }.ok()?;
    // SAFETY: `h` is the search handle just returned.
    let _ = unsafe { FindClose(h) };
    Some(data)
}

// ---- callbacks ----

struct Ctx {
    provider: Arc<dyn Provider>,
    /// The root as registered, for turning a callback's volume-relative path into a sync path.
    root: String,
}

unsafe fn pcwstr_to_string(p: PCWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the platform passes NUL-terminated strings.
    unsafe { p.to_string().unwrap_or_default() }
}

struct WinTransfer {
    key: CF_CONNECTION_KEY,
    transfer_key: i64,
    request_key: i64,
}

impl WinTransfer {
    fn op_info(&self) -> CF_OPERATION_INFO {
        CF_OPERATION_INFO {
            StructSize: size_of::<CF_OPERATION_INFO>() as u32,
            Type: CF_OPERATION_TYPE_TRANSFER_DATA,
            ConnectionKey: self.key,
            TransferKey: self.transfer_key,
            CorrelationVector: std::ptr::null(),
            SyncStatus: std::ptr::null(),
            RequestKey: self.request_key,
        }
    }

    fn execute(&self, status: NTSTATUS, offset: u64, length: u64, buffer: *const c_void) -> windows::core::Result<()> {
        let info = self.op_info();
        // SAFETY: an all-zero parameter block is valid; the union member used is set below.
        let mut params: CF_OPERATION_PARAMETERS = unsafe { std::mem::zeroed() };
        params.ParamSize = (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_0>()) as u32;
        params.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_0 {
            Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
            CompletionStatus: status,
            Buffer: buffer,
            Offset: offset as i64,
            Length: length as i64,
        };
        // SAFETY: both structures are fully initialised; `buffer` (if any) is valid for `length` bytes.
        unsafe { CfExecute(&info, &mut params) }
    }

    /// Ends the request with an error (Explorer shows it at once instead of waiting for a time-out).
    fn abort(&self, e: &CloudError, offset: u64, length: u64) {
        let status = match e {
            CloudError::Offline(_) => STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE,
            _ => STATUS_CLOUD_FILE_UNSUCCESSFUL,
        };
        let _ = self.execute(status, offset, length, std::ptr::null());
    }
}

impl Transfer for WinTransfer {
    fn data(&self, offset: u64, bytes: &[u8]) -> CloudResult<()> {
        self.execute(NTSTATUS(0), offset, bytes.len() as u64, bytes.as_ptr().cast()).map_err(|e| fail("transferring data", e))
    }

    fn progress(&self, total: u64, done: u64) {
        // SAFETY: plain values.
        let _ = unsafe { CfReportProviderProgress(self.key, self.transfer_key, total as i64, done as i64) };
    }
}

unsafe extern "system" fn on_fetch_data(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: the platform passes valid pointers for the duration of the callback; the context is the
    // `Ctx` that `connect` leaked and keeps alive until the connection is dropped.
    let (info, params) = unsafe { (&*info, &*params) };
    let ctx = unsafe { &*(info.CallbackContext as *const Ctx) };
    let fetch = unsafe { params.Anonymous.FetchData };
    let (offset, length) = (fetch.RequiredFileOffset.max(0) as u64, fetch.RequiredLength.max(0) as u64);
    let t = WinTransfer { key: info.ConnectionKey, transfer_key: info.TransferKey, request_key: info.RequestKey };

    let normalized = unsafe { pcwstr_to_string(info.NormalizedPath) };
    let Some(rel) = rel_from_normalized(&ctx.root, &normalized) else {
        t.abort(&CloudError::Failed(format!("{normalized} is not below the sync root")), offset, length);
        return;
    };
    let identity = if info.FileIdentity.is_null() || info.FileIdentityLength == 0 {
        None
    } else {
        // SAFETY: the platform guarantees `FileIdentityLength` readable bytes.
        Identity::decode(unsafe { std::slice::from_raw_parts(info.FileIdentity.cast::<u8>(), info.FileIdentityLength as usize) })
    };
    let req = FetchRequest { rel, identity, file_size: info.FileSize.max(0) as u64, offset, length };
    // a panic must not unwind into the system's callback dispatcher
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.provider.fetch(&req, &t)));
    match r {
        Ok(Ok(())) => {}
        Ok(Err(e)) => t.abort(&e, offset, length),
        Err(_) => t.abort(&CloudError::Failed("the provider panicked".into()), offset, length),
    }
}

unsafe extern "system" fn on_cancel_fetch_data(info: *const CF_CALLBACK_INFO, _params: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: as in `on_fetch_data`.
    let info = unsafe { &*info };
    let ctx = unsafe { &*(info.CallbackContext as *const Ctx) };
    let normalized = unsafe { pcwstr_to_string(info.NormalizedPath) };
    if let Some(rel) = rel_from_normalized(&ctx.root, &normalized) {
        ctx.provider.cancel(&rel);
    }
}

struct WinConnection {
    key: CF_CONNECTION_KEY,
    ctx: *mut Ctx,
}

// SAFETY: the key is a plain value and `ctx` is only freed in `drop`.
unsafe impl Send for WinConnection {}

impl Connection for WinConnection {}

impl Drop for WinConnection {
    fn drop(&mut self) {
        // SAFETY: disconnect first, so that no callback can run, then free what it used.
        unsafe {
            let _ = CfDisconnectSyncRoot(self.key);
            drop(Box::from_raw(self.ctx));
        }
    }
}

// ---- the API ----

/// [`CloudApi`] over cfapi.
#[derive(Default)]
pub struct WinCloud;

impl CloudApi for WinCloud {
    fn available(&self, root: &Path) -> Result<(), String> {
        // SAFETY: no arguments.
        unsafe { CfGetPlatformInfo() }.map_err(|e| format!("Cloud Files is not available on this Windows: {e}"))?;
        let s = root.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        let b = s.as_bytes();
        if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
            return Err("Cloud Files needs a folder on a local drive (not a network path)".into());
        }
        let drive = wide_str(&format!("{}:\\", b[0] as char));
        let mut fs = [0u16; 32];
        // SAFETY: `drive` is NUL-terminated; the buffer is valid for its length.
        unsafe { GetVolumeInformationW(PCWSTR(drive.as_ptr()), None, None, None, None, Some(&mut fs)) }
            .map_err(|e| format!("cannot read the volume of {}: {e}", root.display()))?;
        let name = String::from_utf16_lossy(&fs[..fs.iter().position(|&c| c == 0).unwrap_or(fs.len())]);
        if !name.eq_ignore_ascii_case("NTFS") {
            return Err(format!("Cloud Files needs an NTFS volume ({} is {name})", &s[..2]));
        }
        Ok(())
    }

    fn register(&self, root: &Path, reg: &Registration) -> CloudResult<()> {
        let (name, version) = (wide_str(&reg.provider_name), wide_str(&reg.provider_version));
        let r = CF_SYNC_REGISTRATION {
            StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
            ProviderName: PCWSTR(name.as_ptr()),
            ProviderVersion: PCWSTR(version.as_ptr()),
            SyncRootIdentity: reg.root_identity.as_ptr().cast(),
            SyncRootIdentityLength: reg.root_identity.len() as u32,
            FileIdentity: std::ptr::null(),
            FileIdentityLength: 0,
            ProviderId: GUID::from_u128(u128::from_be_bytes(reg.provider_id)),
        };
        let p = CF_SYNC_POLICIES {
            StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
            Hydration: CF_HYDRATION_POLICY {
                Primary: HYDRATION,
                // Storage Sense may free space again; streaming reads are served as they arrive
                Modifier: CF_HYDRATION_POLICY_MODIFIER(
                    CF_HYDRATION_POLICY_MODIFIER_AUTO_DEHYDRATION_ALLOWED.0 | CF_HYDRATION_POLICY_MODIFIER_STREAMING_ALLOWED.0,
                ),
            },
            // every placeholder is created from the host's listing: browsing needs no network
            Population: CF_POPULATION_POLICY { Primary: CF_POPULATION_POLICY_ALWAYS_FULL, Modifier: CF_POPULATION_POLICY_MODIFIER_NONE },
            InSync: CF_INSYNC_POLICY_TRACK_ALL,
            HardLink: CF_HARDLINK_POLICY_NONE,
            PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
        };
        let w = wide(root);
        // SAFETY: all pointers are valid for the call.
        let first = unsafe { CfRegisterSyncRoot(PCWSTR(w.as_ptr()), &r, &p, CF_REGISTER_FLAG_NONE) };
        match first {
            Ok(()) => Ok(()),
            // already registered (an earlier run): update it
            Err(_) => unsafe { CfRegisterSyncRoot(PCWSTR(w.as_ptr()), &r, &p, CF_REGISTER_FLAG_UPDATE) }
                .map_err(|e| fail(&format!("registering {}", root.display()), e)),
        }
    }

    fn connect(&self, root: &Path, provider: Arc<dyn Provider>) -> CloudResult<Box<dyn Connection>> {
        let ctx = Box::into_raw(Box::new(Ctx { provider, root: root.to_string_lossy().into_owned() }));
        let table = [
            CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_FETCH_DATA, Callback: Some(on_fetch_data) },
            CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA, Callback: Some(on_cancel_fetch_data) },
            CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NONE, Callback: None },
        ];
        let w = wide(root);
        // SAFETY: the table and path are valid for the call; the context lives until the connection drops.
        let key = unsafe { CfConnectSyncRoot(PCWSTR(w.as_ptr()), table.as_ptr(), Some(ctx as *const c_void), CF_CONNECT_FLAG_NONE) };
        match key {
            Ok(key) => Ok(Box::new(WinConnection { key, ctx })),
            Err(e) => {
                // SAFETY: the context was not handed to anything.
                drop(unsafe { Box::from_raw(ctx) });
                Err(fail(&format!("connecting {}", root.display()), e))
            }
        }
    }

    fn unregister(&self, root: &Path) -> CloudResult<()> {
        let w = wide(root);
        // SAFETY: `w` is NUL-terminated.
        unsafe { CfUnregisterSyncRoot(PCWSTR(w.as_ptr())) }.map_err(|e| fail(&format!("unregistering {}", root.display()), e))
    }

    fn create_placeholder(&self, parent: &Path, name: &str, info: &PlaceholderInfo) -> CloudResult<()> {
        let (parent_w, name_w) = (wide(parent), wide_str(name));
        let ident = info.identity.as_ref().map(Identity::encode);
        let mut flags = CF_PLACEHOLDER_CREATE_FLAG_NONE;
        if info.in_sync {
            flags |= CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC;
        }
        if info.is_dir {
            // the listing is complete: Explorer must not ask the provider to populate it
            flags |= CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION;
        }
        let mut items = [CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(name_w.as_ptr()),
            FsMetadata: metadata(info),
            FileIdentity: ident.as_ref().map_or(std::ptr::null(), |v| v.as_ptr().cast()),
            FileIdentityLength: ident.as_ref().map_or(0, |v| v.len() as u32),
            Flags: flags,
            Result: windows::core::HRESULT(0),
            CreateUsn: 0,
        }];
        let mut done = 0u32;
        // SAFETY: all pointers are valid for the call.
        unsafe { CfCreatePlaceholders(PCWSTR(parent_w.as_ptr()), &mut items, CF_CREATE_FLAG_NONE, Some(&mut done)) }
            .map_err(|e| fail(&format!("creating {name}"), e))?;
        if items[0].Result.is_err() {
            return Err(fail(&format!("creating {name}"), windows::core::Error::from(items[0].Result)));
        }
        Ok(())
    }

    fn update_placeholder(&self, path: &Path, info: &PlaceholderInfo, dehydrate: bool) -> CloudResult<()> {
        let h = open_cf(path, true)?;
        let md = metadata(info);
        let ident = info.identity.as_ref().map(Identity::encode);
        let mut flags = CF_UPDATE_FLAG_NONE;
        if info.in_sync {
            flags |= CF_UPDATE_FLAG_MARK_IN_SYNC;
        }
        if dehydrate {
            flags |= CF_UPDATE_FLAG_DEHYDRATE;
        }
        // SAFETY: `h` is an open handle; the pointers are valid for the call.
        unsafe {
            CfUpdatePlaceholder(
                h.0,
                Some(&md),
                ident.as_ref().map(|v| v.as_ptr().cast::<c_void>()),
                ident.as_ref().map_or(0, |v| v.len() as u32),
                None,
                flags,
                None,
                None,
            )
        }
        .map_err(|e| fail(&format!("updating {}", path.display()), e))
    }

    fn mark_synced(&self, path: &Path, identity: &Identity) -> CloudResult<()> {
        let id = identity.encode();
        if self.state(path)?.placeholder {
            let h = open_cf(path, true)?;
            // SAFETY: as in `update_placeholder`; no metadata change.
            unsafe { CfUpdatePlaceholder(h.0, None, Some(id.as_ptr().cast::<c_void>()), id.len() as u32, None, CF_UPDATE_FLAG_MARK_IN_SYNC, None, None) }
                .map_err(|e| fail(&format!("updating {}", path.display()), e))
        } else {
            // a file that was created here (or downloaded): it becomes a placeholder that can be freed later
            let h = open_plain(path)?;
            // SAFETY: `h` is an open handle; `id` is valid for the call.
            unsafe { CfConvertToPlaceholder(h.0, Some(id.as_ptr().cast::<c_void>()), id.len() as u32, CF_CONVERT_FLAG_MARK_IN_SYNC, None, None) }
                .map_err(|e| fail(&format!("converting {}", path.display()), e))
        }
    }

    fn state(&self, path: &Path) -> CloudResult<FileState> {
        state_of(path, true)
    }

    fn state_light(&self, path: &Path) -> CloudResult<FileState> {
        state_of(path, false)
    }

    fn hydrate(&self, path: &Path) -> CloudResult<()> {
        let h = open_cf(path, true)?;
        // SAFETY: `h` is an open handle; length -1 = to the end of the file.
        unsafe { CfHydratePlaceholder(h.0, 0, -1, CF_HYDRATE_FLAG_NONE, None) }.map_err(|e| fail(&format!("hydrating {}", path.display()), e))
    }

    fn dehydrate(&self, path: &Path) -> CloudResult<()> {
        let h = open_cf(path, true)?;
        // SAFETY: as in `hydrate`.
        unsafe { CfDehydratePlaceholder(h.0, 0, -1, CF_DEHYDRATE_FLAG_NONE, None) }
            .map_err(|e| fail(&format!("freeing {}", path.display()), e))
    }

    fn set_pin(&self, path: &Path, pin: Pin) -> CloudResult<()> {
        let h = open_cf(path, true)?;
        let state = match pin {
            Pin::Unspecified => CF_PIN_STATE_UNSPECIFIED,
            Pin::Pinned => CF_PIN_STATE_PINNED,
            Pin::Unpinned => CF_PIN_STATE_UNPINNED,
            Pin::Excluded => CF_PIN_STATE_EXCLUDED,
            Pin::Inherit => CF_PIN_STATE_INHERIT,
        };
        // SAFETY: `h` is an open handle.
        unsafe { CfSetPinState(h.0, state, CF_SET_PIN_FLAG_NONE, None) }.map_err(|e| fail(&format!("pinning {}", path.display()), e))
    }

    fn revert(&self, path: &Path) -> CloudResult<()> {
        let h = open_cf(path, true)?;
        // SAFETY: `h` is an open handle.
        unsafe { CfRevertPlaceholder(h.0, CF_REVERT_FLAG_NONE, None) }.map_err(|e| fail(&format!("reverting {}", path.display()), e))
    }
}

/// The state of a path: attributes from a directory lookup, and (optionally) the placeholder's identity.
fn state_of(path: &Path, with_identity: bool) -> CloudResult<FileState> {
    let Some(d) = find(path) else { return Ok(FileState::default()) };
    let attrs = d.dwFileAttributes;
    let tag = if attrs & attr::REPARSE_POINT != 0 { d.dwReserved0 } else { 0 };
    // SAFETY: plain values.
    let ps = unsafe { CfGetPlaceholderStateFromAttributeTag(attrs, tag) };
    let placeholder = ps != CF_PLACEHOLDER_STATE_INVALID && ps.0 & CF_PLACEHOLDER_STATE_PLACEHOLDER.0 != 0;
    let pin = if attrs & attr::PINNED != 0 {
        Pin::Pinned
    } else if attrs & attr::UNPINNED != 0 {
        Pin::Unpinned
    } else {
        Pin::Unspecified
    };
    let mut st = FileState {
        exists: true,
        is_dir: attrs & attr::DIRECTORY != 0,
        placeholder,
        in_sync: placeholder && ps.0 & CF_PLACEHOLDER_STATE_IN_SYNC.0 != 0,
        dehydrated: placeholder && attrs & attr::RECALL_ON_DATA_ACCESS != 0,
        pin,
        identity: None,
        size: (u64::from(d.nFileSizeHigh) << 32) | u64::from(d.nFileSizeLow),
        attributes: attrs,
    };
    if placeholder && with_identity {
        if let Ok(h) = open_cf(path, false) {
            // 4 KiB of identity plus the header, 8-byte aligned
            let mut buf = [0u64; 640];
            let mut returned = 0u32;
            // SAFETY: the buffer is valid for the stated length.
            let ok = unsafe {
                CfGetPlaceholderInfo(h.0, CF_PLACEHOLDER_INFO_BASIC, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, Some(&mut returned))
            };
            if ok.is_ok() {
                // SAFETY: the call filled a CF_PLACEHOLDER_BASIC_INFO at the start of the buffer.
                let info = unsafe { &*(buf.as_ptr().cast::<CF_PLACEHOLDER_BASIC_INFO>()) };
                let off = offset_of!(CF_PLACEHOLDER_BASIC_INFO, FileIdentity);
                let len = (info.FileIdentityLength as usize).min(buf.len() * 8 - off);
                // SAFETY: `off + len` is inside `buf`.
                let bytes = unsafe { std::slice::from_raw_parts((buf.as_ptr() as *const u8).add(off), len) };
                st.identity = Identity::decode(bytes);
            }
        }
    }
    Ok(st)
}
