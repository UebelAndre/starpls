use std::time::Duration;

use crossbeam_channel::Receiver;
#[cfg(not(target_family = "wasm"))]
use crossbeam_channel::RecvError;
#[cfg(not(target_family = "wasm"))]
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::Sender;
#[cfg(not(target_family = "wasm"))]
use rustc_hash::FxHashSet;
use starpls_common::FileId;

use crate::event_loop::Task;

/// Coalesces bursts of edits into one analysis request.
///
/// Normally a thread waits out the debounce interval. In inline mode (always
/// on WebAssembly, where there are no threads and no idle time to wait in)
/// file ids are forwarded as soon as the event loop calls `pump`, so edits
/// are analysed without debouncing.
pub(crate) struct AnalysisDebouncer {
    pub(crate) sender: Sender<Vec<FileId>>,
    /// Present in inline mode only.
    inline: Option<(Receiver<Vec<FileId>>, Sender<Task>)>,
}

impl AnalysisDebouncer {
    pub(crate) fn new(duration: Duration, sink: Sender<Task>, inline: bool) -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded::<Vec<FileId>>();
        if inline || cfg!(target_family = "wasm") {
            return Self {
                sender,
                inline: Some((receiver, sink)),
            };
        }
        #[cfg(not(target_family = "wasm"))]
        std::thread::spawn(move || {
            let mut active = false;
            let mut pending_file_ids: FxHashSet<FileId> = FxHashSet::default();
            loop {
                if active {
                    match receiver.recv_timeout(duration) {
                        Ok(file_ids) => pending_file_ids.extend(file_ids),
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            sink.send(Task::AnalysisRequested(pending_file_ids.drain().collect()))
                                .unwrap();
                            active = false;
                        }
                    }
                } else {
                    match receiver.recv() {
                        Ok(file_ids) => {
                            active = true;
                            pending_file_ids.extend(file_ids);
                        }
                        Err(RecvError) => break,
                    }
                }
            }
        });
        #[cfg(target_family = "wasm")]
        let _ = (duration, receiver, sink);
        Self {
            sender,
            inline: None,
        }
    }

    /// In inline mode, forwards pending file ids as one analysis request.
    /// A no-op otherwise; the thread does it on its own schedule.
    pub(crate) fn pump(&self) {
        let (receiver, sink) = match &self.inline {
            Some(inline) => inline,
            None => return,
        };
        let mut pending: Vec<FileId> = Vec::new();
        while let Ok(file_ids) = receiver.try_recv() {
            pending.extend(file_ids);
        }
        if !pending.is_empty() {
            pending.sort();
            pending.dedup();
            sink.send(Task::AnalysisRequested(pending)).unwrap();
        }
    }
}
