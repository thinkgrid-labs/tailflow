//! Live stream of committed agent activity (M4-B):
//! `GET /api/v1/stream/agent` (Server-Sent Events).
//!
//! Durable history is the source of truth. The stream only ever delivers
//! events that are already committed to SQLite, identified by their
//! persistent `seq`, and every subscriber keeps its own cursor:
//!
//! - **Catch-up.** A subscriber starts from `after=<seq>` (or the
//!   `Last-Event-ID` an `EventSource` sends when it reconnects) and reads
//!   committed history forward from SQLite, bounded by
//!   [`MAX_CATCHUP_EVENTS`]. Beyond that it gets a `reset` event and
//!   continues from the newest seq; the client reloads its views instead of
//!   receiving an unbounded backlog.
//! - **Live.** One shared [`AgentFeed`] task per daemon reads newly
//!   committed events and broadcasts them, serialized once, to every
//!   subscriber. It wakes on this daemon's own commits that contain
//!   non-runtime events (a storage commit hook; log-only commits are
//!   ignored) and polls [`Storage::newest_seq`] every [`POLL_INTERVAL`] for
//!   commits made by *other* daemons sharing the per-user database. It runs
//!   only while somebody is subscribed.
//! - **Lag.** The broadcast channel is bounded. A subscriber that falls
//!   behind does not lose events: it re-reads from SQLite from its own
//!   cursor and reports a `lag` event. The channel is a latency
//!   optimization, never the record.
//! - **Ordering.** SQLite serializes writers and assigns `seq` under the
//!   write lock, so committed seqs only grow across processes; events are
//!   delivered in strictly increasing `seq` with no duplicates (a
//!   subscriber never emits a seq at or below its cursor).
//! - **Gaps.** Detected loss in a delivered range (`system.events.dropped`
//!   markers and gap rows) and a cursor older than retained history are
//!   sent as `gap` events, never silently skipped.
//!
//! SSE event types: `agent` (an event; `id` is its seq), `gap`, `ready`
//! (catch-up finished, now live; `id` is the cursor), `reset`, `lag`.

use crate::state::AppState;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tailflow_storage::{EventQuery, Order, Storage, StoredEvent};
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;

/// Event families delivered by the stream. `file.*` includes filesystem
/// observations; `system.events.dropped` carries detected loss.
pub const STREAM_TYPES: &[&str] = &[
    "agent.*",
    "command.*",
    "file.*",
    "git.*",
    "system.events.dropped",
];

/// How often the feed checks for commits by other processes while it has
/// subscribers. Own commits wake it immediately.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Minimum spacing between feed reads, so a commit storm (runtime logs at
/// thousands of lines per second) costs at most this many reads per second.
const MIN_READ_SPACING: Duration = Duration::from_millis(50);
/// Events read per storage query.
const PAGE: usize = 500;
/// Pages the feed reads per wake-up before yielding.
const PAGES_PER_WAKE: usize = 8;
/// Batches buffered for subscribers. A subscriber further behind re-reads
/// from storage.
const CHANNEL_BATCHES: usize = 64;
/// History a (re)connecting subscriber receives before a `reset`.
pub const MAX_CATCHUP_EVENTS: usize = 5_000;
/// Concurrent stream subscribers per daemon.
pub const MAX_SUBSCRIBERS: usize = 32;
/// Recently reported gaps remembered per subscriber (to report each once).
const GAP_MEMORY: usize = 64;

/// One committed event, serialized once and shared by every subscriber.
#[derive(Debug)]
pub struct Item {
    pub seq: u64,
    pub json: Arc<str>,
    agent: Option<String>,
    session: Option<String>,
    project: Option<String>,
}

impl Item {
    fn from_stored(e: &StoredEvent) -> Option<Self> {
        let json = serde_json::to_string(e).ok()?;
        Some(Self {
            seq: e.seq,
            json: json.into(),
            agent: e.event.agent_name.clone(),
            session: e.event.session_id.clone(),
            project: e.event.project_id.clone(),
        })
    }
}

