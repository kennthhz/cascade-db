//! The log store over a [`BlockDevice`] (§5): shared segment files of
//! 4 KiB-aligned flush frames, epochs fenced through durable control
//! extents, retention holders, and the §5.7 recovery scan that tells a
//! torn flush from synced-then-damaged corruption.
//!
//! M4 slice 3 shape: the flush is inline — **waiting is what triggers a
//! flush** (§4.3), so `wait_durable` plans a frame with the DRR planner,
//! writes it, syncs it, and advances the per-scope durable LSNs on the
//! calling task. The per-stream flusher task (§5.4's production shape,
//! group-commit window included) is a later slice; the contract law the
//! suites pin — contiguity, framing, S16 group ends, fencing-before-
//! idempotence, S19 blast radius, retention — is identical either way.
//! Segment reuse (§5.6) and the bounded recent-log cache (§5.3: this
//! slice keeps an unbounded tail cache, which the idempotence law's
//! durable-overlap check reads) are the later slices.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Waker;
use std::task::Poll;

use crate::drr::{plan_flush, PendingGroup, ScopeQueue};
use cascade_storage_api::fake::test_ctx;
use cascade_storage_api::log_sink::LogSink;
use cascade_storage_api::log_source::{LogSource, LogSubscription};
use cascade_storage_api::scope_admin::LogScopes;
use cascade_storage_api::{
    BlockDevice, BlockHandle, FileName, IoError, KeyRef, LogChunk, OpCtx,
    RetentionHolder, StoreError,
};
use cascade_storage_api::{CorruptionSite};
use cascade_types::{Epoch, Hlc, Lsn, ScopeId};
use cascade_wal_format::walk;

use crate::frame::{self, Extent, ExtentKind, FrameHeader, FRAME_HEADER_LEN, PAD};
use crate::manifest::{self, ExtentRun, Manifest, StreamPosition};

/// 64 MiB — the segment size (§5.2; a starting value, §11 item 5).
const SEGMENT_SIZE: u64 = 64 * 1024 * 1024;
/// DRR quantum (§5.4, default 64 KiB).
const QUANTUM: usize = 64 * 1024;
/// Frame size cap (§5.4, default 1 MiB).
const FRAME_CAP: usize = 1024 * 1024;
/// The stream every scope shares in slice 3 (§5.4: heavy scopes can be
/// moved to their own stream later).
const STREAM: u32 = 0;
const MANIFEST_NAME: &str = "MANIFEST";

/// Control-op kinds (the control extent payload, §5.2): 21 bytes each —
/// kind, scope, epoch, key id. Control extents are what make fencing and
/// scope lifecycle durable (§4.3 promise 1: `open_writer` returns only
/// after its control extent's frame is synced).
const CTRL_CREATE: u8 = 0;
const CTRL_EPOCH: u8 = 1;
const CTRL_DROP: u8 = 2;

#[derive(Debug, Clone)]
struct ControlOp {
    kind: u8,
    scope: ScopeId,
    epoch: Epoch,
    key_id: u64,
}

impl ControlOp {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(21);
        b.push(self.kind);
        b.extend_from_slice(&self.scope.0.to_le_bytes());
        b.extend_from_slice(&self.epoch.0.to_le_bytes());
        b.extend_from_slice(&(self.key_id as u32).to_le_bytes());
        b
    }
    fn decode(bytes: &[u8]) -> Result<ControlOp, StoreError> {
        if bytes.len() != 21 {
            return Err(StoreError::Corruption {
                what: CorruptionSite::Manifest,
            });
        }
        Ok(ControlOp {
            kind: bytes[0],
            scope: ScopeId(u64::from_le_bytes(bytes[1..9].try_into().expect("8"))),
            epoch: Epoch(u64::from_le_bytes(bytes[9..17].try_into().expect("8"))),
            key_id: u32::from_le_bytes(bytes[17..21].try_into().expect("4")) as u64,
        })
    }
}

#[derive(Clone)]
struct AppendSpan {
    start: Lsn,
    #[allow(dead_code)] // the span's reach; read with the time index (§4.4)
    len: usize,
    max_commit_ts: Option<Hlc>,
}

#[derive(Clone)]
struct ScopeState {
    epoch: Epoch,
    durable: Lsn,
    /// End of staged+flushed bytes — the contiguity frontier (§4.3
    /// promise 2's `expected`).
    appended: Lsn,
    staged: VecDeque<PendingGroup>,
    deficit: usize,
    waiters: Vec<(Lsn, Waker)>,
    holders: HashMap<RetentionHolder, Lsn>,
    /// The extent index: located LSN ranges (§5.3), rebuilt by the scan.
    runs: Vec<ExtentRun>,
    spans: Vec<AppendSpan>,
    /// The recent-log tail (§5.3): the last flushed bytes, from
    /// `cache_from` to `durable`. Slice 3 keeps it unbounded — the trim
    /// is a §5.3 later slice.
    cache: Vec<u8>,
    cache_from: Lsn,
    failed: bool,
    dropped: bool,
    key_id: u64,
}

impl Default for ScopeState {
    fn default() -> Self {
        ScopeState {
            epoch: Epoch::FIRST,
            durable: Lsn(0),
            appended: Lsn(0),
            staged: VecDeque::new(),
            deficit: 0,
            waiters: Vec::new(),
            holders: HashMap::new(),
            runs: Vec::new(),
            spans: Vec::new(),
            cache: Vec::new(),
            cache_from: Lsn(0),
            failed: false,
            dropped: false,
            key_id: 0,
        }
    }
}

#[derive(Clone)]
struct StreamState {
    pos: StreamPosition,
    cur_segment: u64,
    write_offset: u64,
    next_frame_seq: u64,
}

struct Inner {
    incarnation: u64,
    next_scope_id: u64,
    clean_shutdown: bool,
    streams: HashMap<u32, StreamState>,
    scopes: HashMap<ScopeId, ScopeState>,
    staged_controls: Vec<ControlOp>,
}

/// The log store (§5): one instance per process (single server) or per
/// replica (managed service), `Send + Sync`, shared by every core.
pub struct LogStore<D: BlockDevice + 'static> {
    dev: Arc<D>,
    inner: Mutex<Inner>,
    /// This store's own handle, for subscriptions (which must outlive
    /// the `&self` that spawned them).
    me: OnceLock<std::sync::Weak<LogStore<D>>>,
}

