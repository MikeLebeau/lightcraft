//! Browser storage, asynchronous: OPFS (Origin Private File System) when the page can write it
//! from the main thread (`FileSystemFileHandle.createWritable`), else IndexedDB. Both local
//! backends are used from the window and render workers. The experimental remote backend can be
//! used by both, sharing the window's exclusive edit lease through worker initialization.

use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions, FileSystemGetFileOptions, Headers, IdbDatabase, Request,
    RequestInit, Response,
};

const IDB_NAME: &str = "lightcraft";
const IDB_STORE: &str = "files";

#[derive(Clone)]
pub enum Backend {
    Opfs(FileSystemDirectoryHandle),
    Idb(IdbDatabase),
    Remote(RemoteBackend),
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub struct RemoteBackend {
    api: String,
    api_token: String,
    lease: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseReply {
    lease: String,
}

#[derive(serde::Deserialize)]
struct HandoffId {
    id: String,
}

#[derive(serde::Deserialize)]
struct HandoffStatus {
    state: String,
    lease: Option<String>,
}

fn err(e: JsValue) -> String {
    e.as_string().or_else(|| e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message()))).unwrap_or_else(|| format!("{e:?}"))
}

fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

async fn fetch_bytes(
    method: &str,
    url: &str,
    api_token: &str,
    lease: Option<&str>,
    offset: Option<usize>,
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>), String> {
    let headers = Headers::new().map_err(err)?;
    headers.set("Authorization", &format!("Bearer {api_token}")).map_err(err)?;
    if let Some(lease) = lease {
        headers.set("X-LightCraft-Lease", lease).map_err(err)?;
    }
    if let Some(offset) = offset {
        headers.set("X-LightCraft-Offset", &offset.to_string()).map_err(err)?;
    }
    let init = RequestInit::new();
    init.set_method(method);
    init.set_headers(headers.as_ref());
    if let Some(body) = body {
        init.set_body(&Uint8Array::from(body).into());
    }
    let request = Request::new_with_str_and_init(url, &init).map_err(err)?;
    let global = js_sys::global();
    let fetch = Reflect::get(&global, &"fetch".into()).map_err(err)?.dyn_into::<js_sys::Function>().map_err(err)?;
    let promise = fetch.call1(&global, &request).map_err(err)?.dyn_into::<js_sys::Promise>().map_err(err)?;
    let response: Response = JsFuture::from(promise).await.map_err(err)?.dyn_into().map_err(err)?;
    let status = response.status();
    let bytes = JsFuture::from(response.array_buffer().map_err(err)?).await.map_err(err)?;
    Ok((status, Uint8Array::new(&bytes).to_vec()))
}

fn check_status(status: u16, body: &[u8]) -> Result<(), String> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(body);
        Err(format!("shared storage returned HTTP {status}: {detail}"))
    }
}

impl RemoteBackend {
    fn path_url(&self, path: &str) -> String {
        format!("{}/files/{}", self.api, path.split('/').map(encode_component).collect::<Vec<_>>().join("/"))
    }

    async fn request(&self, method: &str, url: &str, offset: Option<usize>, body: Option<&[u8]>) -> Result<(u16, Vec<u8>), String> {
        fetch_bytes(method, url, &self.api_token, Some(&self.lease), offset, body).await
    }

    async fn acquire(api: String, api_token: String) -> Result<RemoteBackend, String> {
        let (status, bytes) = fetch_bytes("POST", &format!("{api}/lease"), &api_token, None, None, None).await?;
        check_status(status, &bytes)?;
        Self::from_lease_reply(api, api_token, &bytes)
    }

    fn from_lease_reply(api: String, api_token: String, bytes: &[u8]) -> Result<RemoteBackend, String> {
        let reply: LeaseReply = serde_json::from_slice(bytes).map_err(|e| format!("invalid lease response: {e}"))?;
        if reply.lease.is_empty() {
            return Err("shared storage returned an empty lease".into());
        }
        Ok(RemoteBackend { api, api_token, lease: reply.lease })
    }

