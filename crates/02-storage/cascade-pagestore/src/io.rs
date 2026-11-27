//! Device-level I/O: everything in the page store that touches a
//! [`BlockHandle`], in one place.
//!
//! Two rules hold for all of it.
//!
//! * **Nothing here takes a store lock.** The state lock is never held
//!   across a device call. Where a read must not meet a flush — a page
//!   record shares its 4 KiB block with 63 others — the caller holds the
//!   home gate around it (see [`read_page`]).
//! * **A short or absent read is an answer, never a guess.** A page no
//!   record describes was never written. A record without its image, or an
//!   image that fails the checksum its record holds, is damage, and is
//!   reported as the page it belongs to.

use std::collections::HashMap;

use cascade_storage_api::{
    BlockHandle, FileName, FileId, IoError, OpCtx, PageKey, StoreError,
};
use cascade_types::{AlignedBuf, Lsn, PageNo, PAGE_SIZE, ScopeId, SegmentId};

use crate::layout::{
    decode_dw_header, decode_page_record, dw_header_blocks, meta_file, page_file, parse_meta_file,
    parse_page_file, LayoutError, Manifest, BLOCK, DBLWRITE_NAME, MANIFEST_NAME, PAGE_REC_LEN,
    PAGE_RECS_PER_BLOCK,
};
use crate::{manifest_damage, page_damage, BatchEntry, PageData};

/// A device error, in the contract's error type.
pub(crate) fn io_err(e: IoError) -> StoreError {
    StoreError::Io(e.0)
}

/// The page files in a directory listing, as `(scope, segment, is a
/// record file)`. Everything else in the namespace — the manifest, the
/// doublewrite area, another store's files — is not a page file.
pub(crate) fn page_files(names: &[FileName]) -> Vec<(ScopeId, SegmentId, bool)> {
    names
        .iter()
        .filter_map(|name| {
            parse_meta_file(&name.0)
                .map(|(scope, segment)| (scope, segment, true))
                .or_else(|| {
                    parse_page_file(&name.0).map(|(scope, segment)| (scope, segment, false))
                })
        })
        .collect()
}

/// Writes `bytes` at `off`. The device takes whole 4 KiB blocks; the tail
/// of the last block is zero-filled.
pub(crate) async fn write_bytes(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
    f: FileId,
    off: u64,
    bytes: &[u8],
) -> Result<(), StoreError> {
    if bytes.is_empty() {
        // Nothing to write; an aligned buffer cannot be empty.
        return Ok(());
    }
    let len = bytes.len().div_ceil(BLOCK) * BLOCK;
    let mut buf = AlignedBuf::zeroed(len);
    buf.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
    handle
        .write_at(ctx, f, off, buf)
        .await
        .map(|_| ())
        .map_err(io_err)
}

/// Reads exactly `len` bytes at `off`.
pub(crate) async fn read_range(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
    f: FileId,
    off: u64,
    len: usize,
) -> Result<AlignedBuf, StoreError> {
    let buf = AlignedBuf::zeroed(len);
    handle.read_at(ctx, f, off, buf).await.map_err(io_err)
}

/// Reads `len` bytes at `off`, or `None` where the file does not reach.
pub(crate) async fn read_if_present(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
    f: FileId,
    off: u64,
    len: usize,
) -> Result<Option<AlignedBuf>, StoreError> {
    let file_len = handle.len(f).await.map_err(io_err)?;
    if off.saturating_add(len as u64) > file_len {
        return Ok(None);
    }
    read_range(handle, ctx, f, off, len).await.map(Some)
}

/// Opens a file; `None` when it does not exist and `create` is unset.
pub(crate) async fn open_file(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
    name: &FileName,
    create: bool,
) -> Result<Option<FileId>, StoreError> {
    match handle.open(ctx, name, create).await {
        Ok(f) => Ok(Some(f)),
        Err(e) if !create && e.0.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(e)),
    }
}

/// Opens a file once per batch: a batch's writes to one file are collected
/// and synced together, so each file is opened a single time.
pub(crate) async fn open_cached(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
    files: &mut HashMap<FileName, FileId>,
    name: FileName,
    create: bool,
) -> Result<FileId, StoreError> {
    if let Some(f) = files.get(&name) {
        return Ok(*f);
    }
    let f = handle.open(ctx, &name, create).await.map_err(io_err)?;
    files.insert(name, f);
    Ok(f)
}