impl<D: BlockDevice + 'static> LogStore<D> {
    /// Opens the store: read the manifest (a fresh one on a new device),
    /// bump the incarnation, run the §5.7 recovery scan, write the
    /// manifest back — all before any frame is written.
    pub async fn open(dev: Arc<D>) -> Result<Arc<Self>, StoreError> {
        let local = dev.local();
        let span = cascade_telemetry::Span::noop();
        let ctx = OpCtx::new(cascade_storage_api::TenantContext::for_database(cascade_types::DatabaseId(0)), &span);

        let manifest_bytes = match read_file(&*local, &ctx, MANIFEST_NAME).await {
            Some(padded) => {
                // The file is length-prefixed (the O_DIRECT pad would
                // otherwise shift the CRC trailer on read-back).
                if padded.len() < 4 {
                    return Err(StoreError::Corruption {
                        what: CorruptionSite::Manifest,
                    });
                }
                let body_len =
                    u32::from_le_bytes(padded[..4].try_into().expect("4")) as usize;
                if padded.len() < 4 + body_len {
                    return Err(StoreError::Corruption {
                        what: CorruptionSite::Manifest,
                    });
                }
                manifest::decode(&padded[4..4 + body_len]).map_err(|_| {
                    StoreError::Corruption {
                        what: CorruptionSite::Manifest,
                    }
                })?
            }
            None => Manifest {
                incarnation: 0,
                next_scope_id: 1,
                clean_shutdown: false,
                streams: vec![StreamPosition {
                    stream: STREAM,
                    next_segment_seq: 0,
                    scan_segment_seq: 0,
                    scan_frame_seq: 0,
                    scan_offset: 0,
                }],
                scopes: vec![],
            },
        };
        let mut m = manifest::bump_incarnation(manifest_bytes);
        if m.streams.iter().all(|s| s.stream != STREAM) {
            m.streams.push(StreamPosition {
                stream: STREAM,
                next_segment_seq: 0,
                scan_segment_seq: 0,
                scan_frame_seq: 0,
                scan_offset: 0,
            });
        }

        let mut inner = Inner {
            incarnation: m.incarnation,
            next_scope_id: m.next_scope_id,
            clean_shutdown: false,
            streams: HashMap::new(),
            scopes: HashMap::new(),
            staged_controls: Vec::new(),
        };
        // Manifest scopes seed the FULL record — the manifest was written
        // from durable state, so its durable/appended/index are the truth
        // the scan extends (never lowers): a manifest whose scan position
        // is already at the end has nothing to replay, and its numbers
        // must survive the open.
        for rec in &m.scopes {
            inner.scopes.insert(
                rec.scope,
                ScopeState {
                    epoch: rec.epoch,
                    key_id: rec.key_id,
                    durable: rec.durable,
                    appended: rec.durable,
                    runs: rec.runs.clone(),
                    cache_from: rec.durable,
                    holders: rec.holders.iter().map(|(h, l)| (*h, *l)).collect(),
                    ..ScopeState::default()
                },
            );
        }

        let seed_pos = m
            .streams
            .iter()
            .find(|s| s.stream == STREAM)
            .cloned()
            .expect("seeded above");
        inner.streams.insert(
            STREAM,
            StreamState {
                pos: seed_pos.clone(),
                cur_segment: seed_pos.scan_segment_seq,
                write_offset: seed_pos.scan_offset,
                next_frame_seq: seed_pos.scan_frame_seq,
            },
        );
        scan_stream(&*local, &ctx, &mut inner, &seed_pos).await?;

        let store = Arc::new(LogStore {
            dev,
            inner: Mutex::new(inner),
            me: OnceLock::new(),
        });
        let _ = store.me.set(Arc::downgrade(&store));
        store.write_manifest().await?;
        Ok(store)
    }

    /// Clean shutdown (§5.7): the manifest records the exact end, so any
    /// bad frame before it is corruption, never a torn flush.
    pub fn mark_clean_shutdown(&self) {
        let mut inner = self.inner.lock().expect("store lock");
        inner.clean_shutdown = true;
    }

    /// Writes the manifest atomically: write → sync → rename →
    /// sync_namespace (§4.8; Appendix C item 11's order). The stream's
    /// scan position moves to the write head, so recovery resumes there.
    async fn write_manifest(&self) -> Result<(), StoreError> {
        let manifest = {
            let inner = self.inner.lock().expect("store lock");
            let streams: Vec<StreamPosition> = inner
                .streams
                .values()
                .map(|s| StreamPosition {
                    stream: s.pos.stream,
                    next_segment_seq: s.pos.next_segment_seq,
                    scan_segment_seq: s.cur_segment,
                    scan_frame_seq: s.next_frame_seq,
                    scan_offset: s.write_offset,
                })
                .collect();
            Manifest {
                incarnation: inner.incarnation,
                next_scope_id: inner.next_scope_id,
                clean_shutdown: inner.clean_shutdown,
                streams,
                scopes: inner
                    .scopes
                    .iter()
                    .map(|(id, s)| manifest::ScopeRecord {
                        scope: *id,
                        epoch: s.epoch,
                        key_id: s.key_id,
                        durable: s.durable,
                        holders: s.holders.iter().map(|(h, l)| (*h, *l)).collect(),
                        runs: s.runs.clone(),
                    })
                    .collect(),
            }
        };
        let local = self.dev.local();
        let span = cascade_telemetry::Span::noop();
        let ctx = OpCtx::new(cascade_storage_api::TenantContext::for_database(cascade_types::DatabaseId(0)), &span);
        let body = manifest::encode(&manifest);
        let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&body);
        write_file(&*local, &ctx, "MANIFEST.tmp", &bytes).await?;
        local
            .rename(&ctx, &FileName("MANIFEST.tmp".into()), &FileName(MANIFEST_NAME.into()))
            .await
            .map_err(|e| StoreError::Io(e.0))?;
        local.sync_namespace().await.map_err(|e| StoreError::Io(e.0))?;
        Ok(())
    }

    /// Builds and persists one frame from the staged queues (inline flush,
    /// §4.3: waiting triggers it). Plans with DRR, composes record extents
    /// plus any staged control ops, writes + syncs, then advances the
    /// per-scope durable LSNs and wakes waiters.
    async fn flush_some(&self) -> Result<(), StoreError> {
        // 1. Plan under the lock; take the staged groups out.
        struct Planned {
            scope: ScopeId,
            groups: Vec<PendingGroup>,
            epoch: Epoch,
            durable_to: Lsn,
        }
        let (planned, controls, st) = {
            let mut inner = self.inner.lock().expect("store lock");
            if inner.staged_controls.is_empty()
                && inner.scopes.values().all(|s| s.staged.is_empty())
            {
                return Ok(());
            }
            let controls = std::mem::take(&mut inner.staged_controls);
            let mut views: Vec<ScopeQueue> = Vec::new();
            let mut indexes: Vec<ScopeId> = Vec::new();
            for (id, s) in inner.scopes.iter_mut() {
                if !s.staged.is_empty() {
                    indexes.push(*id);
                    views.push(ScopeQueue {
                        scope: *id,
                        epoch: s.epoch,
                        groups: std::mem::take(&mut s.staged),
                        deficit: s.deficit,
                        has_waiter: !s.waiters.is_empty(),
                    });
                }
            }
            let plan = plan_flush(&mut views, QUANTUM, FRAME_CAP);
            // Unsent groups go back; deficits persist for the next flush.
            for (i, id) in indexes.iter().enumerate() {
                let s = inner.scopes.get_mut(id).expect("came from scopes");
                s.staged = std::mem::take(&mut views[i].groups);
                s.deficit = views[i].deficit;
            }
            let st = inner.streams.get(&STREAM).expect("stream seeded").clone();
            let mut planned = Vec::new();
            for (scope, groups) in plan {
                let s = inner.scopes.get(&scope).expect("planned from scopes");
                let durable_to = groups
                    .last()
                    .map(|g| g.end_lsn)
                    .expect("plan never returns empty group lists");
                planned.push(Planned {
                    scope,
                    groups,
                    epoch: s.epoch,
                    durable_to,
                });
            }
            (planned, controls, st)
        };

        // 2. Compose the frame (pure).
        let mut extents: Vec<Extent> = Vec::new();
        for p in &planned {
            let mut bytes = Vec::new();
            let mut start = Lsn(0);
            for (i, g) in p.groups.iter().enumerate() {
                if i == 0 {
                    start = Lsn(g.end_lsn.0 - g.bytes.len() as u64);
                }
                bytes.extend_from_slice(&g.bytes);
            }
            extents.push(Extent {
                scope: p.scope,
                epoch: p.epoch,
                start_lsn: start,
                key_version: 0, // plaintext until real sealing (§7 plumbing)
                kind: ExtentKind::Records,
                bytes,
            });
        }
        for c in &controls {
            extents.push(Extent {
                scope: c.scope,
                epoch: c.epoch,
                start_lsn: Lsn(0),
                key_version: 0,
                kind: ExtentKind::Control,
                bytes: c.encode(),
            });
        }
        let incarnation = {
            let inner = self.inner.lock().expect("store lock");
            inner.incarnation
        };
        let header = FrameHeader {
            stream: STREAM,
            segment_seq: st.cur_segment,
            frame_seq: st.next_frame_seq,
            incarnation,
            payload_len: 0, // encoder computes
            extent_count: extents.len() as u32,
        };
        let mut frame_scope_ids: Vec<ScopeId> = planned.iter().map(|p| p.scope).collect();
        for c in &controls {
            if !frame_scope_ids.contains(&c.scope) {
                frame_scope_ids.push(c.scope);
            }
        }
        let frame_bytes = frame::encode_frame(&header, &extents);

        // Roll the segment when this frame would not fit (§5.2).
        let (seg_seq, offset, frame_seq) = {
            let mut inner = self.inner.lock().expect("store lock");
            let s = inner.streams.get_mut(&STREAM).expect("stream");
            if s.write_offset + frame_bytes.len() as u64 > SEGMENT_SIZE {
                s.cur_segment = s.pos.next_segment_seq;
                s.pos.next_segment_seq += 1;
                s.write_offset = 0;
            }
            (s.cur_segment, s.write_offset, s.next_frame_seq)
        };

        // 3. Write + sync — no lock held.
        let local = self.dev.local();
        let span = cascade_telemetry::Span::noop();
        let ctx = OpCtx::new(cascade_storage_api::TenantContext::for_database(cascade_types::DatabaseId(0)), &span);
        let fname = FileName(format!("seg-{STREAM}-{seg_seq}"));
        let f = match local.open(&ctx, &fname, true).await {
            Ok(f) => f,
            Err(e) => {
                return self.fail_frame(frame_scope_ids.clone(), e).await;
            }
        };
        let mut frame_buf = cascade_types::AlignedBuf::zeroed(frame_bytes.len());
        frame_buf.as_mut_slice().copy_from_slice(&frame_bytes);
        if let Err(e) = local.write_at(&ctx, f, offset, frame_buf).await {
            return self.fail_frame(frame_scope_ids.clone(), e).await;
        }
        if let Err(e) = local.sync(&ctx, f).await {
            return self.fail_frame(frame_scope_ids.clone(), e).await;
        }

        // 4. Publish under the lock: durable LSNs, tail cache, index runs.
        {
            let mut inner = self.inner.lock().expect("store lock");
            let s = inner.streams.get_mut(&STREAM).expect("stream");
            s.next_frame_seq = frame_seq + 1;
            s.write_offset = offset + frame_bytes.len() as u64;
            for (p, ext) in planned.iter().zip(extents.iter()) {
                if ext.kind != ExtentKind::Records {
                    continue;
                }
                let scope = inner.scopes.get_mut(&p.scope).expect("planned scope");
                if scope.durable < p.durable_to {
                    scope.durable = p.durable_to;
                }
                scope.cache.extend_from_slice(&ext.bytes);
                scope.spans.push(AppendSpan {
                    start: ext.start_lsn,
                    len: ext.bytes.len(),
                    max_commit_ts: None, // flushed spans carry no ts (§4.4)
                });
                scope.runs.push(ExtentRun {
                    lsn_start: ext.start_lsn,
                    lsn_end: Lsn(ext.start_lsn.0 + ext.bytes.len() as u64),
                    stream: STREAM,
                    segment_seq: seg_seq,
                    offset,
                });
                // Keep cache_from consistent with the cache's reach.
                scope.cache_from = Lsn(scope.durable.0 - scope.cache.len() as u64);
            }
            for s in inner.scopes.values_mut() {
                let waiters = std::mem::take(&mut s.waiters);
                for (_, w) in waiters {
                    w.wake();
                }
            }
        }
        Ok(())
    }

    /// §4.3 promise 7 + S19: a failed write/sync fails the epoch for every
    /// scope in the frame (a frame holds extents of many scopes), refuses
    /// appends until reopen, discards the frame's never-acknowledged
    /// bytes (§8.1: fence first, then discard), and rolls the stream to a
    /// fresh segment — a poisoned file never sees another write (§5.7).
    async fn fail_frame(&self, scope_ids: Vec<ScopeId>, e: IoError) -> Result<(), StoreError> {
        let waiters: Vec<Waker> = {
            let mut inner = self.inner.lock().expect("store lock");
            for id in &scope_ids {
                if let Some(s) = inner.scopes.get_mut(id) {
                    s.failed = true;
                    s.staged.clear();
                    s.deficit = 0;
                }
            }
            let st = inner.streams.get_mut(&STREAM).expect("stream");
            st.cur_segment = st.pos.next_segment_seq;
            st.pos.next_segment_seq += 1;
            st.write_offset = 0;
            inner
                .scopes
                .values_mut()
                .flat_map(|s| std::mem::take(&mut s.waiters).into_iter().map(|(_, w)| w))
                .collect()
        };
        for w in waiters {
            w.wake();
        }
        let _ = e;
        Err(StoreError::Failed {
            scope: scope_ids.first().copied().unwrap_or(ScopeId(0)),
        })
    }
}