/// Detected loss inside a delivered range.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GapNote {
    pub reason: String,
    pub count: Option<u64>,
    pub from_seq: u64,
    pub to_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Events committed in `(after, to]`, in seq order.
#[derive(Debug)]
pub struct Batch {
    pub after: u64,
    pub to: u64,
    pub items: Vec<Item>,
    pub gaps: Vec<GapNote>,
}

/// Counters for diagnostics.
#[derive(Default, Debug, Serialize)]
pub struct FeedStats {
    pub subscribers: usize,
    pub cursor: u64,
    pub batches_published: u64,
    pub events_published: u64,
    pub subscriber_lag_recoveries: u64,
    pub catchup_resets: u64,
    pub refused_over_limit: u64,
    pub read_errors: u64,
}

/// The shared reader of newly committed agent events.
pub struct AgentFeed {
    tx: broadcast::Sender<Arc<Batch>>,
    wake: Notify,
    subscribers: AtomicUsize,
    cursor: AtomicU64,
    batches: AtomicU64,
    events: AtomicU64,
    lag_recoveries: AtomicU64,
    resets: AtomicU64,
    refused: AtomicU64,
    read_errors: AtomicU64,
}

impl AgentFeed {
    /// Start the feed for `storage`. The task ends on `shutdown`.
    pub fn spawn(storage: Storage, shutdown: CancellationToken) -> Arc<Self> {
        let (tx, _) = broadcast::channel(CHANNEL_BATCHES);
        let feed = Arc::new(Self {
            tx,
            wake: Notify::new(),
            subscribers: AtomicUsize::new(0),
            cursor: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            events: AtomicU64::new(0),
            lag_recoveries: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            read_errors: AtomicU64::new(0),
        });
        // Own commits wake the feed at once. `notify_one` stores a permit if
        // the feed is busy, so a commit is never slept through.
        let weak = Arc::downgrade(&feed);
        storage.on_commit(move |info| {
            // Runtime log lines are never streamed here: a log-only commit
            // (thousands per second under load) must not wake the feed.
            if info.inserted_non_runtime == 0 {
                return;
            }
            if let Some(f) = weak.upgrade() {
                if f.subscribers.load(Ordering::Relaxed) > 0 {
                    f.wake.notify_one();
                }
            }
        });
        let task_feed = feed.clone();
        tokio::spawn(async move { task_feed.run(storage, shutdown).await });
        feed
    }

    pub fn stats(&self) -> FeedStats {
        FeedStats {
            subscribers: self.subscribers.load(Ordering::Relaxed),
            cursor: self.cursor.load(Ordering::Relaxed),
            batches_published: self.batches.load(Ordering::Relaxed),
            events_published: self.events.load(Ordering::Relaxed),
            subscriber_lag_recoveries: self.lag_recoveries.load(Ordering::Relaxed),
            catchup_resets: self.resets.load(Ordering::Relaxed),
            refused_over_limit: self.refused.load(Ordering::Relaxed),
            read_errors: self.read_errors.load(Ordering::Relaxed),
        }
    }

    async fn run(self: Arc<Self>, storage: Storage, shutdown: CancellationToken) {
        let mut last_read = Instant::now()
            .checked_sub(MIN_READ_SPACING)
            .unwrap_or_else(Instant::now);
        let mut cursor: Option<u64> = None;
        loop {
            if self.subscribers.load(Ordering::Relaxed) == 0 {
                // Idle: no polling, no reads. A subscriber (or shutdown)
                // wakes us. Forget the cursor so a later subscriber starts
                // from the then-newest seq rather than replaying the gap.
                cursor = None;
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = self.wake.notified() => {}
                }
                continue;
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = self.wake.notified() => {}
                () = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            let since = last_read.elapsed();
            if since < MIN_READ_SPACING {
                tokio::time::sleep(MIN_READ_SPACING.saturating_sub(since)).await;
            }
            last_read = Instant::now();
            let s = storage.clone();
            let from = cursor;
            let read = tokio::task::spawn_blocking(move || read_new(&s, from)).await;
            match read {
                Ok(Ok((new_cursor, batches))) => {
                    cursor = Some(new_cursor);
                    self.cursor.store(new_cursor, Ordering::Relaxed);
                    for b in batches {
                        self.batches.fetch_add(1, Ordering::Relaxed);
                        self.events
                            .fetch_add(b.items.len() as u64, Ordering::Relaxed);
                        // No receivers is fine: everyone may have left.
                        let _receivers = self.tx.send(Arc::new(b));
                    }
                }
                Ok(Err(e)) => {
                    self.read_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(err = %e, "agent stream: reading new events failed");
                }
                Err(e) => {
                    self.read_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(err = %e, "agent stream: read task failed");
                }
            }
        }
    }

