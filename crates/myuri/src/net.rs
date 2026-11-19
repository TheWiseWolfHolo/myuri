//! Streaming HTTP through WinHTTP: the system's TLS, proxy settings and
//! connection pool, with nothing extra to ship. Each request runs on its own
//! thread and reports [`llm::Event`]s as the reply arrives.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows::Win32::Networking::WinHttp::{
    URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD,
    WINHTTP_FLAG_SECURE, WINHTTP_INTERNET_SCHEME_HTTPS, WINHTTP_OPEN_REQUEST_FLAGS,
    WINHTTP_OPTION_DECOMPRESSION, WINHTTP_OPTION_ENABLE_HTTP_PROTOCOL, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect,
    WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::{PCWSTR, w};

use myuri_core::config::Api;
use myuri_core::llm::{self, Decoder, Event, HttpRequest};

use crate::win::wide;

/// Stops a request in flight. Dropping it does not stop the request.
#[derive(Clone)]
pub struct Abort {
    handle: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
}

impl Abort {
    pub fn abort(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        close(&self.handle);
    }
}

/// How a request ended when it did not fail outright.
enum Outcome {
    Finished,
    /// The server answered with an error status.
    Rejected(u16, String),
}

/// Close the request handle exactly once, whoever gets there first. Closing
/// it makes any call blocked on it return with an error.
fn close(slot: &AtomicUsize) {
    let handle = slot.swap(0, Ordering::SeqCst);
    if handle != 0 {
        unsafe {
            let _ = WinHttpCloseHandle(handle as *mut c_void);
        }
    }
}