/// A page from its home files: the record beside it, then the image, each
/// checked against the other.
///
/// `Ok(None)` is a page no record describes — never written (§4.5 promise
/// 4). Anything else that is not a whole, matching pair is `Corruption`
/// naming the page: a record that fails its checksum, an image that fails
/// the checksum its record holds, or a record whose image has gone.
///
/// **The caller holds the home gate**, or is `open`, where nothing else
/// runs: a page record shares a 4 KiB block with 63 others, and a flush may
/// be rewriting some of them.
pub(crate) async fn read_page(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
    scope: ScopeId,
    key: PageKey,
) -> Result<Option<PageData>, StoreError> {
    let segment = key.segment;
    let Some(meta) = open_file(handle, ctx, &meta_file(scope, segment), false).await? else {
        return Ok(None);
    };
    let block = key.page_no as usize / PAGE_RECS_PER_BLOCK;
    let Some(blk) = read_if_present(handle, ctx, meta, block as u64 * BLOCK as u64, BLOCK).await?
    else {
        return Ok(None);
    };
    let slot = key.page_no as usize % PAGE_RECS_PER_BLOCK;
    let record = &blk.as_slice()[slot * PAGE_REC_LEN..(slot + 1) * PAGE_REC_LEN];
    let record = match decode_page_record(record) {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(None),
        // A record of a version this build does not read describes a page
        // this build cannot serve either.
        Err(_) => return Err(page_damage(scope, key)),
    };
    let Some(images) = open_file(handle, ctx, &page_file(scope, segment), false).await? else {
        // The record exists, and says the page was written.
        return Err(page_damage(scope, key));
    };
    let Some(image) =
        read_if_present(handle, ctx, images, key.page_no as u64 * PAGE_SIZE as u64, PAGE_SIZE)
            .await?
    else {
        return Err(page_damage(scope, key));
    };
    if crc(image.as_slice()) != record.image_crc {
        return Err(page_damage(scope, key));
    }
    Ok(Some(PageData {
        image: image.as_slice().to_vec(),
        page_lsn: record.page_lsn,
    }))
}

/// Whether a batch entry's page is already in its home files, exactly.
/// Recovery uses it to rewrite what a crash damaged, and no more. Any read
/// error counts as "not there": the page is rewritten, and if *that*
/// fails, that is the real failure.
pub(crate) async fn home_is_current(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
    scope: ScopeId,
    entry: &BatchEntry,
) -> bool {
    match read_page(handle, ctx, scope, entry.key).await {
        Ok(Some(home)) => home.page_lsn == entry.page_lsn && crc(&home.image) == crc(&entry.image),
        _ => false,
    }
}

/// The manifest, or `None` on a device that has never had one.
pub(crate) async fn read_manifest(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
) -> Result<Option<Manifest>, StoreError> {
    let Some(f) = open_file(handle, ctx, &FileName(MANIFEST_NAME.into()), false).await? else {
        return Ok(None);
    };
    let Some(buf) = read_if_present(handle, ctx, f, 0, BLOCK).await? else {
        return Err(manifest_damage());
    };
    match crate::layout::decode_manifest(buf.as_slice()) {
        Ok(manifest) => Ok(Some(manifest)),
        Err(LayoutError::Corrupt | LayoutError::TooLarge) => Err(manifest_damage()),
        Err(LayoutError::UnknownVersion(_)) => Err(StoreError::Unsupported(
            "a page store manifest version this build does not read",
        )),
    }
}

/// The doublewrite batch in flight, with its page images — or `None`.
///
/// `None` covers every state that is not *a batch whose home copies may
/// have been begun*: no file, a cleared region, a header that fails its
/// checksum, and a header whose images do not match the checksums it
/// holds. That last is a batch being written when the crash came: its sync
/// never completed, so its home copies were never begun, and the previous
/// consistent state stands untouched.
///
/// The header is read in the length *it* says it is, never in the one the
/// current configuration asks for: a batch in flight when the
/// configuration changed must still be found and finished.
pub(crate) async fn read_dw_batch(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
) -> Result<Option<(ScopeId, Lsn, Vec<BatchEntry>)>, StoreError> {
    let Some(f) = open_file(handle, ctx, &FileName(DBLWRITE_NAME.into()), false).await? else {
        return Ok(None);
    };
    let Some(first) = read_if_present(handle, ctx, f, 0, BLOCK).await? else {
        return Ok(None);
    };
    let Ok(blocks) = dw_header_blocks(first.as_slice()) else {
        return Ok(None);
    };
    let header_len = blocks * BLOCK;
    let Some(header) = read_if_present(handle, ctx, f, 0, header_len).await? else {
        return Ok(None);
    };
    let Ok(batch) = decode_dw_header(header.as_slice()) else {
        return Ok(None);
    };
    if batch.entries.is_empty() {
        return Ok(None);
    }
    let mut entries = Vec::with_capacity(batch.entries.len());
    for (i, e) in batch.entries.iter().enumerate() {
        let off = header_len as u64 + i as u64 * PAGE_SIZE as u64;
        let Some(image) = read_if_present(handle, ctx, f, off, PAGE_SIZE).await? else {
            return Ok(None);
        };
        let image = image.as_slice();
        if crc(image) != e.image_crc {
            return Ok(None);
        }
        entries.push(BatchEntry {
            key: PageKey {
                segment: e.segment,
                page_no: e.page_no,
            },
            image: image.to_vec(),
            page_lsn: e.page_lsn,
        });
    }
    Ok(Some((batch.scope, batch.ingested, entries)))
}

