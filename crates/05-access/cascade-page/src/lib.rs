//! 8 KiB page layout for Cascade DB — the format authority every access
//! method shares (T6.1, FR-39).
//!
//! The on-disk unit is `PAGE_SIZE` raw bytes; the layout below is defined by
//! byte offsets in **little-endian**, fixed at this version. There is no
//! `#[repr(C)]` header struct on purpose: the bytes are the format, and the
//! accessors below are its only legal readers/writers.
//!
//! ```text
//! offset   0: format_version  u16   (1)
//! offset   2: page_kind       u16   (1 = heap; btree/undo/columnar land later)
//! offset   4: slot_count      u16
//! offset   6: free_offset     u16   cells occupy [free_offset, PAGE_SIZE)
//! offset   8: page_lsn        u64   high-water WAL boundary of the page's
//!                                   content (invariant 2). ZERO in M6: the
//!                                   BPM frame stamp is the authoritative
//!                                   writeback boundary while no recovery
//!                                   reads page images yet; the field is
//!                                   format-complete so replay-from-pages
//!                                   needs no format change.
//! offset  16: reserved 16 bytes (zero)
//! offset  32: slot array — `slot_count` × u16 LE cell offsets, growing up
//!             from [`SLOTS_BASE`].
//! tail:     cells growing DOWN from `PAGE_SIZE`.
//! ```
//!
//! A cell is `[len u16][xmin u64][xmax u64][next u64][payload..]` where
//! `len` counts the whole cell (`CELL_FIXED + payload.len()`); `xmin`/`xmax`
//! are the MVCC version words ([`VersionWord`], WP1 F1 — upstream §12's
//! creator and deleter), and `next` is the packed chain word ([`NextWord`],
//! WP1 F2 — §12's next-tuple pointer plus flags; the heap has no undo, so
//! the v1 `undo_ref` reservation was repurposed without moving a byte). A
//! fresh cell stamps `xmax`/`next` zero: live, chain ends. The payload is
//! opaque here — heap row encoding is `cascade-heap`'s (out of scope, per
//! the crate's AI/CONTEXT).
//!
//! Checksum division (invariant 1): the OUTER integrity CRC lives in
//! `cascade-storage` — every stored page image is
//! `[u32 LE CRC32 of the rest][payload]`, recomputed on write and validated
//! on read. This crate deliberately carries no checksum field of its own;
//! nesting two CRCs over the same bytes would double the validation cost and
//! blur which layer answers for integrity.

use thiserror::Error;

use cascade_types::{PageNo, VersionWord, PAGE_SIZE};

/// Format version this crate reads and writes.
pub const FORMAT_VERSION: u16 = 1;

/// `page_kind` for heap pages.
pub const PAGE_KIND_HEAP: u16 = 1;

/// Fixed header size; the slot array begins here.
pub const HEADER_LEN: usize = 32;

/// First slot's byte offset (`slot i` lives at `SLOTS_BASE + 2*i`).
pub const SLOTS_BASE: usize = HEADER_LEN;

/// The fixed part of a cell: `[len u16][xmin u64][xmax u64][next u64]`.
pub const CELL_FIXED: usize = 2 + 8 + 8 + 8;

/// `next`-word flag (WP1 F2): the chain continues — without it the rest of
/// the word is ignored, so the all-zero word unambiguously means "this
/// tuple ends its chain".
pub const NEXT_HAS_NEXT: u16 = 1 << 0;
/// `next`-word flag (§12): HOT chain member — reachable only through the
/// chain, never by an index entry.
pub const NEXT_HEAP_ONLY: u16 = 1 << 1;
/// `next`-word flag (§12): the tuple carries out-of-line TOASTed columns.
pub const NEXT_HAS_TOAST: u16 = 1 << 2;

/// The packed chain word occupying the cell's third field (`next`, WP1 F2 —
/// upstream §12's next-tuple pointer; the heap has no undo, so chains run
/// through tuple identity while undo chains stay an IOT concern).
///
/// Bit layout (LE u64): bits 0..15 slot, bits 16..47 relation-local
/// `PageNo` (a chain never leaves its relation), bits 48..63 flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextWord(pub u64);

impl NextWord {
    /// Chain ends here — also the fresh-appended state.
    pub const NONE: NextWord = NextWord(0);

    /// A continuation word. `HAS_NEXT` is forced on: the flag *is* the
    /// meaning of the word, so call sites cannot forget it.
    pub fn chain(page_no: PageNo, slot: u16, flags: u16) -> Self {
        NextWord(
            ((page_no as u64) << 16)
                | (slot as u64)
                | ((flags as u64 | NEXT_HAS_NEXT as u64) << 48),
        )
    }