    async fn renew(&self) -> Result<(), String> {
        let (status, bytes) = self.request("POST", &format!("{}/lease/renew", self.api), None, None).await?;
        check_status(status, &bytes)
    }

    async fn release(&self) -> Result<(), String> {
        let (status, bytes) = fetch_bytes("DELETE", &format!("{}/lease", self.api), &self.api_token, Some(&self.lease), None, None).await?;
        check_status(status, &bytes)
    }

    async fn request_handoff(&self) -> Result<String, String> {
        let (status, bytes) = fetch_bytes("POST", &format!("{}/lease/handoff", self.api), &self.api_token, None, None, None).await?;
        check_status(status, &bytes)?;
        serde_json::from_slice::<HandoffId>(&bytes).map(|reply| reply.id).map_err(|e| format!("invalid handoff response: {e}"))
    }

    async fn pending_handoff(&self) -> Result<Option<String>, String> {
        let (status, bytes) = self.request("GET", &format!("{}/lease/handoff/pending", self.api), None, None).await?;
        check_status(status, &bytes)?;
        serde_json::from_slice::<Option<HandoffId>>(&bytes)
            .map(|reply| reply.map(|reply| reply.id))
            .map_err(|e| format!("invalid pending handoff response: {e}"))
    }

    async fn acknowledge_handoff(&self, id: &str) -> Result<(), String> {
        let (status, bytes) = self.request("POST", &format!("{}/lease/handoff/{id}/ack", self.api), None, None).await?;
        check_status(status, &bytes)
    }

    async fn handoff_status(&self, id: &str) -> Result<HandoffStatus, String> {
        let (status, bytes) = fetch_bytes("GET", &format!("{}/lease/handoff/{id}", self.api), &self.api_token, None, None, None).await?;
        check_status(status, &bytes)?;
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid handoff status: {e}"))
    }

    async fn cancel_handoff(&self, id: &str) -> Result<(), String> {
        let (status, bytes) = fetch_bytes("POST", &format!("{}/lease/handoff/{id}/cancel", self.api), &self.api_token, None, None, None).await?;
        check_status(status, &bytes)
    }

    async fn takeover(&self) -> Result<RemoteBackend, String> {
        let (status, bytes) = fetch_bytes("POST", &format!("{}/lease/takeover", self.api), &self.api_token, None, None, None).await?;
        check_status(status, &bytes)?;
        Self::from_lease_reply(self.api.clone(), self.api_token.clone(), &bytes)
    }
}

/// Did an OPFS call fail because the entry doesn't exist (`NotFoundError`)? Any other failure
/// (security, type mismatch, an unreadable or evicted store) is an error, never "absent" (#1507).
fn not_found(e: &JsValue) -> bool {
    crate::files::is_not_found(Reflect::get(e, &"name".into()).ok().and_then(|n| n.as_string()).as_deref())
}

/// `navigator.storage` in a window or a worker.
fn storage_manager() -> Option<web_sys::StorageManager> {
    let g = js_sys::global();
    let nav = Reflect::get(&g, &"navigator".into()).ok()?;
    let storage = Reflect::get(&nav, &"storage".into()).ok()?;
    (!storage.is_undefined()).then(|| storage.unchecked_into())
}

fn indexed_db() -> Option<web_sys::IdbFactory> {
    let f = Reflect::get(&js_sys::global(), &"indexedDB".into()).ok()?;
    (!f.is_undefined() && !f.is_null()).then(|| f.unchecked_into())
}

/// Can files be written through OPFS writable streams here?
fn opfs_writable() -> bool {
    let g = js_sys::global();
    Reflect::get(&g, &"FileSystemFileHandle".into())
        .ok()
        .filter(|c| !c.is_undefined())
        .and_then(|c| Reflect::get(&c, &"prototype".into()).ok())
        .and_then(|p| Reflect::get(&p, &"createWritable".into()).ok())
        .is_some_and(|f| f.is_function())
}

