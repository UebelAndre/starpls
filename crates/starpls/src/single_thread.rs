//! Running the server on one thread.
//!
//! The normal transport reads stdin and writes stdout on threads of their own
//! and request handlers run on a thread pool. WebAssembly (WASI preview 1 as
//! VS Code and Node provide it) has no threads, so in this mode the main loop
//! reads one message at a time, handles it, runs every task that produced
//! inline, and flushes the output before blocking on the next read. The mode
//! also works natively (`--single_threaded`), which is how it is tested.

use std::io;
use std::io::Read;
use std::io::Write;
use std::time::Duration;

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use lsp_server::Connection;
use lsp_server::ErrorCode;
use lsp_server::Message;
use lsp_server::Response;

/// The file descriptor of standard input.
#[cfg(any(unix, target_family = "wasm"))]
const STDIN_FD: i32 = 0;

pub(crate) struct SingleThreadTransport {
    /// Messages read from stdin are pushed here for the `Connection`.
    pub(crate) inbound: Sender<Message>,
    /// Messages the server sent through the `Connection`, to be written out.
    pub(crate) outbound: Receiver<Message>,
    /// Standard input, unbuffered; `pending` holds what has been read.
    stdin: Box<dyn Read>,
    /// Bytes read from stdin that do not form a complete message yet. A read
    /// that stops mid-frame (`WouldBlock`, a short read) resumes here, so a
    /// frame is never parsed from its middle.
    pending: Vec<u8>,
}

/// The framing of one message: where its body starts and how long it is.
struct Frame {
    body_start: usize,
    body_len: usize,
}

impl SingleThreadTransport {
    /// A `Connection` whose two ends are channels this transport pumps.
    pub(crate) fn new() -> (Connection, Self) {
        let (inbound, receiver) = crossbeam_channel::unbounded();
        let (sender, outbound) = crossbeam_channel::unbounded();
        (
            Connection { sender, receiver },
            SingleThreadTransport {
                inbound,
                outbound,
                stdin: stdin_source(),
                pending: Vec::new(),
            },
        )
    }

    /// Reads one message from stdin, `None` at end of input. A descriptor the
    /// host left non-blocking reports `WouldBlock` between reads; that is
    /// waited out with the partial frame kept.
    pub(crate) fn read(&mut self) -> io::Result<Option<Message>> {
        loop {
            if let Some(frame) = self.frame()? {
                let end = frame.body_start + frame.body_len;
                if self.pending.len() >= end {
                    let message: Message =
                        serde_json::from_slice(&self.pending[frame.body_start..end])
                            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                    self.pending.drain(..end);
                    return Ok(Some(message));
                }
            }
            let mut chunk = [0u8; 8192];
            match self.stdin.read(&mut chunk) {
                Ok(0) => {
                    if self.pending.iter().all(|b| b.is_ascii_whitespace()) {
                        return Ok(None);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "input ended in the middle of a message",
                    ));
                }
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    poll_stdin(Duration::from_secs(1))?;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// The framing of the message at the start of `pending`, once its
    /// headers are complete.
    fn frame(&self) -> io::Result<Option<Frame>> {
        let Some(headers_end) = find(&self.pending, b"\r\n\r\n") else {
            return Ok(None);
        };
        let headers = std::str::from_utf8(&self.pending[..headers_end])
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let mut body_len = None;
        for header in headers.split("\r\n") {
            if let Some((name, value)) = header.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    body_len = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
                    );
                }
            }
        }
        match body_len {
            Some(body_len) => Ok(Some(Frame {
                body_start: headers_end + 4,
                body_len,
            })),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message without a Content-Length header",
            )),
        }
    }

    /// Whether a complete message is already buffered.
    fn has_message(&self) -> bool {
        matches!(
            self.frame(),
            Ok(Some(frame)) if self.pending.len() >= frame.body_start + frame.body_len
        )
    }

    /// Waits until input is available or `timeout` has passed. `Ok(true)`
    /// means `read` has something to work with (possibly end of input).
    pub(crate) fn wait_readable(&mut self, timeout: Duration) -> io::Result<bool> {
        if self.has_message() {
            return Ok(true);
        }
        if timeout.is_zero() {
            return Ok(false);
        }
        poll_stdin(timeout)
    }

    /// Writes every message the server has queued.
    pub(crate) fn flush(&self) -> io::Result<()> {
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        while let Ok(message) = self.outbound.try_recv() {
            message.write(&mut writer)?;
        }
        writer.flush()
    }

    /// The `initialize` handshake, done by hand because the library version
    /// blocks on the connection while the response is still unwritten.
    /// Requests that arrive before `initialize` are answered with
    /// `ServerNotInitialized` and written out at once, so a client waiting
    /// for that error is not stuck; notifications other than `exit` are
    /// dropped, as the protocol asks.
    pub(crate) fn initialize(
        &mut self,
        connection: &Connection,
        server_capabilities: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let message = loop {
            let message = self
                .read()?
                .ok_or_else(|| anyhow::anyhow!("connection closed before `initialize`"))?;
            match message {
                Message::Request(ref req) if req.method == "initialize" => break message,
                Message::Request(req) => {
                    connection.sender.send(Message::Response(Response::new_err(
                        req.id,
                        ErrorCode::ServerNotInitialized as i32,
                        "expected `initialize` request".to_string(),
                    )))?;
                    self.flush()?;
                }
                Message::Notification(not) if not.method == "exit" => {
                    anyhow::bail!("`exit` received before `initialize`")
                }
                Message::Notification(_) | Message::Response(_) => {}
            }
        };
        self.inbound.send(message)?;
        let (id, params) = connection.initialize_start()?;
        connection.sender.send(Message::Response(Response::new_ok(
            id,
            serde_json::json!({ "capabilities": server_capabilities }),
        )))?;
        self.flush()?;
        // The client answers with `initialized`; the event loop ignores it.
        if let Some(message) = self.read()? {
            self.inbound.send(message)?;
        }
        Ok(params)
    }
}