/// One session for the whole process, so connections are reused.
fn session() -> *mut c_void {
    static SESSION: OnceLock<usize> = OnceLock::new();
    *SESSION.get_or_init(|| unsafe {
        let session = WinHttpOpen(
            w!("Myuri"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        // HTTP/2 and gzip are both optional extras; older systems lack them.
        let _ = WinHttpSetOption(Some(session), WINHTTP_OPTION_ENABLE_HTTP_PROTOCOL, Some(&1u32.to_le_bytes()));
        let _ = WinHttpSetOption(Some(session), WINHTTP_OPTION_DECOMPRESSION, Some(&3u32.to_le_bytes()));
        session as usize
    }) as *mut c_void
}

/// One way of making a request: with the reasoning setting, and without it
/// for models that refuse the setting.
pub struct Attempt {
    pub request: HttpRequest,
    pub plain: Option<HttpRequest>,
}

/// Send a request on a worker thread and report the reply as it streams.
/// `attempts` are tried in order: the next one only when a key is refused or
/// the provider cannot be reached, and nothing of a reply has been shown.
/// The last event is always `Done` or `Error`.
pub fn stream(
    attempts: Vec<Attempt>,
    api: Api,
    mut on_event: impl FnMut(Event) + Send + 'static,
) -> Abort {
    let abort = Abort {
        handle: Arc::new(AtomicUsize::new(0)),
        stopped: Arc::new(AtomicBool::new(false)),
    };
    let (slot, stopped) = (abort.handle.clone(), abort.stopped.clone());
    let spawned = std::thread::Builder::new().name("myuri-net".into()).spawn(move || {
        let mut outcome = Err("供应商没有可用的密钥".to_string());
        let mut shown = false;
        let last = attempts.len().saturating_sub(1);
        for (index, attempt) in attempts.iter().enumerate() {
            let mut report = |event| {
                shown = true;
                on_event(event);
            };
            outcome = run(&attempt.request, api, &slot, &mut report);
            if let (Ok(Outcome::Rejected(status, body)), Some(plain)) = (&outcome, &attempt.plain) {
                if llm::rejects_reasoning(*status, body) && !stopped.load(Ordering::SeqCst) {
                    close(&slot);
                    outcome = run(plain, api, &slot, &mut report);
                }
            }
            close(&slot);
            let retry = match &outcome {
                Ok(Outcome::Finished) => false,
                Ok(Outcome::Rejected(status, _)) => llm::another_key_may_work(*status),
                Err(_) => true,
            };
            if !retry || shown || index == last || stopped.load(Ordering::SeqCst) {
                break;
            }
        }
        on_event(match outcome {
            // A request the user stopped ends quietly.
            _ if stopped.load(Ordering::SeqCst) => Event::Done,
            Ok(Outcome::Finished) => return,
            Ok(Outcome::Rejected(status, body)) => Event::Error(llm::describe_failure(status, &body)),
            Err(reason) => Event::Error(reason),
        });
    });
    if spawned.is_err() {
        abort.abort();
    }
    abort
}

fn run(
    request: &HttpRequest,
    api: Api,
    slot: &AtomicUsize,
    on_event: &mut impl FnMut(Event),
) -> Result<Outcome, String> {
    let mut decoder = Decoder::new(api);
    let mut failure = Vec::new();
    let body = request.body.to_string().into_bytes();
    let status = exchange(w!("POST"), &request.url, &request.headers, &body, slot, &mut |status, bytes| {
        if (200..300).contains(&status) {
            decoder.feed(bytes).into_iter().for_each(&mut *on_event);
        } else if failure.len() < 64 * 1024 {
            failure.extend_from_slice(bytes);
        }
    })?;
    if (200..300).contains(&status) {
        decoder.finish().into_iter().for_each(&mut *on_event);
        Ok(Outcome::Finished)
    } else {
        Ok(Outcome::Rejected(status, String::from_utf8_lossy(&failure).into_owned()))
    }
}

/// Send a document and return the status and the reply. Blocks.
pub fn post(url: &str, headers: &[(String, String)], body: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let slot = AtomicUsize::new(0);
    let mut reply = Vec::new();
    let status =
        exchange(w!("POST"), url, headers, body, &slot, &mut |_, bytes| reply.extend_from_slice(bytes));
    close(&slot);
    Ok((status?, reply))
}

/// Store a document at `url`, replacing what is there. Blocks.
pub fn put(url: &str, headers: &[(String, String)], body: &[u8]) -> Result<(), String> {
    let slot = AtomicUsize::new(0);
    let mut reply = Vec::new();
    let status =
        exchange(w!("PUT"), url, headers, body, &slot, &mut |_, bytes| reply.extend_from_slice(bytes));
    close(&slot);
    match status? {
        200..=299 => Ok(()),
        status => Err(llm::describe_failure(status, &String::from_utf8_lossy(&reply))),
    }
}

/// Delete a document. Best effort cleanup; the reply body is ignored.
pub fn delete(url: &str, headers: &[(String, String)]) -> Result<(), String> {
    let slot = AtomicUsize::new(0);
    let mut body = Vec::new();
    let status =
        exchange(w!("DELETE"), url, headers, &[], &slot, &mut |_, bytes| body.extend_from_slice(bytes));
    close(&slot);
    match status? {
        200..=299 => Ok(()),
        status => Err(llm::describe_failure(status, &String::from_utf8_lossy(&body))),
    }
}

/// Fetch a document. Blocks until it has arrived.
pub fn get(url: &str, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
    let slot = AtomicUsize::new(0);
    let mut body = Vec::new();
    let status =
        exchange(w!("GET"), url, headers, &[], &slot, &mut |_, bytes| body.extend_from_slice(bytes));
    close(&slot);
    match status? {
        200..=299 => Ok(body),
        status => Err(llm::describe_failure(status, &String::from_utf8_lossy(&body))),
    }
}

/// One request and its response. `on_bytes` gets the status along with every
/// piece of the body. The request handle is left in `slot` for the caller to
/// close.
fn exchange(
    method: PCWSTR,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    slot: &AtomicUsize,
    on_bytes: &mut dyn FnMut(u16, &[u8]),
) -> Result<u16, String> {
    let network = |error: windows::core::Error| format!("连不上：{}", error.message().trim());
    unsafe {
        let wide_url = wide(url);
        let mut host = [0u16; 256];
        let mut path = [0u16; 2048];
        let mut parts = URL_COMPONENTS {
            dwStructSize: size_of::<URL_COMPONENTS>() as u32,
            lpszHostName: windows::core::PWSTR(host.as_mut_ptr()),
            dwHostNameLength: host.len() as u32,
            lpszUrlPath: windows::core::PWSTR(path.as_mut_ptr()),
            dwUrlPathLength: path.len() as u32,
            ..Default::default()
        };
        WinHttpCrackUrl(&wide_url[..wide_url.len() - 1], 0, &mut parts)
            .map_err(|_| format!("地址写得不对：{url}"))?;
        let secure = parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;

        let connection = WinHttpConnect(session(), PCWSTR(host.as_ptr()), parts.nPort, 0);
        if connection.is_null() {
            return Err(network(windows::core::Error::from_win32()));
        }
        let handle = WinHttpOpenRequest(
            connection,
            method,
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) },
        );
        // The request keeps the connection alive on its own.
        let _ = WinHttpCloseHandle(connection);
        if handle.is_null() {
            return Err(network(windows::core::Error::from_win32()));
        }
        slot.store(handle as usize, Ordering::SeqCst);

        // Resolve, connect, send, and the longest silence allowed mid-reply.
        let _ = WinHttpSetTimeouts(handle, 10_000, 10_000, 30_000, 300_000);
        if !headers.is_empty() {
            let lines: String = headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
            let lines = wide(&lines);
            WinHttpAddRequestHeaders(handle, &lines[..lines.len() - 1], WINHTTP_ADDREQ_FLAG_ADD)
                .map_err(network)?;
        }
        let payload = (!body.is_empty()).then(|| body.as_ptr().cast());
        WinHttpSendRequest(handle, None, payload, body.len() as u32, body.len() as u32, 0)
            .map_err(network)?;
        WinHttpReceiveResponse(handle, std::ptr::null_mut()).map_err(network)?;

        let mut status = 0u32;
        let mut size = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            handle,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(network)?;
        let status = status as u16;

        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            // Returns as soon as anything has arrived, which is what keeps
            // a reply flowing word by word.
            let mut available = 0;
            WinHttpQueryDataAvailable(handle, &mut available).map_err(network)?;
            if available == 0 {
                break;
            }
            let mut read = 0;
            let want = available.min(buffer.len() as u32);
            WinHttpReadData(handle, buffer.as_mut_ptr().cast(), want, &mut read).map_err(network)?;
            if read == 0 {
                break;
            }
            on_bytes(status, &buffer[..read as usize]);
        }
        Ok(status)
    }
}
