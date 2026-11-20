//! Streaming HTTP through WinHTTP: the system's TLS, proxy settings and
//! connection pool, with nothing extra to ship. Each request runs on its own
//! thread and reports [`llm::Event`]s as the reply arrives.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use windows::Win32::Networking::WinHttp::{
    URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD,
    WINHTTP_ASYNC_RESULT, WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE,
    WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING, WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE,
    WINHTTP_CALLBACK_STATUS_READ_COMPLETE, WINHTTP_CALLBACK_STATUS_REQUEST_ERROR,
    WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE, WINHTTP_FLAG_ASYNC, WINHTTP_FLAG_SECURE,
    WINHTTP_INTERNET_SCHEME_HTTPS, WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_OPTION_CONTEXT_VALUE,
    WINHTTP_OPTION_DECOMPRESSION, WINHTTP_OPTION_ENABLE_HTTP_PROTOCOL, WINHTTP_QUERY_CUSTOM,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders,
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetStatusCallback, WinHttpSetTimeouts,
};
use windows::core::{Error, HRESULT, PCWSTR, w};

use myuri_core::config::Api;
use myuri_core::llm::{self, Decoder, Event, HttpRequest};

use crate::win::wide;

/// Stops a request in flight. Dropping it does not stop the request.
#[derive(Clone)]
pub struct Abort {
    stopped: Arc<AtomicBool>,
}

impl Abort {
    pub fn new() -> Self {
        Self::with_flag(Arc::new(AtomicBool::new(false)))
    }

    pub fn with_flag(stopped: Arc<AtomicBool>) -> Self {
        Self { stopped }
    }

    pub fn abort(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        self.collect(w!("POST"), url, headers, body)
    }

    pub fn get(&self, url: &str, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
        let (status, body) = self.collect(w!("GET"), url, headers, &[])?;
        match status {
            200..=299 => Ok(body),
            status => Err(llm::describe_failure(
                status,
                &String::from_utf8_lossy(&body),
            )),
        }
    }

    fn collect(
        &self,
        method: PCWSTR,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        let mut reply = Vec::new();
        let (status, _) = exchange(
            method,
            url,
            headers,
            body.to_vec(),
            Some(self),
            &mut |_, bytes| {
                reply.extend_from_slice(bytes);
            },
        )?;
        Ok((status, reply))
    }
}

/// How a request ended when it did not fail outright.
enum Outcome {
    Finished,
    /// The server answered with an error status.
    Rejected(u16, String),
}

struct Request {
    handle: *mut c_void,
    connection: *mut c_void,
}