/// Reads a whole file from the flat namespace; `None` when absent.
async fn read_file<H: BlockHandle + ?Sized>(
    h: &H,
    ctx: &OpCtx<'_>,
    name: &str,
) -> Option<Vec<u8>> {
    let fname = FileName(name.into());
    let f = h.open(ctx, &fname, false).await.ok()?;
    let len = h.len(f).await.ok()?;
    let padded = (len as usize).div_ceil(PAD) * PAD;
    let buf = cascade_types::AlignedBuf::zeroed(padded.max(PAD));
    let back = h.read_at(ctx, f, 0, buf).await.ok()?;
    Some(back.as_slice()[..len as usize].to_vec())
}

/// Writes a whole file (create/truncate), padded to 4 KiB.
async fn write_file<H: BlockHandle + ?Sized>(
    h: &H,
    ctx: &OpCtx<'_>,
    name: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let fname = FileName(name.into());
    let f = h.open(ctx, &fname, true).await.map_err(ioe)?;
    let padded = bytes.len().div_ceil(PAD) * PAD;
    let mut buf = cascade_types::AlignedBuf::zeroed(padded.max(PAD));
    buf.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
    h.write_at(ctx, f, 0, buf).await.map_err(ioe)?;
    h.sync(ctx, f).await.map_err(ioe)?;
    Ok(())
}