    /// Decodes any raw word (parse is structural; see [`VersionWord`]).
    pub const fn from_raw(word: u64) -> Self {
        NextWord(word)
    }

    pub const fn is_none(&self) -> bool {
        self.0 == 0
    }

    pub const fn has_next(&self) -> bool {
        // The flag bits live in the top u16 — shift down before masking.
        ((self.0 >> 48) & (NEXT_HAS_NEXT as u64)) != 0
    }

    /// The chained tuple's page. Panics on a chain-ending word — call sites
    /// branch on [`has_next`] first (misuse is a bug, not a runtime path).
    pub fn page_no(&self) -> PageNo {
        assert!(self.has_next(), "page_no() on a chain-ending word");
        (self.0 >> 16) as u32
    }

    /// The chained tuple's slot; see [`page_no`] for the panic contract.
    pub fn slot(&self) -> u16 {
        assert!(self.has_next(), "slot() on a chain-ending word");
        self.0 as u16
    }

    /// The raw flag bits (may include reserved bits; the visibility layer
    /// reads the named flags via the constants).
    pub const fn flags(&self) -> u16 {
        (self.0 >> 48) as u16
    }
}

/// Compile-time format locks: the header fits, and the page size is the
/// workspace's 8 KiB (a change here is a new format version, not an edit).
const _: () = assert!(HEADER_LEN == 32);
const _: () = assert!(PAGE_SIZE == 8192);
const _: () = assert!(CELL_FIXED == 26);

/// Hard slot-array bound for validation: the most u16 slots that could fit
/// between [`SLOTS_BASE`] and the page tail.
const MAX_SLOTS: usize = (PAGE_SIZE - SLOTS_BASE) / 2;

/// Everything [`parse`] can refuse. `Malformed` carries the reason — a
/// corrupt page image must never reach an access method as data.
#[derive(Debug, Error)]
pub enum PageError {
    #[error("page is not initialized (all-zero header: a never-written page)")]
    Uninitialized,
    #[error("unsupported page format version {0} (this build reads {FORMAT_VERSION})")]
    WrongVersion(u16),
    #[error("unsupported page kind {0} for this layout (heap is {PAGE_KIND_HEAP})")]
    WrongKind(u16),
    #[error("cell would exceed the u16 length field")]
    CellTooLarge,
    #[error("page full: no room for another cell between the slot array and the cell region")]
    Full,
    #[error("malformed page image: {0}")]
    Malformed(&'static str),
}

/// A page is initialized iff its version bytes are nonzero — the cheap test
/// callers use before paying [`parse`] (a never-written page reads back as
/// zeroes and is treated as empty by the access method, not as corruption).
pub fn is_initialized(buf: &[u8; PAGE_SIZE]) -> bool {
    buf[0] != 0 || buf[1] != 0
}

fn rd_u16(buf: &[u8; PAGE_SIZE], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn rd_u64(buf: &[u8; PAGE_SIZE], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

/// One decoded cell: the MVCC row header (typed words — WP1 F2) plus the
/// opaque payload. The `payload` borrows the page image; nothing points
/// into the page from outside it (invariant 4 — a row is addressed by
/// `(PageId, SlotId)`).
#[derive(Debug)]
pub struct CellRef<'a> {
    /// The §12 creator word: uncommitted-tagged `TxnId` until the commit
    /// stamps the `commit_ts`.
    pub xmin: VersionWord,
    /// The §12 deleter word — [`VersionWord::EMPTY`] while the tuple is live.
    pub xmax: VersionWord,
    /// The §12 next-tuple pointer (chain + flags); [`NextWord::NONE`] ends it.
    pub next: NextWord,
    pub payload: &'a [u8],
}

/// Encodes one standalone cell — `[len u16][xmin u64][xmax u64][next u64]
/// [payload]` — byte-identical to what [`PageMut::append_cell`] lays down
/// inside a page. This is the function the HEAP_INSERT redo record's
/// "cell bytes verbatim as cascade-page lays them down" contract (WP1 F3)
/// points at: WAL producers stamp cells through it, never by hand, so the
/// log carries exactly the page format. Fresh cells are live and chain-end
/// (`xmax`/`next` zero), same as [`PageMut::append_cell`]. Fails with
/// [`PageError::CellTooLarge`] past the u16 cell-length ceiling.
pub fn encode_cell(xmin: VersionWord, payload: &[u8]) -> Result<Vec<u8>, PageError> {
    let cell_len = CELL_FIXED + payload.len();
    if cell_len > u16::MAX as usize {
        return Err(PageError::CellTooLarge);
    }
    let mut b = Vec::with_capacity(cell_len);
    b.extend_from_slice(&(cell_len as u16).to_le_bytes());
    b.extend_from_slice(&xmin.0.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes()); // xmax: live
    b.extend_from_slice(&0u64.to_le_bytes()); // next: chain ends
    b.extend_from_slice(payload);
    Ok(b)
}

/// Parses a standalone cell (the [`encode_cell`] shape) into a [`CellRef`]
/// borrowing `bytes`. Structural validation only — the same rules
/// [`PageView::parse`] applies to cells: fixed-part minimum and length
/// agreement. Version words stay opaque (invariant 3 — semantics are the
/// visibility layer's).
pub fn parse_cell(bytes: &[u8]) -> Result<CellRef<'_>, PageError> {
    if bytes.len() < CELL_FIXED {
        return Err(PageError::Malformed("cell shorter than its fixed part"));
    }
    let len = u16::from_le_bytes(bytes[..2].try_into().expect("2 bytes")) as usize;
    if len != bytes.len() {
        return Err(PageError::Malformed(
            "cell length prefix disagrees with its byte count",
        ));
    }
    Ok(CellRef {
        xmin: VersionWord::from_raw(u64::from_le_bytes(
            bytes[2..10].try_into().expect("8 bytes"),
        )),
        xmax: VersionWord::from_raw(u64::from_le_bytes(
            bytes[10..18].try_into().expect("8 bytes"),
        )),
        next: NextWord::from_raw(u64::from_le_bytes(
            bytes[18..26].try_into().expect("8 bytes"),
        )),
        payload: &bytes[CELL_FIXED..],
    })
}

/// A validated, read-only page image.
#[derive(Debug)]
pub struct PageView<'a> {
    buf: &'a [u8; PAGE_SIZE],
}

