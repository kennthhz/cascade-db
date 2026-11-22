//! T1.5: the F2 cell contract as an executable spec (WP1 doc + the crate's
//! doc comments are the contract; these tests pin it). The laws: the
//! standalone codec (`encode_cell`/`parse_cell`) matches what `append_cell`
//! lays down, the u16 ceiling is exact, parse inverts encode on the whole
//! sampled domain, parse is structural-only (version words stay opaque),
//! fresh cells are live and chain-end, and the mutators stamp and read
//! back through the page.

use cascade_page::{
    encode_cell, is_initialized, parse_cell, NextWord, PageError, PageMut, PageView, CELL_FIXED,
    NEXT_HAS_NEXT, NEXT_HAS_TOAST, PAGE_SIZE,
};
use cascade_types::{Hlc, VersionWord as VW, Xid};

/// Deterministic xorshift64\* — reproducible samples, no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// A sampled payload of `len` pseudo-random bytes.
fn payload(seed: u64, len: usize) -> Vec<u8> {
    let mut r = Rng::new(seed);
    (0..len).map(|_| r.below(256) as u8).collect()
}

/// The layout law, pinned byte for byte: `[len u16 LE][xmin u64 LE]
/// [xmax u64 LE = 0][next u64 LE = 0][payload]` — length counts the whole
/// cell, xmax/next zero for a fresh cell.
#[test]
fn encode_cell_layout_is_pinned() {
    let b = encode_cell(VW::uncommitted(Xid(7)), b"xy").expect("fits");
    assert_eq!(b.len(), CELL_FIXED + 2);
    let mut want = Vec::new();
    want.extend_from_slice(&((CELL_FIXED + 2) as u16).to_le_bytes());
    want.extend_from_slice(&(Xid(7).0 | (1 << 63)).to_le_bytes());
    want.extend_from_slice(&0u64.to_le_bytes());
    want.extend_from_slice(&0u64.to_le_bytes());
    want.extend_from_slice(b"xy");
    assert_eq!(b, want, "the cell layout is a one-way door");
}

/// The ceiling law: a cell must fit `u16::MAX` bytes total, so the largest
/// legal payload is exactly `u16::MAX - CELL_FIXED` — one byte more
/// refuses, by name.
#[test]
fn the_u16_ceiling_is_exact() {
    let max = u16::MAX as usize - CELL_FIXED;
    assert!(
        encode_cell(VW::EMPTY, &payload(0xB01, max)).is_ok(),
        "{max}-byte payload is exactly at the ceiling"
    );
    assert_eq!(
        encode_cell(VW::EMPTY, &payload(0xB02, max + 1)),
        Err(PageError::CellTooLarge),
        "one byte past the ceiling refuses"
    );
}

/// The inverse law: parse inverts encode across the whole sampled domain —
/// every payload size class (empty, sub-fixed, at/around CELL_FIXED, large,
/// at-ceiling) and both version-word classes, words and payload exact.
#[test]
fn parse_inverts_encode_everywhere_sampled() {
    let mut r = Rng::new(0xB03);
    let sizes = [0usize, 1, 25, 26, 27, 255, 4096, u16::MAX as usize - CELL_FIXED];
    let xmins = [
        VW::uncommitted(Xid(1)),
        VW::uncommitted(Xid((1 << 63) - 1)),
        VW::committed(Hlc(0x1234_5678_9ABC)),
    ];
    for (i, &len) in sizes.iter().enumerate() {
        let p = payload(0xB04 + i as u64, len);
        for xmin in xmins {
            let cell = encode_cell(xmin, &p).expect("fits");
            assert_eq!(cell.len(), CELL_FIXED + len);
            let c = parse_cell(&cell).expect("parses");
            assert_eq!(c.xmin, xmin);
            assert_eq!(c.xmax, VW::EMPTY, "fresh cells are live");
            assert_eq!(c.next, NextWord::NONE, "fresh cells end their chain");
            assert_eq!(c.payload, &p[..], "payload verbatim at len {len}");
        }
        let _ = r.next_u64(); // keep the helper exercised for reproducibility
    }
}

/// The structural-refusal matrix: shorter than the fixed part, a length
/// prefix that disagrees with the byte count in either direction.
#[test]
fn parse_refuses_malformed_cells() {
    // 25 bytes — one below the fixed part.
    assert!(matches!(
        parse_cell(&payload(0xB05, CELL_FIXED - 1)),
        Err(PageError::Malformed(_))
    ));
    // Declared length short of the actual bytes.
    let mut b = encode_cell(VW::EMPTY, b"abcd").expect("fits");
    b.extend_from_slice(&[0xEE]); // trailing byte the prefix doesn't know
    assert!(matches!(parse_cell(&b), Err(PageError::Malformed(_))));
    // Declared length past the actual bytes: trim the tail.
    let mut b = encode_cell(VW::EMPTY, b"abcd").expect("fits");
    b.pop();
    assert!(matches!(parse_cell(&b), Err(PageError::Malformed(_))));
    // Empty input.
    assert!(matches!(parse_cell(&[]), Err(PageError::Malformed(_))));
}