/// Resolve an IndexedDB request.
async fn idb_req(req: &web_sys::IdbRequest) -> Result<JsValue, String> {
    let r1 = req.clone();
    let p = js_sys::Promise::new(&mut |resolve, reject| {
        let r2 = r1.clone();
        let ok = Closure::once_into_js(move || {
            let _ = resolve.call1(&JsValue::NULL, &r2.result().unwrap_or(JsValue::UNDEFINED));
        });
        let fail = Closure::once_into_js(move |e: JsValue| {
            let _ = reject.call1(&JsValue::NULL, &e);
        });
        r1.set_onsuccess(Some(ok.unchecked_ref()));
        r1.set_onerror(Some(fail.unchecked_ref()));
    });
    JsFuture::from(p).await.map_err(err)
}

/// Wait for an IndexedDB transaction to commit.
async fn idb_done(tx: &web_sys::IdbTransaction) -> Result<(), String> {
    let t1 = tx.clone();
    let p = js_sys::Promise::new(&mut |resolve, reject| {
        let ok = Closure::once_into_js(move || {
            let _ = resolve.call0(&JsValue::NULL);
        });
        let rej = reject.clone();
        let fail = Closure::once_into_js(move |e: JsValue| {
            let _ = rej.call1(&JsValue::NULL, &e);
        });
        let abort = Closure::once_into_js(move |e: JsValue| {
            let _ = reject.call1(&JsValue::NULL, &e);
        });
        t1.set_oncomplete(Some(ok.unchecked_ref()));
        t1.set_onerror(Some(fail.unchecked_ref()));
        t1.set_onabort(Some(abort.unchecked_ref()));
    });
    JsFuture::from(p).await.map(|_| ()).map_err(err)
}

impl Backend {
    /// OPFS if writable from here (unless `prefer_idb`), else IndexedDB.
    pub async fn open(prefer_idb: bool) -> Result<Backend, String> {
        if !prefer_idb
            && opfs_writable()
            && let Some(sm) = storage_manager()
        {
            match JsFuture::from(sm.get_directory()).await {
                Ok(root) => return Ok(Backend::Opfs(root.unchecked_into())),
                Err(e) => log::warn!("OPFS unavailable ({}); using IndexedDB", err(e)),
            }
        }
        Self::open_idb().await
    }

    /// Open the experimental server backend and acquire the library's exclusive edit lease.
    pub async fn open_remote(base_url: &str, api_token: String) -> Result<Backend, String> {
        let api = Self::remote_api(base_url, &api_token)?;
        Ok(Backend::Remote(RemoteBackend::acquire(api, api_token).await?))
    }

    pub fn from_remote_lease(api: String, api_token: String, lease: String) -> Result<Backend, String> {
        if lease.is_empty() || api_token.len() < 32 {
            return Err("incomplete shared-library lease credentials".into());
        }
        Ok(Backend::Remote(RemoteBackend { api, api_token, lease }))
    }

    pub async fn request_remote_handoff(base_url: &str, api_token: String) -> Result<(String, String), String> {
        let api = Self::remote_api(base_url, &api_token)?;
        let remote = RemoteBackend { api: api.clone(), api_token, lease: String::new() };
        let id = remote.request_handoff().await?;
        Ok((api, id))
    }

    pub async fn remote_handoff_status(api: &str, api_token: &str, id: &str) -> Result<Option<String>, String> {
        let remote = RemoteBackend { api: api.to_string(), api_token: api_token.to_string(), lease: String::new() };
        let status = remote.handoff_status(id).await?;
        match status.state.as_str() {
            "pending" | "expired" => Ok(None),
            "complete" => status.lease.filter(|lease| !lease.is_empty()).map(Some).ok_or("handoff completed without a lease".into()),
            _ => Err(format!("unknown handoff state: {}", status.state)),
        }
    }

    pub async fn cancel_remote_handoff(api: &str, api_token: &str, id: &str) -> Result<(), String> {
        let remote = RemoteBackend { api: api.to_string(), api_token: api_token.to_string(), lease: String::new() };
        remote.cancel_handoff(id).await
    }

    pub async fn take_over_remote(base_url: &str, api_token: String) -> Result<Backend, String> {
        let api = Self::remote_api(base_url, &api_token)?;
        let remote = RemoteBackend { api, api_token, lease: String::new() };
        Ok(Backend::Remote(remote.takeover().await?))
    }