impl<'a> PageView<'a> {
    /// Validates the whole image — header bounds AND every cell — so a
    /// corrupt page is refused once, at the boundary, never mid-scan.
    /// `Uninitialized` is a normal outcome for a never-written page; the
    /// access methods check [`is_initialized`] first when an empty page is
    /// expected and skip straight to `parse` when it is not.
    pub fn parse(buf: &'a [u8; PAGE_SIZE]) -> Result<PageView<'a>, PageError> {
        if !is_initialized(buf) {
            return Err(PageError::Uninitialized);
        }
        let version = rd_u16(buf, 0);
        if version != FORMAT_VERSION {
            return Err(PageError::WrongVersion(version));
        }
        let kind = rd_u16(buf, 2);
        if kind != PAGE_KIND_HEAP {
            return Err(PageError::WrongKind(kind));
        }
        let slot_count = rd_u16(buf, 4) as usize;
        let free_offset = rd_u16(buf, 6) as usize;
        if slot_count > MAX_SLOTS {
            return Err(PageError::Malformed("slot count overflows the slot area"));
        }
        if free_offset > PAGE_SIZE || free_offset < SLOTS_BASE + 2 * slot_count {
            return Err(PageError::Malformed(
                "free offset overlaps or precedes the slot array",
            ));
        }
        for i in 0..slot_count {
            let off = rd_u16(buf, SLOTS_BASE + 2 * i) as usize;
            if !(free_offset..PAGE_SIZE).contains(&off) {
                return Err(PageError::Malformed("slot points outside the cell region"));
            }
            let len = rd_u16(buf, off) as usize;
            if len < CELL_FIXED {
                return Err(PageError::Malformed("cell shorter than its fixed part"));
            }
            if off + len > PAGE_SIZE {
                return Err(PageError::Malformed("cell runs past the page tail"));
            }
        }
        Ok(PageView { buf })
    }

    pub fn slot_count(&self) -> usize {
        rd_u16(self.buf, 4) as usize
    }

    pub fn page_lsn(&self) -> u64 {
        rd_u64(self.buf, 8)
    }

    /// The gap between the slot array's end and the cell region's start.
    /// The next cell may consume this minus 2 bytes: the slot entry its
    /// own append adds to the array.
    pub fn free_bytes(&self) -> usize {
        rd_u16(self.buf, 6) as usize - (SLOTS_BASE + 2 * self.slot_count())
    }

    /// Cell `slot`, bounds-checked (post-[`parse`] this cannot fail, but the
    /// accessor stays total for API honesty).
    pub fn cell(&self, slot: usize) -> Result<CellRef<'a>, PageError> {
        if slot >= self.slot_count() {
            return Err(PageError::Malformed("slot index out of range"));
        }
        let off = rd_u16(self.buf, SLOTS_BASE + 2 * slot) as usize;
        let len = rd_u16(self.buf, off) as usize;
        Ok(CellRef {
            xmin: VersionWord::from_raw(rd_u64(self.buf, off + 2)),
            xmax: VersionWord::from_raw(rd_u64(self.buf, off + 10)),
            next: NextWord::from_raw(rd_u64(self.buf, off + 18)),
            payload: &self.buf[off + CELL_FIXED..off + len],
        })
    }