/// The opacity law (invariant 3): parse never interprets version words —
/// any raw u64 decodes through, semantics are the visibility layer's.
#[test]
fn version_words_stay_opaque_through_parse() {
    let mut cell = encode_cell(VW::EMPTY, b"opaque").expect("fits");
    // Overwrite xmax (bytes 10..18) and next (18..26) with garbage words.
    cell[10..18].copy_from_slice(&0xDEAD_BEEF_1234_5678u64.to_le_bytes());
    cell[18..26].copy_from_slice(&0xFEED_FACE_8765_4321u64.to_le_bytes());
    let c = parse_cell(&cell).expect("garbage words still parse — structural only");
    assert_eq!(c.xmax, VW::from_raw(0xDEAD_BEEF_1234_5678));
    assert_eq!(c.next, NextWord::from_raw(0xFEED_FACE_8765_4321));
    assert_eq!(c.payload, b"opaque");
}

/// The byte-identity seam (WP1 F3's "verbatim"): a cell encoded standalone
/// parses to exactly what `append_cell` lays into a real page for the same
/// inputs — the producer-side and reader-side views of one format.
#[test]
fn standalone_cells_match_the_in_page_cell() {
    let mut buf = [0u8; PAGE_SIZE];
    let mut page = PageMut::init(&mut buf);
    let p = payload(0xB06, 64);
    let slot = page.append_cell(VW::uncommitted(Xid(9)), &p).expect("fits");
    let view: PageView<'_> = page.view().expect("initialized page views");
    let in_page = view.cell(slot as usize).expect("the appended cell");
    let standalone = parse_cell(&encode_cell(VW::uncommitted(Xid(9)), &p).expect("fits"))
        .expect("parses");
    assert_eq!(in_page.xmin, standalone.xmin);
    assert_eq!(in_page.xmax, standalone.xmax);
    assert_eq!(in_page.next, standalone.next);
    assert_eq!(in_page.payload, standalone.payload);
}

/// The §12 delete/update mechanics at the format level: `set_xmax` and
/// `set_next` stamp through the page and read back; `NONE` restores
/// chain-end; a clear `HAS_NEXT` makes the rest of the word ignorable.
#[test]
fn mutators_stamp_through_the_page_and_read_back() {
    let mut buf = [0u8; PAGE_SIZE];
    let mut page = PageMut::init(&mut buf);
    let slot = page.append_cell(VW::uncommitted(Xid(3)), b"row").expect("fits");

    page.set_xmax(slot, VW::committed(Hlc(0x4242)));
    page.set_next(slot, NextWord::chain(5, 1, NEXT_HAS_TOAST));
    let view = page.view().expect("views");
    let c = view.cell(slot as usize).expect("cell");
    assert_eq!(c.xmax, VW::committed(Hlc(0x4242)));
    assert_eq!(c.next, NextWord::chain(5, 1, NEXT_HAS_TOAST));
    assert_eq!(c.next.page_no(), 5);
    assert_eq!(c.next.slot(), 1);
    assert_eq!(c.next.flags(), NEXT_HAS_TOAST);
    assert!(c.next.has_next());

    // Back to chain-end: NONE is the encoded zero, and without HAS_NEXT
    // set the remaining bits are ignored by construction.
    page.set_next(slot, NextWord::NONE);
    let view = page.view().expect("views");
    assert_eq!(view.cell(slot as usize).expect("cell").next, NextWord::NONE);
    assert!(!NextWord::from_raw(0xABCD).has_next(), "no HAS_NEXT bit");
    assert_eq!(NextWord::from_raw(NEXT_HAS_NEXT as u64).has_next(), true);

    // The deleter word survives independent of the chain word.
    let view = page.view().expect("views");
    assert_eq!(
        view.cell(slot as usize).expect("cell").xmax,
        VW::committed(Hlc(0x4242))
    );
}

/// Stamping a nonexistent slot is a programmer error, refused loudly.
#[test]
#[should_panic]
fn set_xmax_refuses_an_out_of_range_slot() {
    let mut buf = [0u8; PAGE_SIZE];
    let mut page = PageMut::init(&mut buf);
    page.set_xmax(99, VW::committed(Hlc(1)));
}

/// A never-written page says so, by name.
#[test]
fn fresh_pages_are_uninitialized() {
    let buf = [0u8; PAGE_SIZE];
    assert!(!is_initialized(&buf));
    assert!(matches!(
        PageView::parse(&buf),
        Err(PageError::Uninitialized)
    ));
}