/// §5.7: scan a stream from its recorded position. A frame is accepted
/// only if its CRCs hold, its segment_seq matches, and its frame_seq is
/// exactly one more than the previous frame's. The scan stops at the
/// first failure — torn (nothing valid after) unless a valid successor
/// appears at a 4 KiB boundary, which is corruption: synced-then-damaged.
async fn scan_stream<H: BlockHandle + ?Sized>(
    h: &H,
    ctx: &OpCtx<'_>,
    inner: &mut Inner,
    from: &StreamPosition,
) -> Result<(), StoreError> {
    let mut seq = from.scan_segment_seq;
    let mut offset = from.scan_offset;
    let mut expected_seq = from.scan_frame_seq;
    loop {
        let fname = FileName(format!("seg-{STREAM}-{seq}"));
        let f = match h.open(ctx, &fname, false).await {
            Ok(f) => f,
            Err(_) => break, // never written / recycled away: end of stream
        };
        let file_len = h.len(f).await.unwrap_or(0);
        loop {
            if offset + PAD as u64 > file_len {
                break; // end of segment
            }
            let head = match h
                .read_at(ctx, f, offset, cascade_types::AlignedBuf::zeroed(PAD))
                .await
            {
                Ok(b) => b,
                Err(_) => break,
            };
            let header = match frame::decode_header_only(&head.as_slice()[..FRAME_HEADER_LEN]) {
                Ok(hd) => hd,
                Err(_) => {
                    // Torn or damaged: is a valid successor at a 4 KiB
                    // boundary? A bounded probe (§5.7's window; slice 3
                    // bounds to 8 MiB).
                    let mut probe = offset + PAD as u64;
                    let limit = file_len.min(offset + 8 * 1024 * 1024);
                    let mut corruption = false;
                    while probe + PAD as u64 <= limit {
                        if let Ok(block) = h
                            .read_at(ctx, f, probe, cascade_types::AlignedBuf::zeroed(PAD))
                            .await
                        {
                            if let Ok(succ) =
                                frame::decode_header_only(&block.as_slice()[..FRAME_HEADER_LEN])
                            {
                                if succ.frame_seq == expected_seq && succ.segment_seq == seq {
                                    corruption = true;
                                    break;
                                }
                            }
                        }
                        probe += PAD as u64;
                    }
                    if corruption {
                        for s in inner.scopes.values_mut() {
                            s.failed = true;
                        }
                        return Err(StoreError::Corruption {
                            what: CorruptionSite::LogFrame {
                                scope: ScopeId(0),
                                at: Lsn(offset),
                            },
                        });
                    }
                    break; // torn tail: the stream resumes here
                }
            };
            if header.segment_seq != seq || header.frame_seq != expected_seq {
                break; // stale frame from an earlier incarnation of the segment
            }
            let total = FRAME_HEADER_LEN + header.payload_len as usize + 4;
            let padded_total = total + (PAD - total % PAD) % PAD;
            if offset + padded_total as u64 > file_len {
                break; // declared length runs past EOF: torn
            }
            let mut whole = cascade_types::AlignedBuf::zeroed(padded_total);
            whole.as_mut_slice()[..PAD].copy_from_slice(&head.as_slice()[..PAD]);
            if padded_total > PAD {
                let rest = h
                    .read_at(
                        ctx,
                        f,
                        offset + PAD as u64,
                        cascade_types::AlignedBuf::zeroed(padded_total - PAD),
                    )
                    .await;
                let rest = match rest {
                    Ok(b) => b,
                    Err(_) => break,
                };
                whole.as_mut_slice()[PAD..].copy_from_slice(rest.as_slice());
            }
            let decoded = match frame::decode_frame(whole.as_slice()) {
                Ok(d) => d,
                Err(_) => break, // trailer mismatch: treat as torn
            };
            // Apply the extents (rebuilds state; §5.7).
            for ext in &decoded.extents {
                let end = Lsn(ext.start_lsn.0 + ext.bytes.len() as u64);
                match ext.kind {
                    ExtentKind::Records => {
                        let s = inner.scopes.entry(ext.scope).or_default();
                        if s.durable < end {
                            s.durable = end;
                        }
                        if s.appended < end {
                            s.appended = end;
                        }
                        s.runs.push(ExtentRun {
                            lsn_start: ext.start_lsn,
                            lsn_end: end,
                            stream: STREAM,
                            segment_seq: seq,
                            offset,
                        });
                    }
                    ExtentKind::Control => {
                        let op = ControlOp::decode(&ext.bytes)?;
                        let s = inner.scopes.entry(op.scope).or_default();
                        match op.kind {
                            CTRL_CREATE | CTRL_EPOCH => {
                                s.epoch = op.epoch;
                                s.key_id = op.key_id;
                                s.dropped = false;
                                if inner.next_scope_id < op.scope.0 + 1 {
                                    inner.next_scope_id = op.scope.0 + 1;
                                }
                            }
                            CTRL_DROP => {
                                s.dropped = true;
                                s.epoch = Epoch(u64::MAX);
                            }
                            _ => {}
                        }
                    }
                }
            }
            expected_seq += 1;
            offset += padded_total as u64;
            if offset + PAD as u64 > SEGMENT_SIZE {
                // A full segment: roll; frame_seq continues.
                seq += 1;
                offset = 0;
                break;
            }
        }
        if offset != 0 {
            break; // the tail is torn: the stream resumes here
        }
    }
    // The scan is authoritative: rebuild the write head.
    if let Some(st) = inner.streams.get_mut(&STREAM) {
        st.pos.scan_segment_seq = seq;
        st.pos.scan_frame_seq = expected_seq;
        st.pos.scan_offset = offset;
        st.cur_segment = seq;
        st.write_offset = offset;
        st.next_frame_seq = expected_seq;
        if st.pos.next_segment_seq < seq + 1 {
            st.pos.next_segment_seq = seq + 1;
        }
    }
    // The scan is authoritative for anything it replayed; manifest state
    // stands for everything else (a manifest whose scan position is at
    // the end has nothing to replay — its durable LSN is the truth).
    for s in inner.scopes.values_mut() {
        s.appended = s.appended.max(s.durable);
        s.cache.clear();
        s.cache_from = s.durable;
    }
    Ok(())
}