    /// Cells in slot order (insertion order — cells grow downward, slots up).
    pub fn iter(&self) -> impl Iterator<Item = CellRef<'a>> + '_ {
        (0..self.slot_count()).map(move |i| {
            // Parse validated every cell; the unwrap documents that
            // invariant rather than re-deriving it.
            self.cell(i).expect("parse validated every slot")
        })
    }
}

/// A writable page image. `init` produces the fresh-page header; `wrap`
/// admits an existing image through the same validation as [`PageView`].
#[derive(Debug)]
pub struct PageMut<'a> {
    buf: &'a mut [u8; PAGE_SIZE],
}

fn wr_u16(buf: &mut [u8; PAGE_SIZE], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn wr_u64(buf: &mut [u8; PAGE_SIZE], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

impl<'a> PageMut<'a> {
    /// Writes the fresh-page header: zero slots, the cell region open to the
    /// full tail, page_lsn zero. Every reserved byte is zeroed.
    pub fn init(buf: &'a mut [u8; PAGE_SIZE]) -> PageMut<'a> {
        for b in buf[..HEADER_LEN].iter_mut() {
            *b = 0;
        }
        wr_u16(buf, 0, FORMAT_VERSION);
        wr_u16(buf, 2, PAGE_KIND_HEAP);
        wr_u16(buf, 4, 0); // slot_count
        wr_u16(buf, 6, PAGE_SIZE as u16); // free_offset: no cells yet
        // page_lsn (offset 8) and the reserved tail of the header stay zero.
        PageMut { buf }
    }

    /// Admits an existing image for mutation after full validation.
    pub fn wrap(buf: &'a mut [u8; PAGE_SIZE]) -> Result<PageMut<'a>, PageError> {
        // Validate through the read path (one implementation of the rules).
        let snapshot = &*buf;
        PageView::parse(snapshot)?;
        Ok(PageMut { buf })
    }

    fn slot_count(&self) -> usize {
        rd_u16(self.buf, 4) as usize
    }

    fn free_offset(&self) -> usize {
        rd_u16(self.buf, 6) as usize
    }

    /// Appends one cell (insertion order = slot order) and returns its slot
    /// index. Cells grow downward from the page tail; the slot array grows
    /// up from [`SLOTS_BASE`]; the append fails with [`PageError::Full`] the
    /// moment the two regions would touch, counting the entry this append
    /// adds to the slot array: a cell may not start inside its own 2-byte
    /// entry, so a fresh page holds at most `PAGE_SIZE - HEADER_LEN - 2`
    /// bytes of cell. `xmin` is the §12 creator word
    /// (tagged with the writer's `TxnId`); `xmax`/`next` are written zero —
    /// live, chain ends. `page_lsn` is NOT touched here — the access method
    /// stamps it via [`PageMut::set_page_lsn`] as part of the logged write.
    pub fn append_cell(&mut self, xmin: VersionWord, payload: &[u8]) -> Result<u16, PageError> {
        let cell_len = CELL_FIXED + payload.len();
        if cell_len > u16::MAX as usize {
            return Err(PageError::CellTooLarge);
        }
        let slot = self.slot_count();
        let slot_array_end = SLOTS_BASE + 2 * slot;
        // `+ 2` is the slot entry this append is about to write at `slot_array_end`:
        // without it the new cell can start inside its own entry, and writing
        // the entry then overwrites the cell's length prefix.
        let new_free = match self.free_offset().checked_sub(cell_len) {
            Some(f) if f >= slot_array_end + 2 => f,
            _ => return Err(PageError::Full),
        };
        wr_u16(self.buf, new_free, cell_len as u16);
        wr_u64(self.buf, new_free + 2, xmin.0);
        wr_u64(self.buf, new_free + 10, 0); // xmax: live
        wr_u64(self.buf, new_free + 18, 0); // next: chain ends
        self.buf[new_free + CELL_FIXED..new_free + cell_len].copy_from_slice(payload);
        wr_u16(self.buf, slot_array_end, new_free as u16);
        wr_u16(self.buf, 4, (slot + 1) as u16);
        wr_u16(self.buf, 6, new_free as u16);
        Ok(slot as u16)
    }

    /// Stamps the deleter word on an existing cell (the format-level §12
    /// delete/update step; the engine semantics that decide *when* —
    /// conflict checks, WAL records — are the heap engine's). Panics on an
    /// out-of-range slot.
    pub fn set_xmax(&mut self, slot: u16, word: VersionWord) {
        let off = self.cell_offset(slot);
        wr_u64(self.buf, off + 10, word.0);
    }

    /// Sets the chain word on an existing cell (the §12 update step: the old
    /// tuple points at the newer version). Panics on an out-of-range slot.
    pub fn set_next(&mut self, slot: u16, word: NextWord) {
        let off = self.cell_offset(slot);
        wr_u64(self.buf, off + 18, word.0);
    }

    fn cell_offset(&self, slot: u16) -> usize {
        assert!(
            (slot as usize) < self.slot_count(),
            "slot out of range"
        );
        rd_u16(self.buf, SLOTS_BASE + 2 * slot as usize) as usize
    }

    pub fn page_lsn(&self) -> u64 {
        rd_u64(self.buf, 8)
    }

    pub fn set_page_lsn(&mut self, lsn: u64) {
        wr_u64(self.buf, 8, lsn);
    }

    pub fn free_bytes(&self) -> usize {
        self.free_offset() - (SLOTS_BASE + 2 * self.slot_count())
    }

    /// The read-only twin over the same image.
    pub fn view(&self) -> Result<PageView<'_>, PageError> {
        PageView::parse(self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_types::{Hlc, TxnId};

    /// The whole layout, pinned byte-for-byte at the only version that will
    /// ever read these images: all 32 header bytes, the slot array, and both
    /// cells in full — plus the untouched regions staying zero.
    #[test]
    fn layout_golden_literals() {
        let mut img = [0u8; PAGE_SIZE];
        {
            let mut p = PageMut::init(&mut img);
            assert_eq!(
                p.append_cell(VersionWord::uncommitted(TxnId(7)), b"ab").unwrap(),
                0
            );
            assert_eq!(
                p.append_cell(VersionWord::uncommitted(TxnId(9)), b"cd").unwrap(),
                1
            );
            p.set_page_lsn(0x0102_0304_0506_0708);
        }

        // Cell 1 (first appended): len 28 = 26 + 2, at 8192-28 = 8164
        // (0x1FFC). Cell 2: same size, at 8136 (0x1FC8).
        assert_eq!(8164, PAGE_SIZE - 28);
        assert_eq!(8136, PAGE_SIZE - 56);
        // Header: version 1, kind heap, slot_count 2, free_offset 8136,
        // page_lsn as set, reserved zero.
        assert_eq!(
            &img[..HEADER_LEN],
            &[
                0x01, 0x00, // version 1
                0x01, 0x00, // kind heap
                0x02, 0x00, // slot_count 2
                0xC8, 0x1F, // free_offset 8136
                0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // page_lsn LE
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
            ][..]
        );
        // Slot array: slot 0 → 8164 = 0x1FE4, slot 1 → 8136 = 0x1FC8, LE.
        assert_eq!(&img[SLOTS_BASE..SLOTS_BASE + 4], &[0xE4, 0x1F, 0xC8, 0x1F]);
        // Cell 0 whole: [len 28][xmin uncommitted(7)][xmax 0][next 0]["ab"].
        // The typed words changed exactly one golden byte: the creator word
        // now carries the uncommitted tag in its MSB (WP1 F1); xmax/next
        // stay byte-identical to the v1 reserved zeros when fresh.
        assert_eq!(
            &img[8164..8192],
            &[
                0x1C, 0x00, // len 28
                0x07, 0, 0, 0, 0, 0, 0, 0x80, // xmin: uncommitted(7), tag MSB
                0, 0, 0, 0, 0, 0, 0, 0, // xmax: live (EMPTY)
                0, 0, 0, 0, 0, 0, 0, 0, // next: chain ends
                b'a', b'b',
            ][..]
        );
        // Cell 1 whole: [len 28][xmin uncommitted(9)][xmax 0][next 0]["cd"].
        assert_eq!(
            &img[8136..8164],
            &[
                0x1C, 0x00, //
                0x09, 0, 0, 0, 0, 0, 0, 0x80, // xmin: uncommitted(9)
                0, 0, 0, 0, 0, 0, 0, 0, //
                0, 0, 0, 0, 0, 0, 0, 0, //
                b'c', b'd',
            ][..]
        );
        // The gap between slot array end (36) and cell 1 (8136) is untouched.
        assert!(img[36..8136].iter().all(|&b| b == 0));
    }

    /// Init → append → parse → read back, and the accessors agree with the
    /// writer's accounting.
    #[test]
    fn roundtrip_readback() {
        let mut img = [0u8; PAGE_SIZE];
        let free_after_append;
        let lsn = 0xA5A5_A5A5;
        {
            let mut p = PageMut::init(&mut img);
            let slots: Vec<u16> = (0..100u64)
                .map(|i| {
                    p.append_cell(
                        VersionWord::uncommitted(TxnId(1_000 + i)),
                        &vec![b'x'; (i % 40) as usize],
                    )
                    .unwrap()
                })
                .collect();
            assert_eq!(slots.len(), 100);
            free_after_append = p.free_bytes();
            p.set_page_lsn(lsn);
        } // the mutable borrow ends here; the image is read back below

        let v = PageView::parse(&img).unwrap();
        assert_eq!(v.slot_count(), 100);
        assert_eq!(v.page_lsn(), lsn);
        assert_eq!(v.free_bytes(), free_after_append);
        for (i, cell) in v.iter().enumerate() {
            assert_eq!(cell.xmin, VersionWord::uncommitted(TxnId(1_000 + i as u64)));
            assert_eq!(cell.xmax, VersionWord::EMPTY, "fresh cell is live");
            assert_eq!(cell.next, NextWord::NONE, "fresh cell ends its chain");
            assert_eq!(cell.payload, vec![b'x'; i % 40]);
        }
    }

    /// The WP1 F2 mutation steps: stamp a deleter word and a chain word on
    /// an existing cell, then read them back through a fresh parse.
    #[test]
    fn deleter_and_chain_stamp_read_back() {
        let mut img = [0u8; PAGE_SIZE];
        let commit_ts = Hlc(0x0123_4567_89AB_CDEF);
        {
            let mut p = PageMut::init(&mut img);
            let s = p
                .append_cell(VersionWord::uncommitted(TxnId(5)), b"row")
                .unwrap();
            p.set_xmax(s, VersionWord::committed(commit_ts));
            p.set_next(
                s,
                NextWord::chain(0x0102_0304, 0x0506, NEXT_HEAP_ONLY | NEXT_HAS_TOAST),
            );
        }
        let cell = PageView::parse(&img).unwrap().cell(0).unwrap();
        assert_eq!(cell.xmax, VersionWord::committed(commit_ts));
        assert!(cell.next.has_next());
        assert_eq!(cell.next.page_no(), 0x0102_0304);
        assert_eq!(cell.next.slot(), 0x0506);
        assert_eq!(
            cell.next.flags(),
            NEXT_HAS_NEXT | NEXT_HEAP_ONLY | NEXT_HAS_TOAST,
            "chain() forces HAS_NEXT on top of the caller's flags"
        );
    }

    /// The `next` word's bit layout is format law (WP1 F2): pin the exact
    /// u64, the flag forcing, and the NONE/chain boundary behavior.
    #[test]
    fn next_word_bit_layout_is_pinned() {
        let w = NextWord::chain(0x0102_0304, 0x0506, NEXT_HEAP_ONLY);
        assert_eq!(
            w.0,
            0x0003_0102_0304_0506,
            "slot|page_no<<16|flags<<48 with HAS_NEXT|HEAP_ONLY = 0x0003 in the top u16"
        );
        assert_eq!(w.page_no(), 0x0102_0304);
        assert_eq!(w.slot(), 0x0506);

        // chain() cannot produce a word that reads as chain-ending, whatever
        // flags the caller passes.
        assert!(NextWord::chain(7, 9, 0).has_next());
        assert!(NextWord::chain(7, 9, u16::MAX).has_next());

        // NONE and from_raw(0) are the same chain-ending word; its accessors
        // refuse to synthesize a position.
        assert_eq!(NextWord::NONE, NextWord::from_raw(0));
        assert!(NextWord::NONE.is_none());
        assert!(!NextWord::NONE.has_next());
    }

    #[test]
    #[should_panic(expected = "slot out of range")]
    fn set_xmax_refuses_out_of_range_slot() {
        let mut img = [0u8; PAGE_SIZE];
        let mut p = PageMut::init(&mut img);
        p.set_xmax(0, VersionWord::EMPTY);
    }

    /// Zero-payload fills until the two growth regions touch, at an exact
    /// count: cells of 26 bytes down from 8192, slots of 2 bytes up from 32
    /// admit 291 cells, leaving free_bytes == 12 — and the 292nd append is
    /// refused, not truncated.
    #[test]
    fn full_boundary_is_exact() {
        let mut img = [0u8; PAGE_SIZE];
        let mut p = PageMut::init(&mut img);
        let mut n = 0;
        while p
            .append_cell(VersionWord::uncommitted(TxnId(1)), &[])
            .is_ok()
        {
            n += 1;
        }
        assert_eq!(n, 291);
        assert_eq!(p.free_bytes(), 12);
        assert!(matches!(
            p.append_cell(VersionWord::uncommitted(TxnId(1)), &[]),
            Err(PageError::Full)
        ));
        // The refused append left the page consistent.
        assert_eq!(p.view().unwrap().slot_count(), 291);
    }

    /// The entry an append adds to the slot array is part of the append: a
    /// cell may not start inside its own 2-byte entry. On a fresh page that
    /// caps a cell at `PAGE_SIZE - HEADER_LEN - 2` bytes; the next two sizes
    /// were accepted before the rule counted the entry, and the image they
    /// left behind did not parse.
    #[test]
    fn fresh_page_boundary_counts_the_new_slot_entry() {
        let max_payload = PAGE_SIZE - HEADER_LEN - 2 - CELL_FIXED; // 8132
        let mut img = [0u8; PAGE_SIZE];
        {
            let mut p = PageMut::init(&mut img);
            assert_eq!(
                p.append_cell(VersionWord::uncommitted(TxnId(1)), &vec![b'x'; max_payload])
                    .unwrap(),
                0
            );
            assert_eq!(p.free_bytes(), 0, "the cell and its entry take the whole tail");
        }
        let v = PageView::parse(&img).expect("an exactly full page is a legal page");
        assert_eq!((v.slot_count(), v.free_bytes()), (1, 0));

        for payload_len in [max_payload + 1, max_payload + 2] {
            let mut img = [0u8; PAGE_SIZE];
            PageMut::init(&mut img);
            let before = img;
            let outcome = PageMut::wrap(&mut img)
                .expect("a fresh page wraps")
                .append_cell(VersionWord::uncommitted(TxnId(1)), &vec![b'x'; payload_len]);
            assert!(
                matches!(outcome, Err(PageError::Full)),
                "payload {payload_len} must be refused, got {outcome:?}"
            );
            assert!(img == before, "a refused append leaves the page untouched");
        }
    }

    /// Every page state the zero-payload fill reaches (0 through 291 cells),
    /// crossed with the cell lengths around that state's boundary. The law,
    /// checked both ways: an append is accepted exactly when the cell plus
    /// its own slot entry fit in `free_bytes`; an accepted append leaves a
    /// page that parses, and a refused one leaves the image untouched.
    #[test]
    fn append_boundary_holds_in_every_page_state() {
        let mut states = Vec::new();
        let mut img = [0u8; PAGE_SIZE];
        PageMut::init(&mut img);
        loop {
            let free = PageView::parse(&img).unwrap().free_bytes();
            states.push((img, free));
            let filled = PageMut::wrap(&mut img)
                .unwrap()
                .append_cell(VersionWord::uncommitted(TxnId(1)), &[])
                .is_ok();
            if !filled {
                break;
            }
        }
        assert_eq!(states.len(), 292, "0..=291 cells, then the page is full");

        for (img, free) in states {
            // Cell lengths from 4 bytes below the gap to the gap itself; the
            // two largest are the ones the old rule accepted.
            for cell_len in (free.saturating_sub(4)..=free).map(|len| len.max(CELL_FIXED)) {
                let payload_len = cell_len - CELL_FIXED;
                let mut img = img;
                let before = img;
                let outcome = PageMut::wrap(&mut img)
                    .unwrap()
                    .append_cell(VersionWord::uncommitted(TxnId(2)), &vec![b'x'; payload_len]);
                let accepted = outcome.is_ok();
                assert_eq!(
                    accepted,
                    cell_len + 2 <= free,
                    "free {free}, cell {cell_len}, outcome {outcome:?}"
                );
                if accepted {
                    let v = PageView::parse(&img)
                        .expect("an accepted append leaves a page that parses");
                    assert_eq!(v.free_bytes(), free - cell_len - 2);
                } else {
                    assert!(img == before, "a refused append leaves the page untouched");
                }
            }
        }
    }

    /// A payload that cannot fit the u16 length field is refused up front.
    #[test]
    fn oversized_cell_refused() {
        let mut img = [0u8; PAGE_SIZE];
        let mut p = PageMut::init(&mut img);
        let huge = vec![0u8; u16::MAX as usize]; // > u16::MAX - 26
        assert!(matches!(
            p.append_cell(VersionWord::uncommitted(TxnId(1)), &huge),
            Err(PageError::CellTooLarge)
        ));
    }

    /// Every corruption class the parser promises to catch, one test each on
    /// a hand-mangled image.
    #[test]
    fn malformed_images_are_refused() {
        let mut img = [0u8; PAGE_SIZE];
        {
            let mut p = PageMut::init(&mut img);
            p.append_cell(VersionWord::uncommitted(TxnId(5)), b"row")
                .unwrap();
        }

        // Uninitialized: a zeroed page.
        assert!(matches!(
            PageView::parse(&[0u8; PAGE_SIZE]),
            Err(PageError::Uninitialized)
        ));

        // Wrong version.
        let mut bad = img;
        bad[0] = 2;
        assert!(matches!(PageView::parse(&bad), Err(PageError::WrongVersion(2))));

        // Wrong kind.
        let mut bad = img;
        bad[2] = 7;
        assert!(matches!(PageView::parse(&bad), Err(PageError::WrongKind(7))));

        // Slot count overflows the slot area.
        let mut bad = img;
        bad[4] = 0x88; // 0x8811 > MAX_SLOTS (4080) — hand-picked LE pair
        bad[5] = 0x11;
        assert!(matches!(PageView::parse(&bad), Err(PageError::Malformed(_))));

        // Free offset pulled below the slot array's end (overlap).
        let mut bad = img;
        bad[6] = 0;
        bad[7] = 0;
        assert!(matches!(PageView::parse(&bad), Err(PageError::Malformed(_))));

        // Slot pointer moved out of the cell region (into the header area).
        let mut bad = img;
        bad[SLOTS_BASE] = 0;
        bad[SLOTS_BASE + 1] = 0; // slot 0 → offset 0
        assert!(matches!(PageView::parse(&bad), Err(PageError::Malformed(_))));

        // Cell length shrunk below the fixed part.
        let mut bad = img;
        bad[PAGE_SIZE - 29] = 3; // len 3 < 26
        bad[PAGE_SIZE - 28] = 0;
        assert!(matches!(PageView::parse(&bad), Err(PageError::Malformed(_))));

        // Cell length extended past the page tail.
        let mut bad = img;
        bad[PAGE_SIZE - 29] = 0xFF; // len 0x0FFF… past the tail
        bad[PAGE_SIZE - 28] = 0x0F;
        assert!(matches!(PageView::parse(&bad), Err(PageError::Malformed(_))));
    }

    /// `wrap` admits a good image and refuses a bad one — the mutation path
    /// never bypasses the read path's rules.
    #[test]
    fn wrap_validates_like_parse() {
        let mut good = [0u8; PAGE_SIZE];
        PageMut::init(&mut good);
        assert!(PageMut::wrap(&mut good).is_ok());

        let mut bad = [0u8; PAGE_SIZE];
        bad[0] = 1; // a real version, so we get past the initialization check
        bad[2] = 9; // wrong kind
        assert!(matches!(PageMut::wrap(&mut bad), Err(PageError::WrongKind(9))));
    }

    /// The HEAP_INSERT contract's "cell bytes verbatim as cascade-page lays
    /// them down" (WP1 F3): the standalone encoder must produce exactly what
    /// `append_cell` writes into a page — one code path, or the log and the
    /// page drift. `parse_cell` inverts it, words and payload intact.
    #[test]
    fn standalone_cell_is_byte_identical_to_the_in_page_cell() {
        let payload = b"the row image";
        let cell = encode_cell(VersionWord::uncommitted(TxnId(42)), payload).unwrap();

        let mut img = [0u8; PAGE_SIZE];
        PageMut::init(&mut img)
            .append_cell(VersionWord::uncommitted(TxnId(42)), payload)
            .unwrap();
        let off =
            u16::from_le_bytes(img[SLOTS_BASE..SLOTS_BASE + 2].try_into().unwrap()) as usize;
        let in_page = &img[off..off + cell.len()];
        assert_eq!(cell, in_page, "verbatim means verbatim");

        let r = parse_cell(&cell).unwrap();
        assert_eq!(r.xmin, VersionWord::uncommitted(TxnId(42)));
        assert_eq!(r.xmax, VersionWord::EMPTY);
        assert_eq!(r.next, NextWord::NONE);
        assert_eq!(r.payload, payload);
    }

    /// Structural refusals: too-short, length-prefix disagreement, and the
    /// u16 ceiling — parse and encode refuse before any caller can mistake
    /// garbage for a cell.
    #[test]
    fn standalone_cell_refuses_structurally() {
        assert!(matches!(parse_cell(&[0u8; 10]), Err(PageError::Malformed(_))));
        let mut bad = encode_cell(VersionWord::EMPTY, b"ab").unwrap();
        bad[..2].copy_from_slice(&99u16.to_le_bytes());
        assert!(matches!(parse_cell(&bad), Err(PageError::Malformed(_))));
        let big = vec![0u8; u16::MAX as usize - CELL_FIXED + 1];
        assert!(matches!(
            encode_cell(VersionWord::EMPTY, &big),
            Err(PageError::CellTooLarge)
        ));
    }
}
