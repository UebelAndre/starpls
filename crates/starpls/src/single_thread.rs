//! Running the server on one thread.
//!
//! The normal transport reads stdin and writes stdout on threads of their own
//! and request handlers run on a thread pool. WebAssembly (WASI preview 1 as
//! VS Code and Node provide it) has no threads, so in this mode the main loop
//! reads one message at a time, handles it, runs every task that produced
//! inline, and flushes the output before blocking on the next read. The mode
//! also works natively (`--single_threaded`), which is how it is tested.

use std::io;
use std::io::Write;

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use lsp_server::Connection;
use lsp_server::Message;
use lsp_server::Response;

pub(crate) struct SingleThreadTransport {
    /// Messages read from stdin are pushed here for the `Connection`.
    pub(crate) inbound: Sender<Message>,
    /// Messages the server sent through the `Connection`, to be written out.
    pub(crate) outbound: Receiver<Message>,
}

impl SingleThreadTransport {
    /// A `Connection` whose two ends are channels this transport pumps.
    pub(crate) fn new() -> (Connection, Self) {
        let (inbound, receiver) = crossbeam_channel::unbounded();
        let (sender, outbound) = crossbeam_channel::unbounded();
        (
            Connection { sender, receiver },
            SingleThreadTransport { inbound, outbound },
        )
    }

    /// Reads one message from stdin, `None` at end of input.
    pub(crate) fn read(&self) -> io::Result<Option<Message>> {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        Message::read(&mut reader)
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
    pub(crate) fn initialize(
        &self,
        connection: &Connection,
        server_capabilities: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let message = self
            .read()?
            .ok_or_else(|| anyhow::anyhow!("connection closed before `initialize`"))?;
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