fn ioe(e: IoError) -> StoreError {
    StoreError::Io(e.0)
}

fn floor_of(s: &ScopeState) -> Lsn {
    s.holders.values().copied().min().unwrap_or(Lsn(0))
}

/// Framing law for one staged group (§4.3 promise 4) — the same check the
/// fake runs, against the same walker.
fn check_framing(bytes: &[u8], start: Lsn, epoch: Epoch) -> Result<(), StoreError> {
    use cascade_storage_api::FrameError;
    if bytes.is_empty() {
        return Err(StoreError::BadFrame { at: start, reason: FrameError::BadLength });
    }
    let mut at = 0usize;
    let mut last = None;
    while at < bytes.len() {
        let f = walk(&bytes[at..]).map_err(|e| StoreError::BadFrame {
            at: Lsn(start.0 + at as u64),
            reason: match e {
                cascade_wal_format::WalkError::BadCrc => FrameError::BadCrc,
                cascade_wal_format::WalkError::Truncated
                | cascade_wal_format::WalkError::UnknownVersion(_) => FrameError::BadLength,
                cascade_wal_format::WalkError::UnknownFlags(_) => FrameError::UnknownFrame,
            },
        })?;
        if f.lsn != start.0 + at as u64 {
            return Err(StoreError::BadFrame {
                at: Lsn(start.0 + at as u64),
                reason: FrameError::BadLsn,
            });
        }
        if f.epoch != epoch.0 {
            return Err(StoreError::BadFrame {
                at: Lsn(start.0 + at as u64),
                reason: FrameError::BadEpoch,
            });
        }
        at += f.total;
        last = Some(f);
    }
    if at != bytes.len() {
        return Err(StoreError::BadFrame {
            at: Lsn(start.0 + at as u64),
            reason: FrameError::BadLength,
        });
    }
    let last = last.expect("non-empty checked");
    let ends_group = (last.format_version == 3 && last.group_end) || last.format_version == 2;
    if !ends_group {
        return Err(StoreError::BadFrame {
            at: Lsn(start.0 + at as u64),
            reason: FrameError::NotGroupEnd,
        });
    }
    Ok(())
}