    pub fn handoff_context(&self) -> Result<(String, String), String> {
        match self {
            Backend::Remote(remote) => Ok((remote.api.clone(), remote.api_token.clone())),
            _ => Err("this library is not using the shared service".into()),
        }
    }

    pub async fn request_control_handoff(&self) -> Result<String, String> {
        match self {
            Backend::Remote(remote) => remote.request_handoff().await,
            _ => Err("this library is not using the shared service".into()),
        }
    }

    pub fn handoff_config(&self, lease: String) -> Result<String, String> {
        let Backend::Remote(remote) = self else { return Err("this library is not using the shared service".into()) };
        if lease.is_empty() {
            return Err("handoff returned an empty lease".into());
        }
        serde_json::to_string(&RemoteBackend { api: remote.api.clone(), api_token: remote.api_token.clone(), lease })
            .map_err(|e| format!("couldn't save handoff credentials: {e}"))
    }

    pub async fn force_takeover_config(&self) -> Result<String, String> {
        let Backend::Remote(remote) = self else { return Err("this library is not using the shared service".into()) };
        let new_lease = remote.takeover().await?;
        serde_json::to_string(&new_lease).map_err(|e| format!("couldn't save the new lease: {e}"))
    }

    pub fn open_handoff_config(base_url: &str, config: &str) -> Result<Backend, String> {
        let expected_api = format!("{}/v1", base_url.trim_end_matches('/'));
        let remote: RemoteBackend = serde_json::from_str(config).map_err(|e| format!("invalid handoff credentials: {e}"))?;
        if remote.api != expected_api || remote.api_token.len() < 32 || remote.lease.is_empty() {
            return Err("handoff credentials do not match the configured shared library".into());
        }
        Ok(Backend::Remote(remote))
    }

    fn remote_api(base_url: &str, api_token: &str) -> Result<String, String> {
        let base = base_url.trim_end_matches('/');
        let local_http = base.starts_with("http://127.0.0.1:") || base.starts_with("http://localhost:");
        if !(base.starts_with("https://") || local_http) {
            return Err("shared storage requires HTTPS, except for development on localhost".into());
        }
        if base.contains('?') || base.contains('#') || api_token.len() < 32 {
            return Err("invalid shared storage URL or access token".into());
        }
        Ok(format!("{base}/v1"))
    }

    /// Configuration sent only to same-origin render workers. Remote workers reuse this tab's
    /// lease; they must not acquire their own lease because the library is single-editor.
    pub fn worker_config(&self) -> Result<String, String> {
        match self {
            Backend::Remote(remote) => serde_json::to_string(remote).map_err(|e| format!("couldn't configure remote render worker: {e}")),
            Backend::Opfs(_) => Ok("opfs".into()),
            Backend::Idb(_) => Ok("idb".into()),
        }
    }

    /// Open a backend inside a render worker using the window's configuration and lease.
    pub async fn open_worker(config: &str) -> Result<Option<Backend>, String> {
        if config == "memory" {
            return Ok(None);
        }
        if config.starts_with('{') {
            let remote: RemoteBackend = serde_json::from_str(config).map_err(|e| format!("invalid shared worker configuration: {e}"))?;
            if !remote.api.starts_with("https://") && !remote.api.starts_with("http://127.0.0.1:") && !remote.api.starts_with("http://localhost:") {
                return Err("invalid shared worker service URL".into());
            }
            if remote.api_token.len() < 32 || remote.lease.is_empty() {
                return Err("incomplete shared worker credentials".into());
            }
            return Ok(Some(Backend::Remote(remote)));
        }
        match config {
            "opfs" => Self::open(false).await.map(Some),
            "idb" => Self::open(true).await.map(Some),
            _ => Err("unknown browser storage configuration for render worker".into()),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Backend::Remote(_))
    }

    pub async fn renew_lease(&self) -> Result<(), String> {
        match self {
            Backend::Remote(remote) => remote.renew().await,
            _ => Ok(()),
        }
    }

    pub async fn pending_handoff(&self) -> Result<Option<String>, String> {
        match self {
            Backend::Remote(remote) => remote.pending_handoff().await,
            _ => Ok(None),
        }
    }