/// The highest page of a segment that has a record on disk, found by
/// walking the record file backwards from its end. A record that is
/// present but fails its checksum is damage, and names its page.
pub(crate) async fn scan_high(
    handle: &dyn BlockHandle,
    ctx: &OpCtx<'_>,
    scope: ScopeId,
    segment: SegmentId,
    slots: u32,
) -> Result<Option<PageNo>, StoreError> {
    let first = PageKey {
        segment,
        page_no: 0,
    };
    let f = open_file(handle, ctx, &meta_file(scope, segment), false)
        .await?
        .ok_or_else(|| page_damage(scope, first))?;
    // Start at the block holding the last record: `slots` need not be a
    // whole number of blocks. `65 / 64` would start at block 0 and never
    // look at record 64 — reporting a segment's open page as the one
    // before it, and routing every later insert by a page that is full.
    let mut block = (slots as usize).div_ceil(PAGE_RECS_PER_BLOCK);
    while block > 0 {
        block -= 1;
        let blk = read_range(handle, ctx, f, block as u64 * BLOCK as u64, BLOCK).await?;
        for slot in (0..PAGE_RECS_PER_BLOCK).rev() {
            let index = block * PAGE_RECS_PER_BLOCK + slot;
            if index >= slots as usize {
                continue;
            }
            let record = &blk.as_slice()[slot * PAGE_REC_LEN..(slot + 1) * PAGE_REC_LEN];
            let page = PageKey {
                segment,
                page_no: index as PageNo,
            };
            match decode_page_record(record) {
                Ok(Some(_)) => return Ok(Some(index as PageNo)),
                Ok(None) => continue,
                Err(_) => return Err(page_damage(scope, page)),
            }
        }
    }
    Ok(None)
}

fn crc(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{encode_page_record, PageRecord};
    use cascade_storage_api::fake::{test_ctx, MemBlockDevice};
    use cascade_storage_api::BlockDevice as _;
    use futures_lite::future::block_on;

    /// `slots` need not be a whole number of record blocks. With 65
    /// records the highest page is 64, which lives in the second block:
    /// starting the scan at `65 / 64` would never look at it and report
    /// page 63 as the segment's open page — a full page, which every later
    /// insert would then be routed by.
    #[test]
    fn scan_high_finds_the_last_record_whatever_the_count() {
        let span = cascade_telemetry::Span::noop();
        let ctx = test_ctx(&span, 1);
        block_on(async {
            let dev = MemBlockDevice::new();
            let handle = dev.local();
            let (scope, segment) = (ScopeId(7), SegmentId(3));
            let f = handle
                .open(&ctx, &meta_file(scope, segment), true)
                .await
                .expect("creates the record file");
            // Two record blocks: pages 0..=63, then page 64 and nothing.
            let mut records = AlignedBuf::zeroed(2 * BLOCK);
            for page_no in 0..65u32 {
                let image = [page_no as u8; PAGE_SIZE];
                let record =
                    encode_page_record(&PageRecord::seal(&image, Lsn(page_no as u64 * 100)));
                let at = page_no as usize * PAGE_REC_LEN;
                records.as_mut_slice()[at..at + PAGE_REC_LEN].copy_from_slice(&record);
            }
            handle.write_at(&ctx, f, 0, records).await.expect("writes");

            for (slots, want) in [
                (0u32, None),
                (1, Some(0)),
                (63, Some(62)),
                (64, Some(63)),
                (65, Some(64)),
                (128, Some(64)),
            ] {
                assert_eq!(
                    scan_high(&*handle, &ctx, scope, segment, slots)
                        .await
                        .expect("scans"),
                    want,
                    "slots {slots}"
                );
            }
        });
    }
}