impl<D: BlockDevice + 'static> LogStore<D> {

    /// Serves whole records from one scope's located runs (the shared read
    /// path of `read_log` and subscriptions). `from` must be a record
    /// boundary the reader learned from the log (§4.4 promise 3).
    async fn serve(
        &self,
        scope: ScopeId,
        st: &ScopeState,
        from: Lsn,
        max_bytes: usize,
        ctx: &OpCtx<'_>,
    ) -> Result<LogChunk, StoreError> {
        let floor = floor_of(st);
        if from < floor {
            return Err(StoreError::Truncated { floor });
        }
        if from >= st.durable {
            return Ok(LogChunk {
                start: from,
                bytes: Vec::new(),
                end: from,
                durable: st.durable,
            });
        }
        // Reads continue across runs until about `max_bytes` or the
        // durable LSN — an extent boundary is only an internal cut (it is
        // a group end, §5.4), never a chunk boundary the caller chases.
        let serve_until = st.durable.0.min(from.0 + max_bytes.max(1) as u64);
        let local = self.dev.local();
        let mut out: Vec<u8> = Vec::new();
        let mut pos = from;
        while (pos.0 as usize) < serve_until as usize {
            let run = st
                .runs
                .iter()
                .find(|r| r.lsn_start <= pos && pos < r.lsn_end)
                .ok_or(StoreError::NotRecordBoundary { at: pos })?
                .clone();
            let fname = FileName(format!("seg-{STREAM}-{}", run.segment_seq));
            let f = local.open(ctx, &fname, false).await.map_err(ioe)?;
            let head = local
                .read_at(ctx, f, run.offset, cascade_types::AlignedBuf::zeroed(PAD))
                .await
                .map_err(ioe)?;
            let header = frame::decode_header_only(&head.as_slice()[..FRAME_HEADER_LEN])
                .map_err(|_| StoreError::Corruption {
                    what: CorruptionSite::LogFrame { scope, at: Lsn(run.offset) },
                })?;
            let total = FRAME_HEADER_LEN + header.payload_len as usize + 4;
            let padded_total = total + (PAD - total % PAD) % PAD;
            let mut whole = cascade_types::AlignedBuf::zeroed(padded_total);
            whole.as_mut_slice()[..PAD].copy_from_slice(&head.as_slice()[..PAD]);
            if padded_total > PAD {
                let rest = local
                    .read_at(
                        ctx,
                        f,
                        run.offset + PAD as u64,
                        cascade_types::AlignedBuf::zeroed(padded_total - PAD),
                    )
                    .await
                    .map_err(ioe)?;
                whole.as_mut_slice()[PAD..].copy_from_slice(rest.as_slice());
            }
            let decoded = frame::decode_frame(whole.as_slice())
                .map_err(|_| StoreError::Corruption {
                    what: CorruptionSite::LogFrame { scope, at: Lsn(run.offset) },
                })?;
            // The extent carrying `pos`.
            let mut found: Option<(usize, Vec<u8>, Lsn)> = None; // payload offset, bytes, start lsn
            let mut at = FRAME_HEADER_LEN as u64;
            for ext in &decoded.extents {
                let elen = ext.bytes.len() as u64;
                if ext.kind == ExtentKind::Records
                    && ext.scope == scope
                    && ext.start_lsn <= pos
                    && pos < Lsn(ext.start_lsn.0 + elen)
                {
                    found = Some((at as usize, ext.bytes.clone(), ext.start_lsn));
                    break;
                }
                at += 40 + elen;
            }
            let (_, bytes, ext_start) =
                found.ok_or(StoreError::NotRecordBoundary { at: pos })?;
            let in_extent = (pos.0 - ext_start.0) as usize;
            // Every position must be a record boundary (§4.4 P3); later
            // extents start at group ends, so their walk is trivial.
            let mut rec_at = 0usize;
            let mut boundary = false;
            while rec_at <= in_extent && rec_at < bytes.len() {
                if rec_at == in_extent {
                    boundary = true;
                    break;
                }
                let f = walk(&bytes[rec_at..]).map_err(|_| StoreError::Corruption {
                    what: CorruptionSite::LogFrame { scope, at: pos },
                })?;
                rec_at += f.total;
            }
            if !boundary {
                return Err(StoreError::NotRecordBoundary { at: pos });
            }
            // Whole records, at least one, bounded by the extent end (a
            // group end, §5.4) and the durable LSN (§4.4 P1).
            let mut end = in_extent;
            while (ext_start.0 + end as u64) < serve_until && end < bytes.len() {
                let f = walk(&bytes[end..]).map_err(|_| StoreError::Corruption {
                    what: CorruptionSite::LogFrame { scope, at: pos },
                })?;
                end += f.total;
            }
            out.extend_from_slice(&bytes[in_extent..end]);
            pos = Lsn(ext_start.0 + end as u64);
        }
        Ok(LogChunk {
            start: from,
            bytes: out,
            end: pos,
            durable: st.durable,
        })
    }

    async fn flush_until(&self, scope: ScopeId, lsn: Lsn) -> Result<(), StoreError> {
        loop {
            {
                let inner = self.inner.lock().expect("store lock");
                let s = inner.scopes.get(&scope).ok_or(StoreError::UnknownScope(scope))?;
                if s.failed {
                    return Err(StoreError::Failed { scope });
                }
                if s.durable >= lsn {
                    return Ok(());
                }
            }
            let before = {
                let inner = self.inner.lock().expect("store lock");
                inner.scopes.get(&scope).map(|s| s.durable).unwrap_or(Lsn(0))
            };
            // Waiting is what triggers a flush (§4.3).
            self.flush_some().await?;
            let after = {
                let inner = self.inner.lock().expect("store lock");
                inner.scopes.get(&scope).map(|s| s.durable).unwrap_or(Lsn(0))
            };
            if after >= lsn {
                return Ok(());
            }
            if after == before {
                // No progress and no failure: the request reaches past
                // everything staged — park until another task's flush
                // wakes us (a single-task suite never reaches here).
                futures_lite::future::poll_fn(|cx| {
                    let mut inner = self.inner.lock().expect("store lock");
                    if let Some(s) = inner.scopes.get_mut(&scope) {
                        if s.durable >= lsn {
                            return Poll::Ready(());
                        }
                        s.waiters.push((lsn, cx.waker().clone()));
                    }
                    Poll::Pending
                })
                .await;
            }
        }
    }
}


impl<D: BlockDevice + 'static> LogStore<D> {
    fn self_arc(&self) -> Result<Arc<Self>, StoreError> {
        self.me
            .get()
            .and_then(|w| w.upgrade())
            .ok_or(StoreError::Unavailable)
    }
}