    pub async fn acknowledge_handoff(&self, id: &str) -> Result<(), String> {
        match self {
            Backend::Remote(remote) => remote.acknowledge_handoff(id).await,
            _ => Ok(()),
        }
    }

    pub async fn release_lease(&self) -> Result<(), String> {
        match self {
            Backend::Remote(remote) => remote.release().await,
            _ => Ok(()),
        }
    }

    async fn open_idb() -> Result<Backend, String> {
        let f = indexed_db().ok_or("no IndexedDB")?;
        let req = f.open_with_u32(IDB_NAME, 1).map_err(err)?;
        let upgrade = Closure::<dyn FnMut(web_sys::IdbVersionChangeEvent)>::new(move |e: web_sys::IdbVersionChangeEvent| {
            if let Some(db) = e.target().and_then(|t| t.dyn_into::<web_sys::IdbOpenDbRequest>().ok()).and_then(|r| r.result().ok()) {
                let db: IdbDatabase = db.unchecked_into();
                if !db.object_store_names().contains(IDB_STORE) {
                    let _ = db.create_object_store(IDB_STORE);
                }
            }
        });
        req.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));
        let db = idb_req(&req).await?;
        drop(upgrade);
        Ok(Backend::Idb(db.unchecked_into()))
    }

    /// "opfs" or "idb" (workers open the same kind).
    pub fn kind(&self) -> &'static str {
        match self {
            Backend::Opfs(_) => "opfs",
            Backend::Idb(_) => "idb",
            Backend::Remote(_) => "remote",
        }
    }

    /// The directory holding `path`'s last component (created when `create`).
    async fn parent(root: &FileSystemDirectoryHandle, path: &str, create: bool) -> Result<Option<(FileSystemDirectoryHandle, String)>, String> {
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let name = parts.pop().ok_or("empty path")?.to_string();
        let mut dir = root.clone();
        for p in parts {
            let opts = FileSystemGetDirectoryOptions::new();
            opts.set_create(create);
            match JsFuture::from(dir.get_directory_handle_with_options(p, &opts)).await {
                Ok(d) => dir = d.unchecked_into(),
                Err(e) if !create && not_found(&e) => return Ok(None),
                Err(e) => return Err(err(e)),
            }
        }
        Ok(Some((dir, name)))
    }

    async fn opfs_file(root: &FileSystemDirectoryHandle, path: &str, create: bool) -> Result<Option<FileSystemFileHandle>, String> {
        let Some((dir, name)) = Self::parent(root, path, create).await? else { return Ok(None) };
        let opts = FileSystemGetFileOptions::new();
        opts.set_create(create);
        match JsFuture::from(dir.get_file_handle_with_options(&name, &opts)).await {
            Ok(h) => Ok(Some(h.unchecked_into())),
            Err(e) if !create && not_found(&e) => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    fn idb_store(db: &IdbDatabase, write: bool) -> Result<(web_sys::IdbTransaction, web_sys::IdbObjectStore), String> {
        let mode = if write { web_sys::IdbTransactionMode::Readwrite } else { web_sys::IdbTransactionMode::Readonly };
        let tx = db.transaction_with_str_and_mode(IDB_STORE, mode).map_err(err)?;
        let store = tx.object_store(IDB_STORE).map_err(err)?;
        Ok((tx, store))
    }

    /// A whole file, or `None` if it doesn't exist.
    pub async fn read(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            Backend::Remote(remote) => {
                let (status, bytes) = remote.request("GET", &remote.path_url(path), None, None).await?;
                if status == 404 {
                    Ok(None)
                } else {
                    check_status(status, &bytes)?;
                    Ok(Some(bytes))
                }
            }
            Backend::Opfs(root) => {
                let Some(h) = Self::opfs_file(root, path, false).await? else { return Ok(None) };
                let file: web_sys::File = JsFuture::from(h.get_file()).await.map_err(err)?.unchecked_into();
                let buf = JsFuture::from(file.array_buffer()).await.map_err(err)?;
                Ok(Some(Uint8Array::new(&buf).to_vec()))
            }
            Backend::Idb(db) => {
                let (_tx, store) = Self::idb_store(db, false)?;
                let v = idb_req(&store.get(&path.into()).map_err(err)?).await?;
                if v.is_undefined() || v.is_null() {
                    return Ok(None);
                }
                Ok(Some(Uint8Array::new(&v).to_vec()))
            }
        }
    }

    /// Replace a file atomically (OPFS: the writable's swap file is committed on close).
    pub async fn write(&self, path: &str, data: &[u8]) -> Result<(), String> {
        self.write_at(path, None, data).await
    }

    /// Write `data` at `offset` keeping the bytes before it (`None`: replace the file). Atomic.
    pub async fn write_at(&self, path: &str, offset: Option<usize>, data: &[u8]) -> Result<(), String> {
        match self {
            Backend::Remote(remote) => {
                let (method, offset) = match offset {
                    Some(offset) => ("PATCH", Some(offset)),
                    None => ("PUT", None),
                };
                let (status, bytes) = remote.request(method, &remote.path_url(path), offset, Some(data)).await?;
                check_status(status, &bytes)
            }
            Backend::Opfs(root) => {
                let h = Self::opfs_file(root, path, true).await?.ok_or("can't create file")?;
                let opts = web_sys::FileSystemCreateWritableOptions::new();
                opts.set_keep_existing_data(offset.is_some());
                let w: web_sys::FileSystemWritableFileStream =
                    JsFuture::from(h.create_writable_with_options(&opts)).await.map_err(err)?.unchecked_into();
                let res = async {
                    if let Some(off) = offset {
                        JsFuture::from(w.truncate_with_f64(off as f64).map_err(err)?).await.map_err(err)?;
                        JsFuture::from(w.seek_with_f64(off as f64).map_err(err)?).await.map_err(err)?;
                    }
                    JsFuture::from(w.write_with_js_u8_array(&Uint8Array::from(data)).map_err(err)?).await.map_err(err)?;
                    Ok::<(), String>(())
                }
                .await;
                match res {
                    Ok(()) => JsFuture::from(w.close()).await.map(|_| ()).map_err(err),
                    Err(e) => {
                        let _ = JsFuture::from(w.abort()).await;
                        Err(e)
                    }
                }
            }
            Backend::Idb(db) => {
                let (tx, store) = Self::idb_store(db, true)?;
                let value = match offset {
                    // read-modify-write inside one transaction
                    Some(off) => {
                        let old = idb_req(&store.get(&path.into()).map_err(err)?).await?;
                        let mut v = if old.is_undefined() || old.is_null() { Vec::new() } else { Uint8Array::new(&old).to_vec() };
                        v.truncate(off);
                        v.extend_from_slice(data);
                        Uint8Array::from(&v[..])
                    }
                    None => Uint8Array::from(data),
                };
                store.put_with_key(&value, &path.into()).map_err(err)?;
                idb_done(&tx).await
            }
        }
    }

    /// Is there a file at `path`?
    pub async fn exists(&self, path: &str) -> Result<bool, String> {
        match self {
            Backend::Remote(remote) => {
                let (status, bytes) = remote.request("HEAD", &remote.path_url(path), None, None).await?;
                if status == 404 {
                    Ok(false)
                } else {
                    check_status(status, &bytes)?;
                    Ok(true)
                }
            }
            Backend::Opfs(root) => Ok(Self::opfs_file(root, path, false).await?.is_some()),
            Backend::Idb(db) => {
                let (_tx, store) = Self::idb_store(db, false)?;
                let n = idb_req(&store.count_with_key(&path.into()).map_err(err)?).await?;
                Ok(n.as_f64().is_some_and(|n| n > 0.0))
            }
        }
    }

    pub async fn remove(&self, path: &str) -> Result<(), String> {
        match self {
            Backend::Remote(remote) => {
                let (status, bytes) = remote.request("DELETE", &remote.path_url(path), None, None).await?;
                check_status(status, &bytes)
            }
            Backend::Opfs(root) => {
                let Some((dir, name)) = Self::parent(root, path, false).await? else { return Ok(()) };
                match JsFuture::from(dir.remove_entry(&name)).await {
                    Ok(_) => Ok(()),
                    Err(e) if not_found(&e) => Ok(()),
                    Err(e) => Err(err(e)),
                }
            }
            Backend::Idb(db) => {
                let (tx, store) = Self::idb_store(db, true)?;
                store.delete(&path.into()).map_err(err)?;
                idb_done(&tx).await
            }
        }
    }

    /// Names of the files directly inside `dir`.
    pub async fn list(&self, dir: &str) -> Result<Vec<String>, String> {
        match self {
            Backend::Remote(remote) => {
                let url = format!("{}/list?path={}", remote.api, encode_component(dir));
                let (status, bytes) = remote.request("GET", &url, None, None).await?;
                check_status(status, &bytes)?;
                serde_json::from_slice(&bytes).map_err(|e| format!("invalid shared storage directory listing: {e}"))
            }
            Backend::Opfs(root) => {
                // `parent` of a child path is `dir` itself
                let Some((handle, _)) = Self::parent(root, &format!("{dir}/-"), false).await? else { return Ok(Vec::new()) };
                dir_keys(&handle).await
            }
            Backend::Idb(db) => {
                let (_tx, store) = Self::idb_store(db, false)?;
                let prefix = format!("{dir}/");
                let range = web_sys::IdbKeyRange::bound(&prefix.as_str().into(), &format!("{prefix}\u{ffff}").into()).map_err(err)?;
                let keys = idb_req(&store.get_all_keys_with_key(&range).map_err(err)?).await?;
                let arr: js_sys::Array = keys.unchecked_into();
                Ok(arr
                    .iter()
                    .filter_map(|k| k.as_string())
                    .filter_map(|k| k.strip_prefix(&prefix).filter(|r| !r.contains('/')).map(str::to_string))
                    .collect())
            }
        }
    }

    /// Delete everything (the "reset" escape hatch: `?reset` in the URL).
    pub async fn clear(&self) -> Result<(), String> {
        match self {
            Backend::Remote(_) => Err("clearing a shared library from the browser is disabled".into()),
            Backend::Opfs(root) => {
                for name in dir_keys(root).await? {
                    let opts = web_sys::FileSystemRemoveOptions::new();
                    opts.set_recursive(true);
                    JsFuture::from(root.remove_entry_with_options(&name, &opts)).await.map_err(err)?;
                }
                Ok(())
            }
            Backend::Idb(db) => {
                let (tx, store) = Self::idb_store(db, true)?;
                store.clear().map_err(err)?;
                idb_done(&tx).await
            }
        }
    }
}