impl Drop for Request {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.handle);
            let _ = WinHttpCloseHandle(self.connection);
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
            WINHTTP_FLAG_ASYNC,
        );
        // HTTP/2 and gzip are both optional extras; older systems lack them.
        let _ = WinHttpSetOption(
            Some(session),
            WINHTTP_OPTION_ENABLE_HTTP_PROTOCOL,
            Some(&1u32.to_le_bytes()),
        );
        let _ = WinHttpSetOption(
            Some(session),
            WINHTTP_OPTION_DECOMPRESSION,
            Some(&3u32.to_le_bytes()),
        );
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
/// `prepare` resolves any credentials needed by the requests on the worker.
/// The last event is always `Done` or `Error`.
pub fn stream(
    mut attempts: Vec<Attempt>,
    api: Api,
    prepare: impl FnOnce(&mut [Attempt]) -> Result<(), String> + Send + 'static,
    mut on_event: impl FnMut(Event) + Send + 'static,
) -> Abort {
    let abort = Abort::new();
    let worker = abort.clone();
    let spawned = std::thread::Builder::new()
        .name("myuri-net".into())
        .spawn(move || {
            if worker.is_stopped() {
                on_event(Event::Done);
                return;
            }
            if let Err(reason) = prepare(&mut attempts) {
                on_event(if worker.is_stopped() {
                    Event::Done
                } else {
                    Event::Error(reason)
                });
                return;
            }
            let mut outcome = Err("供应商没有可用的密钥".to_string());
            let mut shown = false;
            let last = attempts.len().saturating_sub(1);
            for (index, attempt) in attempts.iter().enumerate() {
                let mut report = |event| {
                    shown = true;
                    on_event(event);
                };
                outcome = run(&attempt.request, api, &worker, &mut report);
                if let (Ok(Outcome::Rejected(status, body)), Some(plain)) =
                    (&outcome, &attempt.plain)
                    && llm::rejects_reasoning(*status, body)
                    && !worker.is_stopped()
                {
                    outcome = run(plain, api, &worker, &mut report);
                }
                let retry = match &outcome {
                    Ok(Outcome::Finished) => false,
                    Ok(Outcome::Rejected(status, _)) => llm::another_key_may_work(*status),
                    Err(_) => true,
                };
                if !retry || shown || index == last || worker.is_stopped() {
                    break;
                }
            }
            on_event(match outcome {
                // A request the user stopped ends quietly.
                _ if worker.is_stopped() => Event::Done,
                Ok(Outcome::Finished) => return,
                Ok(Outcome::Rejected(status, body)) => {
                    Event::Error(llm::describe_failure(status, &body))
                }
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
    abort: &Abort,
    on_event: &mut impl FnMut(Event),
) -> Result<Outcome, String> {
    let mut decoder = Decoder::new(api);
    let mut failure = Vec::new();
    let body = request.body.to_string().into_bytes();
    let (status, _) = exchange(
        w!("POST"),
        &request.url,
        &request.headers,
        body,
        Some(abort),
        &mut |status, bytes| {
            if (200..300).contains(&status) {
                decoder.feed(bytes).into_iter().for_each(&mut *on_event);
            } else if failure.len() < 64 * 1024 {
                failure.extend_from_slice(bytes);
            }
        },
    )?;
    if (200..300).contains(&status) {
        decoder.finish().into_iter().for_each(&mut *on_event);
        Ok(Outcome::Finished)
    } else {
        Ok(Outcome::Rejected(
            status,
            String::from_utf8_lossy(&failure).into_owned(),
        ))
    }
}

/// Send a document and return the status and the reply. Blocks.
pub fn post(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let (status, reply, _) = post_for(url, headers, body, "")?;
    Ok((status, reply))
}

/// Like [`post`], and also returns the response header `wanted` if the
/// server sent it.
pub fn post_for(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    wanted: &str,
) -> Result<(u16, Vec<u8>, Option<String>), String> {
    let mut reply = Vec::new();
    let (status, request) = exchange(
        w!("POST"),
        url,
        headers,
        body.to_vec(),
        None,
        &mut |_, bytes| {
            reply.extend_from_slice(bytes);
        },
    )?;
    let found = (!wanted.is_empty())
        .then(|| header(&request, wanted))
        .flatten();
    Ok((status, reply, found))
}

fn header(request: &Request, name: &str) -> Option<String> {
    let name = wide(name);
    let mut buffer = [0u16; 512];
    let mut size = size_of_val(&buffer) as u32;
    unsafe {
        WinHttpQueryHeaders(
            request.handle,
            WINHTTP_QUERY_CUSTOM,
            PCWSTR(name.as_ptr()),
            Some(buffer.as_mut_ptr().cast()),
            &mut size,
            std::ptr::null_mut(),
        )
        .ok()?;
    }
    let length = (size as usize / 2).min(buffer.len());
    Some(
        String::from_utf16_lossy(&buffer[..length])
            .trim()
            .to_string(),
    )
    .filter(|value| !value.is_empty())
}

/// Store a document at `url`, replacing what is there. Blocks.
pub fn put(url: &str, headers: &[(String, String)], body: &[u8]) -> Result<(), String> {
    document(w!("PUT"), url, headers, body.to_vec()).map(|_| ())
}

/// Delete a document. Best effort cleanup; the reply body is ignored.
pub fn delete(url: &str, headers: &[(String, String)]) -> Result<(), String> {
    document(w!("DELETE"), url, headers, Vec::new()).map(|_| ())
}

fn document(
    method: PCWSTR,
    url: &str,
    headers: &[(String, String)],
    payload: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let (status, _) = exchange(method, url, headers, payload, None, &mut |_, bytes| {
        body.extend_from_slice(bytes);
    })?;
    match status {
        200..=299 => Ok(body),
        status => Err(llm::describe_failure(
            status,
            &String::from_utf8_lossy(&body),
        )),
    }
}

/// Fetch a document and return the status and the reply. Blocks.
pub fn get_status(url: &str, headers: &[(String, String)]) -> Result<(u16, Vec<u8>), String> {
    let mut body = Vec::new();
    let (status, _) = exchange(
        w!("GET"),
        url,
        headers,
        Vec::new(),
        None,
        &mut |_, bytes| {
            body.extend_from_slice(bytes);
        },
    )?;
    Ok((status, body))
}

/// Fetch a document. Blocks until it has arrived.
pub fn get(url: &str, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
    document(w!("GET"), url, headers, Vec::new())
}

struct ExchangeState {
    body: Vec<u8>,
    buffer: [u8; 16 * 1024],
    events: SyncSender<(u32, u32)>,
}

unsafe extern "system" fn completed(
    _handle: *mut c_void,
    context: usize,
    status: u32,
    info: *mut c_void,
    length: u32,
) {
    let state = context as *mut ExchangeState;
    if state.is_null() {
        return;
    }
    unsafe {
        // WinHTTP sends the final callback after all notifications and I/O.
        if status == WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING {
            drop(Box::from_raw(state));
            return;
        }
        let value = match status {
            WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE => info.cast::<u32>().read_unaligned(),
            WINHTTP_CALLBACK_STATUS_REQUEST_ERROR => {
                info.cast::<WINHTTP_ASYNC_RESULT>().read_unaligned().dwError
            }
            WINHTTP_CALLBACK_STATUS_READ_COMPLETE => length,
            WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE
            | WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE => 0,
            _ => return,
        };
        let _ = (*state).events.try_send((status, value));
    }
}

fn wait_for(
    events: &Receiver<(u32, u32)>,
    expected: u32,
    abort: Option<&Abort>,
) -> Result<u32, String> {
    let (status, value) = loop {
        if abort.is_some_and(Abort::is_stopped) {
            return Err("请求已停止".into());
        }
        let received = if abort.is_some() {
            events.recv_timeout(Duration::from_millis(40))
        } else {
            events.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match received {
            Ok(event) => break event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Err("WinHTTP 请求已关闭".into()),
        }
    };
    if abort.is_some_and(Abort::is_stopped) {
        return Err("请求已停止".into());
    }
    match status {
        WINHTTP_CALLBACK_STATUS_REQUEST_ERROR => {
            Err(network(Error::from_hresult(HRESULT::from_win32(value))))
        }
        status if status == expected => Ok(value),
        _ => Err("WinHTTP 返回了意外的请求状态".into()),
    }
}

fn network(error: Error) -> String {
    format!("连不上：{}", error.message().trim())
}

fn open_request(
    method: PCWSTR,
    url: &str,
    headers: &[(String, String)],
) -> Result<Request, String> {
    unsafe {
        let wide_url = wide(url);
        let mut host = [0u16; 256];
        let mut path = vec![0u16; wide_url.len()];
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
            return Err(network(Error::from_win32()));
        }
        let handle = WinHttpOpenRequest(
            connection,
            method,
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if secure {
                WINHTTP_FLAG_SECURE
            } else {
                WINHTTP_OPEN_REQUEST_FLAGS(0)
            },
        );
        if handle.is_null() {
            let error = Error::from_win32();
            let _ = WinHttpCloseHandle(connection);
            return Err(network(error));
        }
        let request = Request { handle, connection };
        // Resolve, connect, send, and the longest silence allowed mid-reply.
        let _ = WinHttpSetTimeouts(handle, 10_000, 10_000, 30_000, 300_000);
        if !headers.is_empty() {
            let lines: String = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let lines = wide(&lines);
            WinHttpAddRequestHeaders(handle, &lines[..lines.len() - 1], WINHTTP_ADDREQ_FLAG_ADD)
                .map_err(network)?;
        }
        Ok(request)
    }
}

/// The worker alone uses and closes the handle. WinHTTP owns the heap-backed
/// request body and read buffer until its final HANDLE_CLOSING notification.
fn exchange(
    method: PCWSTR,
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    abort: Option<&Abort>,
    on_bytes: &mut dyn FnMut(u16, &[u8]),
) -> Result<(u16, Request), String> {
    if abort.is_some_and(Abort::is_stopped) {
        return Err("请求已停止".into());
    }
    let request = open_request(method, url, headers)?;
    let handle = request.handle;
    let length = u32::try_from(body.len()).map_err(|_| "HTTP 请求体太大")?;
    let (send, events) = mpsc::sync_channel(16);
    let state = Box::into_raw(Box::new(ExchangeState {
        body,
        buffer: [0; 16 * 1024],
        events: send,
    }));
    let context = state as usize;
    unsafe {
        if let Err(error) = WinHttpSetOption(
            Some(handle),
            WINHTTP_OPTION_CONTEXT_VALUE,
            Some(&context.to_ne_bytes()),
        ) {
            drop(Box::from_raw(state));
            return Err(network(error));
        }
        let flags = WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE
            | WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE
            | WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE
            | WINHTTP_CALLBACK_STATUS_READ_COMPLETE
            | WINHTTP_CALLBACK_STATUS_REQUEST_ERROR
            | WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING;
        let previous = WinHttpSetStatusCallback(handle, Some(completed), flags, 0);
        if previous.is_some_and(|callback| callback as usize == usize::MAX) {
            let error = Error::from_win32();
            drop(Box::from_raw(state));
            return Err(network(error));
        }

        let payload = (length != 0).then(|| (*state).body.as_ptr().cast());
        WinHttpSendRequest(handle, None, payload, length, length, context).map_err(network)?;
        wait_for(&events, WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE, abort)?;
        WinHttpReceiveResponse(handle, std::ptr::null_mut()).map_err(network)?;
        wait_for(&events, WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE, abort)?;

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
        let buffer = (&raw mut (*state).buffer).cast::<u8>();
        loop {
            // Query first so streaming callers get partial SSE chunks promptly.
            WinHttpQueryDataAvailable(handle, std::ptr::null_mut()).map_err(network)?;
            let available = wait_for(&events, WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE, abort)?;
            if available == 0 {
                break;
            }
            let want = available.min(16 * 1024);
            WinHttpReadData(handle, buffer.cast(), want, std::ptr::null_mut()).map_err(network)?;
            let read = wait_for(&events, WINHTTP_CALLBACK_STATUS_READ_COMPLETE, abort)?;
            if read == 0 {
                break;
            }
            on_bytes(status, std::slice::from_raw_parts(buffer, read as usize));
        }
        Ok((status, request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    const WAIT: Duration = Duration::from_secs(5);

    fn serve(listener: TcpListener) -> (BufReader<TcpStream>, String, usize) {
        let (socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        let mut socket = BufReader::new(socket);
        let mut start = String::new();
        socket.read_line(&mut start).unwrap();
        let mut length = 0;
        loop {
            let mut header = String::new();
            if socket.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        (socket, start, length)
    }

    #[test]
    fn cancels_while_waiting_for_the_rest_of_a_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/slow", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _, _) = serve(listener);
            socket
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nfirst")
                .unwrap();
            // The request has ended when the peer closes the socket, even
            // though we never sent the remainder of the declared body.
            match socket.read(&mut [0u8; 1]) {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                outcome => panic!("expected the client to close the connection: {outcome:?}"),
            }
        });
        let abort = Abort::new();
        let worker_abort = abort.clone();
        let (chunks, output) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let client = std::thread::spawn(move || {
            let outcome = exchange(
                w!("GET"),
                &url,
                &[],
                Vec::new(),
                Some(&worker_abort),
                &mut |_, chunk| {
                    let _ = chunks.send(chunk.to_vec());
                },
            );
            let _ = finished.send(outcome.map(|(status, _)| status));
        });
        assert_eq!(output.recv_timeout(WAIT).unwrap(), b"first");
        abort.abort();
        assert_eq!(
            result.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err("请求已停止".into())
        );
        client.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn collects_a_document_and_preserves_the_query() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/echo?format=raw", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, start, length) = serve(listener);
            assert_eq!(start, "POST /echo?format=raw HTTP/1.1\r\n");
            let mut payload = vec![0; length];
            socket.read_exact(&mut payload).unwrap();
            assert_eq!(payload, b"in");
            socket.get_mut().write_all(
                b"HTTP/1.1 201 Created\r\nContent-Length: 3\r\nX-Sample: test\r\nConnection: close\r\n\r\nout",
            ).unwrap();
        });
        assert_eq!(
            post_for(&url, &[], b"in", "X-Sample"),
            Ok((201, b"out".to_vec(), Some("test".into())))
        );
        server.join().unwrap();
    }
}