impl<D: BlockDevice + 'static> LogSink for LogStore<D> {
    fn open_writer<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        epoch: Epoch,
    ) -> cascade_types::LocalBoxFuture<'a, Result<Lsn, StoreError>> {
        Box::pin(async move {
            let key_id = {
                let mut inner = self.inner.lock().expect("store lock");
                let s = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
                if s.dropped {
                    return Err(StoreError::Fenced { current: s.epoch });
                }
                if epoch == s.epoch {
                    // A retried open returns the durable LSN unchanged
                    // (§4.3 promise 1).
                    return Ok(s.durable);
                }
                if epoch < s.epoch {
                    return Err(StoreError::Fenced { current: s.epoch });
                }
                // A new epoch discards accepted-but-unflushed bytes from
                // older epochs — they were never acknowledged (§4.3
                // promise 1) — so the contiguity frontier falls back to
                // the durable LSN, and their spans die with them. Clears
                // a failed epoch too (§8.1 step 4).
                s.staged.clear();
                s.deficit = 0;
                s.appended = s.durable;
                s.spans.retain(|sp| sp.start < s.durable);
                s.failed = false;
                // The epoch applies in memory NOW — the control extent
                // (staged below) is what makes it durable.
                s.epoch = epoch;
                let key_id = s.key_id;
                inner.staged_controls.push(ControlOp {
                    kind: CTRL_EPOCH,
                    scope,
                    epoch,
                    key_id,
                });
                key_id
            };
            let _ = key_id;
            self.flush_some().await?;
            self.write_manifest().await?;
            let inner = self.inner.lock().expect("store lock");
            Ok(inner.scopes.get(&scope).map(|s| s.durable).unwrap_or(Lsn(0)))
        })
    }

    fn append<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        epoch: Epoch,
        start: Lsn,
        bytes: &'a [u8],
        meta: cascade_storage_api::AppendMeta,
    ) -> cascade_types::LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("store lock");
            let s = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            if s.dropped {
                return Err(StoreError::Fenced { current: s.epoch });
            }
            if s.failed {
                return Err(StoreError::Failed { scope });
            }
            if epoch < s.epoch {
                return Err(StoreError::Fenced { current: s.epoch });
            }
            // The epoch check happened before anything else mutable — a
            // re-send from a fenced epoch fails Fenced, not Conflict.
            check_framing(bytes, start, epoch)?;
            let len = bytes.len();
            if start > s.appended {
                return Err(StoreError::Gap { expected: s.appended });
            }
            if start < s.appended {
                // Overlap: identical bytes only (§4.3 promise 3), against
                // the recent tail (§5.3) for durable bytes and the staged
                // queue for accepted-but-unflushed ones.
                let overlap_end = (start.0 as usize + len).min(s.appended.0 as usize);
                let given = &bytes[..overlap_end - start.0 as usize];
                // Durable region: [max(start, cache_from), min(overlap_end, durable)).
                let durable_lo = start.0.max(s.cache_from.0) as usize;
                let durable_hi = overlap_end.min(s.durable.0 as usize);
                if durable_lo < durable_hi {
                    let cache_off = durable_lo - s.cache_from.0 as usize;
                    let cached = &s.cache[cache_off..cache_off + (durable_hi - durable_lo)];
                    let given_slice = &given[durable_lo - start.0 as usize..durable_hi - start.0 as usize];
                    if cached != given_slice {
                        return Err(StoreError::Conflict { at: start });
                    }
                }
                // Staged region: [max(start, durable), overlap_end).
                let staged_lo = (start.0.max(s.durable.0)) as usize;
                if staged_lo < overlap_end {
                    let mut staged_bytes = Vec::with_capacity(overlap_end - staged_lo);
                    let mut cursor = s.durable.0;
                    for g in &s.staged {
                        let g_start = cursor;
                        let g_end = cursor + g.bytes.len() as u64;
                        if (staged_lo as u64) < g_end && g_start < (overlap_end as u64) {
                            let lo = (staged_lo as u64).max(g_start);
                            let hi = (overlap_end as u64).min(g_end);
                            let off = (lo - g_start) as usize;
                            let take = (hi - lo) as usize;
                            staged_bytes.extend_from_slice(&g.bytes[off..off + take]);
                        }
                        cursor = g_end;
                    }
                    if staged_bytes.as_slice() != &given[staged_lo - start.0 as usize..] {
                        return Err(StoreError::Conflict { at: start });
                    }
                }
                if start.0 as usize + len <= s.appended.0 as usize {
                    return Ok(()); // pure re-send: no further effect
                }
                // Identical overlap with an extension: stage the suffix.
                let suffix_at = s.appended.0 as usize - start.0 as usize;
                let suffix = &bytes[suffix_at..];
                s.staged.push_back(PendingGroup {
                    bytes: suffix.to_vec(),
                    end_lsn: Lsn(start.0 + len as u64),
                });
                s.appended = Lsn(start.0 + len as u64);
                s.spans.push(AppendSpan {
                    start: Lsn(start.0 + suffix_at as u64),
                    len: suffix.len(),
                    max_commit_ts: meta.max_commit_ts,
                });
                return Ok(());
            }
            // Fresh append: stage as one group (S16 — the planner never
            // splits it).
            s.staged.push_back(PendingGroup {
                bytes: bytes.to_vec(),
                end_lsn: Lsn(start.0 + len as u64),
            });
            s.appended = Lsn(start.0 + len as u64);
            s.spans.push(AppendSpan {
                start,
                len,
                max_commit_ts: meta.max_commit_ts,
            });
            Ok(())
        })
    }

    fn durable_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        inner
            .scopes
            .get(&scope)
            .map(|s| s.durable)
            .ok_or(StoreError::UnknownScope(scope))
    }

    fn wait_durable<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        lsn: Lsn,
    ) -> cascade_types::LocalBoxFuture<'a, Result<Lsn, StoreError>> {
        Box::pin(async move {
            self.flush_until(scope, lsn).await?;
            let inner = self.inner.lock().expect("store lock");
            inner
                .scopes
                .get(&scope)
                .map(|s| s.durable)
                .ok_or(StoreError::UnknownScope(scope))
        })
    }
}

impl<D: BlockDevice + 'static> LogStore<D> {
    /// The appended watermark — a diagnostic the noisy-neighbor test
    /// reads (how much a scope has staged beyond its durable LSN).
    pub fn appended_lsn(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        inner
            .scopes
            .get(&scope)
            .map(|s| s.appended)
            .ok_or(StoreError::UnknownScope(scope))
    }
}

