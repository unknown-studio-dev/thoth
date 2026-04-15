//! File change watcher.
//!
//! Wraps [`notify`] and exposes a tokio-friendly [`Watcher`] that publishes
//! [`thoth_core::Event`] values on an mpsc channel.
//!
//! Debouncing and deletion handling are intentionally simple here; the
//! orchestrator is responsible for batching changes into index deltas.

use std::path::Path;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use thoth_core::{Error, Event, Result};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// A running file watcher.
///
/// Drop the [`Watcher`] to stop watching. The [`rx`](Self::rx) side is what
/// consumers use to receive [`Event`] values.
pub struct Watcher {
    _inner: RecommendedWatcher,
    rx: mpsc::Receiver<Event>,
}

impl Watcher {
    /// Start watching `root` recursively and return a [`Watcher`] whose
    /// channel will emit events until dropped.
    ///
    /// `buffer` is the size of the internal mpsc channel; bursty workloads
    /// may want something generous (e.g. 1024).
    pub fn watch(root: impl AsRef<Path>, buffer: usize) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Event>(buffer);
        let tx_for_cb = tx.clone();

        let mut inner: RecommendedWatcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let ev = match res {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(error = %e, "watcher error");
                        return;
                    }
                };
                for path in ev.paths {
                    let now = OffsetDateTime::now_utc();
                    let mapped = match ev.kind {
                        EventKind::Remove(_) => Some(Event::FileDeleted { path, at: now }),
                        EventKind::Create(_) | EventKind::Modify(_) => Some(Event::FileChanged {
                            path,
                            commit: None,
                            at: now,
                        }),
                        _ => None,
                    };
                    if let Some(m) = mapped {
                        // blocking_send is acceptable inside notifies worker
                        // thread; if the receiver is gone, we drop silently.
                        if tx_for_cb.blocking_send(m).is_err() {
                            debug!("watcher channel closed; dropping event");
                        }
                    }
                }
            })
            .map_err(|e| Error::Other(anyhow::anyhow!("notify init: {e}")))?;

        inner
            .watch(root.as_ref(), RecursiveMode::Recursive)
            .map_err(|e| Error::Other(anyhow::anyhow!("notify watch: {e}")))?;

        // keep `tx` alive only via the closure; drop the original handle so
        // the channel closes when the watcher is dropped.
        drop(tx);

        Ok(Self { _inner: inner, rx })
    }

    /// Receive the next event, or `None` if the watcher has been dropped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