/// The first position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Path canonicalization that works without a real file system.
///
/// WASI has no `realpath`, so `std::fs::canonicalize` fails there; paths are
/// normalized lexically instead. Mount points are already absolute and the
/// sandbox cannot follow symlinks out of them, so nothing is lost.
#[cfg(target_family = "wasm")]
pub(crate) fn canonicalize(path: &std::path::Path) -> io::Result<std::path::PathBuf> {
    use std::path::Component;
    use std::path::PathBuf;

    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot canonicalize relative path {}", path.display()),
        ));
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    // Keep the semantics of `canonicalize`: the path must exist.
    std::fs::metadata(&out)?;
    Ok(out)
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn canonicalize(path: &std::path::Path) -> io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
}

/// Standard input as an unbuffered reader. On unix and WASI it is the raw
/// descriptor, which `poll_stdin` can wait on; the `File` closes descriptor 0
/// when dropped, which only happens when the loop returns and the process
/// is about to exit.
#[cfg(unix)]
fn stdin_source() -> Box<dyn Read> {
    use std::os::fd::FromRawFd;
    // SAFETY: descriptor 0 is open for the lifetime of the process and this
    // is the only reader of it.
    Box::new(unsafe { std::fs::File::from_raw_fd(STDIN_FD) })
}

#[cfg(target_family = "wasm")]
fn stdin_source() -> Box<dyn Read> {
    use std::os::wasi::io::FromRawFd;
    // SAFETY: as above.
    Box::new(unsafe { std::fs::File::from_raw_fd(STDIN_FD) })
}

/// Windows has no `poll` on standard input here, so reads simply block; the
/// standard library's handle is used as is.
#[cfg(not(any(unix, target_family = "wasm")))]
fn stdin_source() -> Box<dyn Read> {
    Box::new(io::stdin())
}

/// Without a way to wait with a timeout, report input as available and let
/// the read block; parked requests then expire on the next message.
#[cfg(not(any(unix, target_family = "wasm")))]
fn poll_stdin(_timeout: Duration) -> io::Result<bool> {
    Ok(true)
}

/// Blocks until standard input is readable or `timeout` passes; `Ok(true)`
/// when readable. A timeout that the host cannot honour degrades to the
/// caller's plain blocking read, never to a busy loop.
#[cfg(unix)]
fn poll_stdin(timeout: Duration) -> io::Result<bool> {
    let mut fds = [libc::pollfd {
        fd: STDIN_FD,
        events: libc::POLLIN,
        revents: 0,
    }];
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `fds` is a valid array of one element for the duration of the call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, millis) };
    match ready {
        -1 => {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                Ok(true)
            } else {
                Err(err)
            }
        }
        0 => Ok(false),
        _ => Ok(true),
    }
}

#[cfg(target_family = "wasm")]
fn poll_stdin(timeout: Duration) -> io::Result<bool> {
    use std::mem::MaybeUninit;

    const READ: wasi::Userdata = 0;
    let subscriptions = [
        wasi::Subscription {
            userdata: READ,
            u: wasi::SubscriptionU {
                tag: wasi::EVENTTYPE_FD_READ.raw(),
                u: wasi::SubscriptionUU {
                    fd_read: wasi::SubscriptionFdReadwrite {
                        file_descriptor: STDIN_FD as wasi::Fd,
                    },
                },
            },
        },
        wasi::Subscription {
            userdata: 1,
            u: wasi::SubscriptionU {
                tag: wasi::EVENTTYPE_CLOCK.raw(),
                u: wasi::SubscriptionUU {
                    clock: wasi::SubscriptionClock {
                        id: wasi::CLOCKID_MONOTONIC,
                        timeout: timeout.as_nanos().min(u64::MAX as u128) as u64,
                        precision: 0,
                        flags: 0,
                    },
                },
            },
        },
    ];
    let mut events = [MaybeUninit::<wasi::Event>::uninit(), MaybeUninit::uninit()];
    // SAFETY: the pointers cover `subscriptions.len()` valid subscriptions and
    // as many event slots.
    let count = match unsafe {
        wasi::poll_oneoff(
            subscriptions.as_ptr(),
            events.as_mut_ptr() as *mut wasi::Event,
            subscriptions.len(),
        )
    } {
        Ok(count) => count,
        Err(err) => {
            // Some hosts cannot wait on a pipe (Node on Windows polls
            // sockets only). Reading blocks then, and parked requests
            // expire on the next message instead of on time; that is a
            // degradation, not a reason to stop the server.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                log::warn!(
                    "the host cannot wait on standard input ({}); request deadlines \
                     are checked when the next message arrives",
                    err
                );
            });
            return Ok(true);
        }
    };
    // SAFETY: the host wrote `count` events.
    let ready = events[..count]
        .iter()
        .any(|event| unsafe { event.assume_init_ref() }.userdata == READ);
    Ok(ready)
}