/// Names of the entries in an OPFS directory.
async fn dir_keys(dir: &FileSystemDirectoryHandle) -> Result<Vec<String>, String> {
    let keys = dir.keys();
    let mut out = Vec::new();
    loop {
        let next = JsFuture::from(keys.next().map_err(err)?).await.map_err(err)?;
        if Reflect::get(&next, &"done".into()).ok().is_some_and(|d| d.is_truthy()) {
            break;
        }
        if let Some(k) = Reflect::get(&next, &"value".into()).ok().and_then(|v| v.as_string()) {
            out.push(k);
        }
    }
    Ok(out)
}

/// Ask the browser not to evict our storage under pressure; `on_result(granted)` once known
/// (`false` also when the browser has no storage manager).
pub fn request_persistence(on_result: impl FnOnce(bool) + 'static) {
    let Some(sm) = storage_manager() else {
        on_result(false);
        return;
    };
    wasm_bindgen_futures::spawn_local(async move {
        // already persistent: nothing to ask
        let already = match sm.persisted() {
            Ok(p) => JsFuture::from(p).await.ok().is_some_and(|v| v.is_truthy()),
            Err(_) => false,
        };
        let granted = already
            || match sm.persist() {
                Ok(p) => JsFuture::from(p).await.ok().is_some_and(|v| v.is_truthy()),
                Err(_) => false,
            };
        log::info!("lightcraft: persistent storage {}", if granted { "granted" } else { "not granted" });
        on_result(granted);
    });
}
