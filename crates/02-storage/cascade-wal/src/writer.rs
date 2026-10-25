//! The per-DB writer: reserve/fill watermarks (FR-25) and leader-CAS group
//! flush with durable-LSN waiters (FR-26).

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    task::{Poll, Waker},
};

use cascade_types::Lsn;

use crate::{format, io::WalHandle, WalError, WalRecord};

/// Per-database WAL writer (§8.2): `Arc`-shared by every reactor appending
/// to this DB. Holds *only* shared memory — atomics and the staging map of
/// not-yet-durable record bytes. No file handle ever lives here (I2): the
/// flush leader borrows the caller's [`WalHandle`], on the caller's own
/// ring.
pub struct WalWriter {
    /// Reserved watermark: the next byte offset [`WalWriter::append_wal`]
    /// will hand out. LSNs exist before their records do (FR-25).
    reserved: AtomicU64,
    /// Filled watermark (FR-25, wal invariant 5): the highest byte offset
    /// reachable from 0 through *completely written* records. Advances
    /// only over the contiguous prefix — an unfilled reservation stops it
    /// dead, so no flush can ever cover a torn record.
    filled: AtomicU64,
    /// Durable watermark (FR-26): everything at or below this offset is
    /// `fdatasync`ed.
    durable: AtomicU64,
    /// Completed `fdatasync` barriers — the observable behind
    /// [`WalWriter::syncs`]: a caller that has *seen an ack* can assert the
    /// barrier count moved, proving the ack followed durability.
    syncs: AtomicU64,
    /// Encoded record bytes by start offset, until durable. The lock is the
    /// debug-slice ADR (§8.2's "mutex-free ring" lands with the perf ADR);
    /// the *watermark protocol* above is the production contract. Short
    /// critical sections only — never held across an await.
    pending: Mutex<BTreeMap<u64, Vec<u8>>>,
    /// Flush leadership (FR-26): CAS `false → true` elects the one leader.
    leader: AtomicBool,
    /// Waiters woken on any progress (fill advance, durable advance,
    /// leadership release). Wakers re-check their condition — the
    /// register-then-recheck pattern means no lost wakeups.
    wake: Mutex<Vec<Waker>>,
}

impl Default for WalWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl WalWriter {
    pub fn new() -> Self {
        WalWriter {
            reserved: AtomicU64::new(0),
            filled: AtomicU64::new(0),
            durable: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
            pending: Mutex::new(BTreeMap::new()),
            leader: AtomicBool::new(false),
            wake: Mutex::new(Vec::new()),
        }
    }

    /// Highest fully written byte offset (a record boundary).
    pub fn filled(&self) -> Lsn {
        Lsn(self.filled.load(Ordering::Acquire))
    }

    /// Highest `fdatasync`ed byte offset.
    pub fn durable(&self) -> Lsn {
        Lsn(self.durable.load(Ordering::Acquire))
    }

    /// How many `fdatasync` barriers have completed. FR-27's
    /// durable-before-ack ordering, observable: once a COMMIT ack is on the
    /// wire, this count has already moved.
    pub fn syncs(&self) -> u64 {
        self.syncs.load(Ordering::Acquire)
    }