    /// Register a subscriber, or `None` at [`MAX_SUBSCRIBERS`].
    fn subscribe(self: &Arc<Self>) -> Option<Subscription> {
        let mut n = self.subscribers.load(Ordering::Relaxed);
        loop {
            if n >= MAX_SUBSCRIBERS {
                self.refused.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            match self.subscribers.compare_exchange_weak(
                n,
                n + 1,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => n = actual,
            }
        }
        let rx = self.tx.subscribe();
        self.wake.notify_one();
        Some(Subscription {
            feed: self.clone(),
            rx: Some(rx),
        })
    }
}

/// Holds a subscriber slot; released on drop (client gone).
struct Subscription {
    feed: Arc<AgentFeed>,
    rx: Option<broadcast::Receiver<Arc<Batch>>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.feed.subscribers.fetch_sub(1, Ordering::SeqCst);
    }
}

fn stream_query(after: u64, limit: usize) -> EventQuery {
    EventQuery {
        event_types: STREAM_TYPES.iter().map(|s| (*s).to_string()).collect(),
        after_seq: Some(after),
        order: Order::Asc,
        limit,
        ..EventQuery::default()
    }
}

fn gaps_of(page: &tailflow_storage::EventPage) -> Vec<GapNote> {
    page.completeness
        .gaps
        .iter()
        .map(|g| GapNote {
            reason: g.reason.clone(),
            count: g.count,
            from_seq: g.from_seq,
            to_seq: g.to_seq,
            first_at: g.first_at,
            last_at: g.last_at,
        })
        .collect()
}

/// Read events committed after `from` (`None`: start at the newest seq).
/// Returns the new cursor and the batches read. Blocking.
fn read_new(storage: &Storage, from: Option<u64>) -> tailflow_storage::Result<(u64, Vec<Batch>)> {
    let newest = storage.newest_seq()?;
    let Some(mut cursor) = from else {
        // Starting (first subscriber after idle): begin at the newest seq,
        // and say so with an empty batch. A subscriber whose own cursor is
        // behind it fills the difference from storage at once instead of
        // waiting for the next commit.
        return Ok((
            newest,
            vec![Batch {
                after: newest,
                to: newest,
                items: Vec::new(),
                gaps: Vec::new(),
            }],
        ));
    };
    let mut out = Vec::new();
    for _ in 0..PAGES_PER_WAKE {
        if cursor >= newest {
            break;
        }
        let page = storage.query(&stream_query(cursor, PAGE))?;
        let to = page.next_cursor.unwrap_or(cursor).max(cursor);
        let items: Vec<Item> = page.events.iter().filter_map(Item::from_stored).collect();
        let gaps = gaps_of(&page);
        if !items.is_empty() || !gaps.is_empty() {
            out.push(Batch {
                after: cursor,
                to,
                items,
                gaps,
            });
        } else if to > cursor {
            // Nothing of interest, but the range is covered: publish an
            // empty batch so subscribers' cursors advance with the feed.
            out.push(Batch {
                after: cursor,
                to,
                items: Vec::new(),
                gaps: Vec::new(),
            });
        }
        if to == cursor || !page.has_more {
            cursor = to;
            break;
        }
        cursor = to;
    }
    Ok((cursor, out))
}

