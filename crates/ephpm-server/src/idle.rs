//! Idle-connection tracking for HTTP listeners.
//!
//! Implements the `[server.timeouts] idle` knob: a connection with no read
//! or write activity for the configured duration is shut down gracefully.
//!
//! [`IdleIo`] wraps the accepted stream (plain TCP or TLS) and stamps a
//! shared [`ActivityTracker`] on every successful read/write. The connection
//! task races the hyper connection future against
//! [`ActivityTracker::idle_expired`], which acts as a watchdog: it sleeps
//! until the most recent activity plus the idle window, re-arming itself
//! whenever new activity pushes the deadline forward.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

/// Coarse granularity for activity stamps. A read/write within this window of
/// the last recorded activity does not re-store the timestamp, so a busy
/// connection's `poll_read`/`poll_write` storm doesn't bounce the shared
/// `last_activity_ms` cache line between the read and write tasks on every
/// byte. The idle watchdog only cares about second-scale windows, so a 50ms
/// stamp granularity is invisible to it.
const TOUCH_GRANULARITY_MS: u64 = 50;

/// Shared last-activity clock for a single connection.
///
/// Stores milliseconds elapsed since the tracker was created in an
/// [`AtomicU64`], so the I/O wrapper can stamp activity from `poll_read` /
/// `poll_write` without locks.
#[derive(Clone)]
pub(crate) struct ActivityTracker {
    /// Reference point for the atomic offset — the moment the connection
    /// was accepted.
    epoch: Instant,
    /// Milliseconds since `epoch` of the most recent read/write.
    last_activity_ms: Arc<AtomicU64>,
}

impl ActivityTracker {
    /// Create a tracker whose last-activity time is "now".
    pub(crate) fn new() -> Self {
        Self { epoch: Instant::now(), last_activity_ms: Arc::new(AtomicU64::new(0)) }
    }

    /// Record activity at the current instant.
    ///
    /// Skips the atomic store when the previous stamp is younger than
    /// [`TOUCH_GRANULARITY_MS`], collapsing the per-poll write storm on a busy
    /// connection into at most one store per 50ms window. The read of
    /// `last_activity_ms` is `Relaxed` and uncontended in the common case; the
    /// deadline only ever moves forward, so a coalesced stamp can under-report
    /// activity by at most one granularity window — negligible against the
    /// second-scale idle timeout.
    fn touch(&self) {
        let ms = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        let prev = self.last_activity_ms.load(Ordering::Relaxed);
        if ms.saturating_sub(prev) >= TOUCH_GRANULARITY_MS {
            self.last_activity_ms.store(ms, Ordering::Relaxed);
        }
    }

    /// The instant of the most recent recorded activity.
    fn last_activity(&self) -> Instant {
        self.epoch + Duration::from_millis(self.last_activity_ms.load(Ordering::Relaxed))
    }

    /// Resolve once the connection has seen no activity for `idle`.
    ///
    /// Re-arms itself whenever activity moves the deadline forward, so it
    /// only completes after a full quiet window.
    pub(crate) async fn idle_expired(&self, idle: Duration) {
        loop {
            let deadline = self.last_activity() + idle;
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

/// `AsyncRead`/`AsyncWrite` adapter that stamps an [`ActivityTracker`] on
/// every successful read or write.
///
/// Flush and shutdown are deliberately *not* counted as activity — only
/// actual byte transfer keeps a connection alive.
pub(crate) struct IdleIo<I> {
    inner: I,
    tracker: ActivityTracker,
}

impl<I> IdleIo<I> {
    /// Wrap `inner`, stamping `tracker` on read/write progress.
    pub(crate) fn new(inner: I, tracker: ActivityTracker) -> Self {
        Self { inner, tracker }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for IdleIo<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.tracker.touch();
        }
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for IdleIo<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(result, Poll::Ready(Ok(n)) if n > 0) {
            self.tracker.touch();
        }
        result
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if matches!(result, Poll::Ready(Ok(n)) if n > 0) {
            self.tracker.touch();
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn expires_after_quiet_window() {
        let tracker = ActivityTracker::new();
        let start = Instant::now();
        tracker.idle_expired(Duration::from_secs(60)).await;
        assert!(start.elapsed() >= Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn touch_pushes_deadline_forward() {
        let tracker = ActivityTracker::new();
        let start = Instant::now();

        // Halfway through the window, record activity — the watchdog must
        // wait a *full* window from the touch, not from creation.
        tokio::time::advance(Duration::from_secs(30)).await;
        tracker.touch();

        tracker.idle_expired(Duration::from_secs(60)).await;
        assert!(start.elapsed() >= Duration::from_secs(90));
    }

    #[tokio::test(start_paused = true)]
    async fn touch_within_granularity_is_coalesced() {
        let tracker = ActivityTracker::new();

        // First touch after 100ms records the stamp.
        tokio::time::advance(Duration::from_millis(100)).await;
        tracker.touch();
        let first = tracker.last_activity_ms.load(Ordering::Relaxed);
        assert_eq!(first, 100);

        // A second touch 10ms later (< 50ms granularity) must NOT move the
        // stamp — the store is skipped.
        tokio::time::advance(Duration::from_millis(10)).await;
        tracker.touch();
        assert_eq!(
            tracker.last_activity_ms.load(Ordering::Relaxed),
            first,
            "touch within the granularity window must not re-store"
        );

        // Past the window, the stamp advances again.
        tokio::time::advance(Duration::from_millis(50)).await;
        tracker.touch();
        assert_eq!(tracker.last_activity_ms.load(Ordering::Relaxed), 160);
    }
}