impl<D: BlockDevice + 'static> LogSource for LogStore<D> {
    fn read_log<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        from: Lsn,
        max_bytes: usize,
    ) -> cascade_types::LocalBoxFuture<'a, Result<LogChunk, StoreError>> {
        Box::pin(async move {
            let st = {
                let inner = self.inner.lock().expect("store lock");
                inner
                    .scopes
                    .get(&scope)
                    .cloned()
                    .ok_or(StoreError::UnknownScope(scope))?
            };
            self.serve(scope, &st, from, max_bytes, ctx).await
        })
    }

    fn subscribe(
        &self,
        _ctx: &OpCtx<'_>,
        scope: ScopeId,
        from: Lsn,
    ) -> Box<dyn LogSubscription> {
        let store = self.self_arc().expect("subscribe on a live store");
        Box::new(StoreSubscription {
            store,
            scope,
            pos: from,
        })
    }

    fn set_retained_from<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        holder: RetentionHolder,
        lsn: Lsn,
    ) -> cascade_types::LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("store lock");
            let s = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            // The floor never regresses: a holder's value only moves
            // forward, a new holder registers at or above it (§4.4 P4).
            let clamped = lsn.max(floor_of(s));
            s.holders.insert(holder, clamped);
            Ok(())
        })
    }

    fn release_holder<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        holder: RetentionHolder,
    ) -> cascade_types::LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("store lock");
            let s = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
            s.holders.remove(&holder);
            Ok(())
        })
    }

    fn retained_floor(&self, scope: ScopeId) -> Result<Lsn, StoreError> {
        let inner = self.inner.lock().expect("store lock");
        inner
            .scopes
            .get(&scope)
            .map(floor_of)
            .ok_or(StoreError::UnknownScope(scope))
    }

    fn append_before_time<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        ts: Hlc,
    ) -> cascade_types::LocalBoxFuture<'a, Result<Lsn, StoreError>> {
        Box::pin(async move {
            let inner = self.inner.lock().expect("store lock");
            let s = inner.scopes.get(&scope).ok_or(StoreError::UnknownScope(scope))?;
            let answer = s
                .spans
                .iter()
                .find(|sp| sp.max_commit_ts.is_some_and(|t| t > ts))
                .map(|sp| sp.start)
                .unwrap_or(s.durable);
            let floor = floor_of(s);
            if answer < floor {
                return Err(StoreError::Truncated { floor });
            }
            Ok(answer)
        })
    }
}

/// A subscription cursor (§5.5): reads what is durable, triggers a flush
/// at the end, and parks until the next advance.
struct StoreSubscription<D: BlockDevice + 'static> {
    store: Arc<LogStore<D>>,
    scope: ScopeId,
    pos: Lsn,
}

impl<D: BlockDevice + 'static> LogSubscription for StoreSubscription<D> {
    fn next(&mut self) -> cascade_types::LocalBoxFuture<'_, Result<LogChunk, StoreError>> {
        Box::pin(async move {
            loop {
                let chunk = {
                    let st = {
                        let inner = self.store.inner.lock().expect("store lock");
                        inner
                            .scopes
                            .get(&self.scope)
                            .cloned()
                            .ok_or(StoreError::UnknownScope(self.scope))?
                    };
                    if st.failed {
                        return Err(StoreError::Failed { scope: self.scope });
                    }
                    if self.pos < st.durable {
                        let span = cascade_telemetry::Span::noop();
                        let ctx = test_ctx(&span, 0);
                        Some(
                            self.store
                                .serve(self.scope, &st, self.pos, 1024 * 1024, &ctx)
                                .await?,
                        )
                    } else {
                        None
                    }
                };
                if let Some(chunk) = chunk {
                    self.pos = chunk.end;
                    return Ok(chunk);
                }
                // At the durable LSN: waiting triggers a flush (§4.3),
                // then serve; only a truly idle scope parks.
                self.store.flush_until(self.scope, self.pos).await?;
                {
                    let inner = self.store.inner.lock().expect("store lock");
                    let st = inner
                        .scopes
                        .get(&self.scope)
                        .ok_or(StoreError::UnknownScope(self.scope))?;
                    if self.pos < st.durable {
                        continue;
                    }
                }
                futures_lite::future::poll_fn(|cx| {
                    let mut inner = self.store.inner.lock().expect("store lock");
                    if let Some(s) = inner.scopes.get_mut(&self.scope) {
                        if self.pos < s.durable {
                            return Poll::Ready(());
                        }
                        s.waiters.push((self.pos, cx.waker().clone()));
                    }
                    Poll::Pending
                })
                .await;
            }
        })
    }
}

impl<D: BlockDevice + 'static> LogScopes for LogStore<D> {
    fn create_scope<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
        initial: Epoch,
        key: KeyRef,
    ) -> cascade_types::LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let initial = Epoch::issued(initial.0);
            {
                let mut inner = self.inner.lock().expect("store lock");
                if inner.scopes.contains_key(&scope) {
                    return Err(StoreError::ScopeExists(scope));
                }
                if scope.0 >= inner.next_scope_id {
                    inner.next_scope_id = scope.0 + 1;
                }
                inner.scopes.insert(
                    scope,
                    ScopeState {
                        epoch: initial,
                        key_id: key.key_id,
                        ..ScopeState::default()
                    },
                );
                inner.staged_controls.push(ControlOp {
                    kind: CTRL_CREATE,
                    scope,
                    epoch: initial,
                    key_id: key.key_id,
                });
            }
            // The creation is durable before create returns (a control
            // extent — fencing's durability rides the same law, §5.2).
            self.flush_some().await?;
            self.write_manifest().await?;
            Ok(())
        })
    }

    fn drop_scope<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        scope: ScopeId,
    ) -> cascade_types::LocalBoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            {
                let mut inner = self.inner.lock().expect("store lock");
                let s = inner.scopes.get_mut(&scope).ok_or(StoreError::UnknownScope(scope))?;
                // Fence every epoch at once.
                s.epoch = Epoch(u64::MAX);
                let waiters = std::mem::take(&mut s.waiters);
                for (_, w) in waiters {
                    w.wake();
                }
                if s.holders.is_empty() {
                    inner.scopes.remove(&scope);
                } else {
                    // Freed once no holder remains (§4.6).
                    s.dropped = true;
                }
                inner.staged_controls.push(ControlOp {
                    kind: CTRL_DROP,
                    scope,
                    epoch: Epoch(u64::MAX),
                    key_id: 0,
                });
            }
            self.flush_some().await?;
            self.write_manifest().await?;
            Ok(())
        })
    }
}