/// Per-subscriber filters (each list empty = any).
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub agents: Vec<String>,
    pub sessions: Vec<String>,
    pub projects: Vec<String>,
}

impl Filter {
    fn matches(&self, item: &Item) -> bool {
        let ok = |list: &[String], v: &Option<String>| {
            list.is_empty() || v.as_deref().is_some_and(|v| list.iter().any(|l| l == v))
        };
        ok(&self.agents, &item.agent)
            && ok(&self.sessions, &item.session)
            && ok(&self.projects, &item.project)
    }
}

/// What a subscriber's stream produces next.
enum Out {
    Agent(u64, Arc<str>),
    Gap(GapNote),
    Ready { cursor: u64, caught_up: usize },
    Reset { reason: &'static str, cursor: u64 },
    Lag { skipped_batches: u64 },
}

impl Out {
    fn into_event(self) -> Event {
        match self {
            Out::Agent(seq, json) => Event::default()
                .event("agent")
                .id(seq.to_string())
                .data(json.as_ref()),
            Out::Gap(g) => Event::default()
                .event("gap")
                .data(serde_json::to_string(&g).unwrap_or_default()),
            Out::Ready { cursor, caught_up } => {
                Event::default().event("ready").id(cursor.to_string()).data(
                    serde_json::json!({ "cursor": cursor, "caught_up_events": caught_up })
                        .to_string(),
                )
            }
            Out::Reset { reason, cursor } => Event::default()
                .event("reset")
                .id(cursor.to_string())
                .data(serde_json::json!({ "reason": reason, "cursor": cursor }).to_string()),
            Out::Lag { skipped_batches } => Event::default().event("lag").data(
                serde_json::json!({
                    "skipped_batches": skipped_batches,
                    "recovered_from_storage": true
                })
                .to_string(),
            ),
        }
    }
}

/// One subscriber's state machine. Produces events in seq order.
struct Subscriber {
    storage: Storage,
    filter: Filter,
    /// Everything at or below this seq has been delivered (or did not
    /// match): never emitted again. `None` until the start is resolved
    /// (a live-only subscriber starts at the newest committed seq).
    cursor: u64,
    start_resolved: bool,
    queue: VecDeque<Out>,
    reported_gaps: VecDeque<(u64, u64, String)>,
    sub: Subscription,
    caught_up: bool,
    shutdown: CancellationToken,
    feed: Arc<AgentFeed>,
}

impl Subscriber {
    fn note_gaps(&mut self, gaps: Vec<GapNote>) {
        for g in gaps {
            // Only loss that overlaps what this subscriber has not seen yet.
            if g.to_seq < self.cursor && g.from_seq < self.cursor {
                continue;
            }
            let key = (g.from_seq, g.to_seq, g.reason.clone());
            if self.reported_gaps.contains(&key) {
                continue;
            }
            if self.reported_gaps.len() >= GAP_MEMORY {
                self.reported_gaps.pop_front();
            }
            self.reported_gaps.push_back(key);
            self.queue.push_back(Out::Gap(g));
        }
    }

    /// Read committed history after `self.cursor` from storage, up to the
    /// catch-up bound. Returns `false` when the bound was hit.
    async fn catch_up(&mut self) -> bool {
        let mut delivered = 0usize;
        loop {
            let s = self.storage.clone();
            let after = self.cursor;
            let page = tokio::task::spawn_blocking(move || {
                let newest = s.newest_seq()?;
                let page = s.query(&stream_query(after, PAGE))?;
                Ok::<_, tailflow_storage::StorageError>((newest, page))
            })
            .await;
            let (newest, page) = match page {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    tracing::warn!(err = %e, "agent stream: catch-up read failed");
                    return true;
                }
                Err(_) => return true,
            };
            // Retention removed events this subscriber never saw.
            let evicted = page.completeness.evicted_through_seq;
            if evicted > after && after > 0 {
                self.note_gaps(vec![GapNote {
                    reason: "retention".into(),
                    count: None,
                    from_seq: after,
                    to_seq: evicted,
                    first_at: None,
                    last_at: None,
                }]);
            }
            self.note_gaps(gaps_of(&page));
            for e in &page.events {
                if let Some(item) = Item::from_stored(e) {
                    if item.seq > self.cursor && self.filter.matches(&item) {
                        delivered += 1;
                        self.queue.push_back(Out::Agent(item.seq, item.json));
                    }
                }
            }
            let to = page.next_cursor.unwrap_or(after).max(after);
            self.cursor = to;
            if !page.has_more || to == after {
                return true;
            }
            if delivered >= MAX_CATCHUP_EVENTS {
                // Too far behind to stream: skip to the newest seq and let
                // the client reload its views from the REST API.
                self.cursor = self.cursor.max(newest);
                self.feed.resets.fetch_add(1, Ordering::Relaxed);
                self.queue.push_back(Out::Reset {
                    reason: "catchup_limit",
                    cursor: self.cursor,
                });
                return false;
            }
        }
    }