    /// §8.2 `append_wal`: atomically reserve a byte range and return the
    /// reservation. The LSN exists now; the record does not. Finishing is
    /// [`Reservation::fill`] — dropping the reservation instead leaves a
    /// permanent tear (see the crate doc).
    pub fn append_wal(&self, record: &WalRecord) -> Reservation<'_> {
        let bytes = format::encode(record);
        let start = self
            .reserved
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Reservation {
            writer: self,
            start,
            bytes,
        }
    }

    /// §8.2 `flush_wal`: make everything up to `up_to` durable (FR-26),
    /// bounded always by the filled watermark (FR-25): bytes that are not
    /// completely written are never fsynced, and this call does not return
    /// until `durable >= up_to` — it waits rather than report a durability
    /// that has not happened. Elects one leader by CAS; the leader writes
    /// and `fdatasync`s through `io` (the caller's per-reactor handle, own
    /// ring) and publishes the durable LSN; losers wait for it.
    pub async fn flush_wal(&self, io: &WalHandle, up_to: Lsn) -> Result<Lsn, WalError> {
        loop {
            if self.durable() >= up_to {
                return Ok(self.durable());
            }
            if self
                .leader
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Leader: drain the contiguous prefix the watermarks allow.
                // The guard releases leadership on every exit path — an
                // abandoned (dropped) leader future cannot wedge the log.
                let progressed = {
                    let _leader = LeaderGuard(self);
                    let start = self.durable().0;
                    let target = self.filled().0.min(up_to.0);
                    if target > start {
                        let batch = self.take_batch(start, target)?;
                        io.write_batch(start, batch).await?;
                        io.sync_data().await?;
                        self.durable.store(target, Ordering::Release);
                        self.syncs.fetch_add(1, Ordering::AcqRel);
                        true
                    } else {
                        false
                    }
                };
                if !progressed {
                    // Nothing was flushable (a gap wedges us). Wait like any
                    // waiter instead of spinning through re-elections.
                    self.wait_durable(up_to).await?;
                }
            } else {
                self.wait_durable(up_to).await?;
            }
        }
    }

    /// Resolves when the durable LSN covers `lsn` (FR-26: "waiters are
    /// notified exactly when the durable LSN covers theirs"). A record
    /// beyond the filled watermark never resolves — its reservation must
    /// fill first.
    pub async fn wait_durable(&self, lsn: Lsn) -> Result<(), WalError> {
        let writer = self;
        std::future::poll_fn(move |cx| {
            if writer.durable() >= lsn {
                return Poll::Ready(Ok(()));
            }
            writer
                .wake
                .lock()
                .expect("wake lock")
                .push(cx.waker().clone());
            // Register-then-recheck: the wake may have fired between the
            // load above and the push.
            if writer.durable() >= lsn {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
        .await
    }

    /// Reservation `start` is now completely written. Publish the bytes and
    /// advance the filled watermark over the contiguous prefix (FR-25):
    /// a gap below us keeps `filled` — and therefore flush eligibility —
    /// stopped. The read-modify-store of `filled` happens under the pending
    /// lock so concurrent fills from different reactors serialize; computing
    /// outside and storing after would let a stale write retract the
    /// watermark.
    fn fill(&self, start: u64, bytes: Vec<u8>) {
        let mut pending = self.pending.lock().expect("pending lock");
        pending.insert(start, bytes);
        let mut filled = self.filled.load(Ordering::Acquire);
        while let Some(rec) = pending.get(&filled) {
            filled += rec.len() as u64;
        }
        self.filled.store(filled, Ordering::Release);
        drop(pending);
        self.wake_all();
    }

    /// Removes and returns the encoded records in `[start, end)` — a
    /// contiguous range of complete records (both ends are record
    /// boundaries). The leader is the only consumer; records below
    /// `durable` were already removed by earlier batches.
    fn take_batch(&self, start: u64, end: u64) -> Result<Vec<u8>, WalError> {
        let mut pending = self.pending.lock().expect("pending lock");
        let first = match pending.keys().next() {
            Some(k) => *k,
            None => start,
        };
        if first != start {
            return Err(WalError::Corruption(Lsn(first)));
        }
        let mut batch = Vec::new();
        let mut cursor = start;
        while cursor < end {
            let Some(bytes) = pending.remove(&cursor) else {
                return Err(WalError::Corruption(Lsn(cursor)));
            };
            cursor += bytes.len() as u64;
            batch.extend_from_slice(&bytes);
        }
        Ok(batch)
    }

    fn wake_all(&self) {
        let wakers = std::mem::take(&mut *self.wake.lock().expect("wake lock"));
        for w in wakers {
            w.wake();
        }
    }
}

/// Holds flush leadership for one leader section; dropping it releases the
/// flag and wakes the losers so one of them can be elected. This is what
/// makes a flush future safe to abandon mid-write — the guard fires on the
/// unwind/drop path instead of the flag leaking.
struct LeaderGuard<'a>(&'a WalWriter);

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        self.0.leader.store(false, Ordering::Release);
        self.0.wake_all();
    }
}

/// A reserved LSN range whose record does not exist yet.
///
/// Fill it with [`Reservation::fill`]. Dropping it unfilled leaves the
/// range torn forever: the filled watermark stops before it, so nothing at
/// or after it can flush (the crate doc has the crash rationale).
#[must_use = "a dropped reservation tears the log — call fill() or abort deliberately"]
pub struct Reservation<'a> {
    writer: &'a WalWriter,
    start: u64,
    bytes: Vec<u8>,
}

impl Reservation<'_> {
    /// LSN of the record's first byte.
    pub fn start(&self) -> Lsn {
        Lsn(self.start)
    }

    /// LSN one past the record's last byte — the flush/wait boundary.
    pub fn end(&self) -> Lsn {
        Lsn(self.start + self.bytes.len() as u64)
    }

    /// Publish the record: the bytes become visible to flushers and the
    /// filled watermark advances (over the contiguous prefix). Returns the
    /// record's LSN.
    pub fn fill(self) -> Lsn {
        let Reservation {
            writer,
            start,
            bytes,
        } = self;
        writer.fill(start, bytes);
        Lsn(start)
    }
}