    fn apply(&mut self, batch: &Batch) {
        self.note_gaps(batch.gaps.clone());
        for item in &batch.items {
            if item.seq > self.cursor && self.filter.matches(item) {
                self.queue
                    .push_back(Out::Agent(item.seq, item.json.clone()));
            }
        }
        self.cursor = self.cursor.max(batch.to);
    }

    /// Next event for the client, or `None` when the stream ends.
    async fn next(&mut self) -> Option<Out> {
        loop {
            if let Some(out) = self.queue.pop_front() {
                return Some(out);
            }
            if self.shutdown.is_cancelled() {
                return None;
            }
            if !self.start_resolved {
                // Live-only: start at the newest committed seq.
                let s = self.storage.clone();
                match tokio::task::spawn_blocking(move || s.newest_seq()).await {
                    Ok(Ok(n)) => self.cursor = n,
                    Ok(Err(e)) => {
                        tracing::warn!(err = %e, "agent stream: newest seq unavailable");
                        return None;
                    }
                    Err(_) => return None,
                }
                self.start_resolved = true;
            }
            if !self.caught_up {
                let _within_bound = self.catch_up().await;
                self.caught_up = true;
                self.queue.push_back(Out::Ready {
                    cursor: self.cursor,
                    caught_up: self
                        .queue
                        .iter()
                        .filter(|o| matches!(o, Out::Agent(..)))
                        .count(),
                });
                continue;
            }
            let rx = self.sub.rx.as_mut()?;
            let received = tokio::select! {
                () = self.shutdown.cancelled() => return None,
                r = rx.recv() => r,
            };
            match received {
                Ok(batch) => {
                    if batch.after > self.cursor {
                        // A hole between what we read and this batch: fill it
                        // from storage first (self-healing; should not happen).
                        let _within_bound = self.catch_up().await;
                    }
                    self.apply(&batch);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    self.feed.lag_recoveries.fetch_add(1, Ordering::Relaxed);
                    self.queue.push_back(Out::Lag { skipped_batches: n });
                    let _within_bound = self.catch_up().await;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// Start a subscriber stream from `after` (exclusive).
///
/// # Errors
/// `None` when the subscriber limit is reached.
pub fn subscribe(
    state: &AppState,
    storage: Storage,
    filter: Filter,
    after: Option<u64>,
) -> Option<Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>>> {
    let feed = state.agent_feed.as_ref()?.clone();
    let sub = feed.subscribe()?;
    let subscriber = Subscriber {
        storage,
        filter,
        cursor: after.unwrap_or(0),
        start_resolved: after.is_some(),
        queue: VecDeque::new(),
        reported_gaps: VecDeque::new(),
        sub,
        // A live-only subscriber has nothing to catch up on.
        caught_up: false,
        shutdown: state.shutdown.clone(),
        feed,
    };
    let stream = futures_util::stream::unfold(subscriber, |mut s| async move {
        let out = s.next().await?;
        Some((Ok(out.into_event()), s))
    });
    Some(Sse::new(stream).keep_alive(KeepAlive::default()))
}
