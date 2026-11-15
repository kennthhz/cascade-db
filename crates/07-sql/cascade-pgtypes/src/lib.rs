//! PostgreSQL type system.
//!
//! See `README.md` and `AI/CONTEXT.md`.
//!
//! Since T1.4 this crate also owns the **key contract** (WP1 F4, WP5 D13):
//! [`KeyBytes`]/[`KeyRange`] — the order-preserving normalized encoding
//! every index and scan range is defined on — and the logical index-entry
//! shape ([`IndexEntry`]). Per-type encodings live here; the byte layout is
//! a one-way door documented in `docs/storage-wp1-formats.md` (F4).

#![allow(dead_code)]

use cascade_types::VersionWord;

/// PG type OID — matches PostgreSQL's reserved range for built-in types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeOid(pub u32);

/// Well-known type OIDs (match upstream PG).
pub mod oid {
    use super::TypeOid;
    pub const INT2: TypeOid = TypeOid(21);
    pub const INT4: TypeOid = TypeOid(23);
    pub const INT8: TypeOid = TypeOid(20);
    pub const TEXT: TypeOid = TypeOid(25);
    pub const NUMERIC: TypeOid = TypeOid(1700);
    pub const TIMESTAMP: TypeOid = TypeOid(1114);
    pub const TIMESTAMPTZ: TypeOid = TypeOid(1184);
    pub const UUID: TypeOid = TypeOid(2950);
    pub const JSON: TypeOid = TypeOid(114);
    pub const JSONB: TypeOid = TypeOid(3802);
    pub const BYTEA: TypeOid = TypeOid(17);
    // ... etc.
}

// ---- The key contract (WP1 F4 / WP5 D13; T1.4) ----

pub use std::ops::Bound;

/// First byte of every non-null key encoding: the frozen type tag.
///
/// Tags order *types* relative to each other (arbitrary but total — a
/// single index column holds one type, so cross-type comparison never
/// decides query results; the tags exist to keep every non-null encoding
/// below the null marker and to make key bytes self-describing in dumps).
/// Like the WAL's type bytes: extend, never renumber.
pub mod tag {
    /// All integer widths, canonicalized to `i64` (one tag: width-uniform
    /// ordering — `int2`/`int4` keys interleave correctly with `int8`).
    pub const INT: u8 = 0x01;
    /// UTF-8 text, bytewise (C) collation — see [`encode_text`].
    pub const TEXT: u8 = 0x02;
    /// Boolean, `false` < `true`.
    pub const BOOL: u8 = 0x03;
    /// The NULL marker: a bare `0xFF` byte, above every non-null encoding's
    /// tag — this is what makes NULLS LAST (the global rule, WP1 F4) hold
    /// bytewise. No non-null payload may start with `0xFF`.
    pub const NULL: u8 = 0xFF;
}

/// A normalized, order-preserving key encoding (WP1 F4, WP5 D13).
///
/// Equality and ordering are defined *on these bytes* — bytewise, exactly
/// what `Vec<u8>`'s derived `Ord` does — never on the decoded value
/// (invariant 7). Non-null encodings are `[tag][payload]`; NULL is a bare
/// `[0xFF]`. Multi-column keys are the concatenation of per-column
/// encodings ([`concat`]); prefix scans are ranges over the concatenation
/// ([`KeyRange::prefix_bytes`], [`KeyRange::text_prefix`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyBytes(pub Vec<u8>);

/// All integer widths (int2/int4/int8) as one canonical order-preserving
/// encoding: the `i64` value sign-flipped (`^ i64::MIN`) then big-endian.
/// The flip maps the signed ordering onto unsigned byte ordering, so
/// bytewise compare *is* numeric compare; width canonicalization means a
/// `int4` key never disagrees with the same `int8` value.
pub fn encode_int8(v: i64) -> KeyBytes {
    let mut b = Vec::with_capacity(9);
    b.push(tag::INT);
    b.extend_from_slice((v ^ i64::MIN).to_be_bytes().as_slice());
    KeyBytes(b)
}

/// The inverse of [`encode_int8`] (`None` on a key that is not one).
pub fn decode_int8(k: &KeyBytes) -> Option<i64> {
    let b = k.0.as_slice();
    if b.len() != 9 || b[0] != tag::INT {
        return None;
    }
    let raw = i64::from_be_bytes(b[1..9].try_into().expect("8 bytes"));
    Some(raw ^ i64::MIN)
}

/// UTF-8 text under **bytewise (C) collation**: `[TEXT][utf8 bytes][0x00]`.
///
/// Order-preserving by two properties of the domain: UTF-8 byte order is
/// code-point order, and interior `0x00` bytes compare exactly as
/// themselves against payload bytes — the trailing `0x00` terminator only
/// ever breaks a tie in favor of the shorter string, which is the correct
/// lexicographic verdict (a strict prefix sorts first). The terminator
/// also makes the encoding prefix-free, which [`KeyRange::text_prefix`]
/// exploits. A non-C collation is *not* order-preserving under this
/// encoding — invariant 7 gates its participation in an index until a
/// collation-aware normalization lands here.
pub fn encode_text(v: &str) -> KeyBytes {
    let mut b = Vec::with_capacity(2 + v.len());
    b.push(tag::TEXT);
    b.extend_from_slice(v.as_bytes());
    b.push(0);
    KeyBytes(b)
}

/// The inverse of [`encode_text`] (`None` on a key that is not one).
pub fn decode_text(k: &KeyBytes) -> Option<String> {
    let b = k.0.as_slice();
    if b.len() < 2 || b[0] != tag::TEXT || *b.last().expect("len >= 2 checked") != 0 {
        return None;
    }
    String::from_utf8(b[1..b.len() - 1].to_vec()).ok()
}

/// Boolean: `[BOOL][0x00]` for `false`, `[BOOL][0x01]` for `true`.
pub fn encode_bool(v: bool) -> KeyBytes {
    KeyBytes(vec![tag::BOOL, u8::from(v)])
}

/// The inverse of [`encode_bool`] (`None` on a key that is not one).
pub fn decode_bool(k: &KeyBytes) -> Option<bool> {
    match k.0.as_slice() {
        [tag::BOOL, 0] => Some(false),
        [tag::BOOL, 1] => Some(true),
        _ => None,
    }
}

/// The NULL key — a bare `0xFF` byte. Sorts above every non-null encoding
/// (NULLS LAST, WP1 F4's global rule), bytewise, with no per-type rule.
pub fn encode_null() -> KeyBytes {
    KeyBytes(vec![tag::NULL])
}

/// True when the key *is* the NULL marker.
pub fn is_null(k: &KeyBytes) -> bool {
    k.0.as_slice() == [tag::NULL]
}

/// Multi-column keys are the concatenation of per-column encodings (WP1
/// F4): bytewise compare over the concatenation is lexicographic compare
/// over the columns, because every per-column encoding is order-preserving
/// and self-terminating (fixed-width, terminator, or the bare null marker).
pub fn concat(parts: &[KeyBytes]) -> KeyBytes {
    let mut b = Vec::with_capacity(parts.iter().map(|k| k.0.len()).sum());
    for k in parts {
        b.extend_from_slice(&k.0);
    }
    KeyBytes(b)
}

/// An open/closed range over normalized keys — the API's most-used read
/// parameter (WP5 D13). Bounds compare on normalized bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    pub lower: Bound<KeyBytes>,
    pub upper: Bound<KeyBytes>,
}

impl KeyRange {
    /// Every key.
    pub fn all() -> Self {
        KeyRange {
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
        }
    }

    /// Exactly one key (an exact-index lookup).
    pub fn point(k: KeyBytes) -> Self {
        KeyRange {
            lower: Bound::Included(k.clone()),
            upper: Bound::Included(k),
        }
    }

    /// The general multi-column prefix: every key whose first columns are
    /// `fixed`. The excluded upper — `fixed ++ [0xFF, 0xFF]` — sits above
    /// every continuation (a next column's encoding starts with a tag
    /// `0x01..0x03`, and its NULL marker `[0xFF]` is *shorter* than the
    /// bound's two `0xFF`s, so it stays inside the range: a row whose next
    /// column is NULL is in the prefix, as it must be) and below every
    /// different-prefix key.
    pub fn prefix_bytes(fixed: KeyBytes) -> Self {
        let mut upper = fixed.0.clone();
        upper.extend_from_slice(&[0xFF, 0xFF]);
        KeyRange {
            lower: Bound::Included(fixed),
            upper: Bound::Excluded(KeyBytes(upper)),
        }
    }

    /// Every text value with prefix `p` — the string-scan shape. Lower is
    /// `p`'s own encoding; the excluded upper appends `0xFF` to the payload
    /// (no terminator), which every continuation byte of a UTF-8 payload
    /// (`0x00..0xF4`, terminator included) sorts below, and no other valid
    /// text key can reach.
    pub fn text_prefix(p: &str) -> Self {
        let mut b = Vec::with_capacity(2 + p.len());
        b.push(tag::TEXT);
        b.extend_from_slice(p.as_bytes());
        b.push(0xFF);
        KeyRange {
            lower: Bound::Included(encode_text(p)),
            upper: Bound::Excluded(KeyBytes(b)),
        }
    }
}

/// First byte of a [`Locator`] inside an encoded [`IndexEntry`].
pub mod locator {
    pub const PK: u8 = 0x00;
    pub const HEAP: u8 = 0x01;
}

/// Where the indexed row lives (WP1 F4/§13): at its primary key (IOT) or
/// at a relation-local heap address — the same `[page_no u32][slot u16]`
/// packing family as F2's `next` word, without flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locator {
    Pk(KeyBytes),
    Heap { page_no: u32, slot: u16 },
}

/// The logical index entry (WP1 F4/§13): `[key bytes][locator][create word
/// u64 LE][delete word u64 LE]`, the create/delete words being F1 version
/// words stored raw. The B-tree *page* layout — slotting, splits, SMOs —
/// is cascade-btree's (WP6/WP8), deliberately unpinned here; this shape is
/// what redo and uniqueness checks consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub key: KeyBytes,
    pub locator: Locator,
    /// The version word of the txn that inserted the entry.
    pub create: VersionWord,
    /// The deleter word — [`VersionWord::EMPTY`] while the entry is live.
    pub delete: VersionWord,
}

impl IndexEntry {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(
            self.key.0.len()
                + 1
                + match self.locator {
                    Locator::Pk(_) => self.key.0.len(),
                    Locator::Heap { .. } => 6,
                }
                + 16,
        );
        b.extend_from_slice(&self.key.0);
        match &self.locator {
            Locator::Pk(k) => {
                b.push(locator::PK);
                b.extend_from_slice(&k.0);
            }
            Locator::Heap { page_no, slot } => {
                b.push(locator::HEAP);
                b.extend_from_slice(&page_no.to_le_bytes());
                b.extend_from_slice(&slot.to_le_bytes());
            }
        }
        b.extend_from_slice(&self.create.0.to_le_bytes());
        b.extend_from_slice(&self.delete.0.to_le_bytes());
        b
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    /// Numeric ordering survives the flip: sampled across the extremes and
    /// the sign boundary, byte order == value order.
    #[test]
    fn int_keys_order_like_the_values() {
        let vs = [
            i64::MIN,
            i64::MIN + 1,
            -1,
            0,
            1,
            42,
            1 << 40,
            i64::MAX - 1,
            i64::MAX,
        ];
        for w in vs.windows(2) {
            assert!(
                encode_int8(w[0]) < encode_int8(w[1]),
                "{} must sort below {}",
                w[0],
                w[1]
            );
        }
        assert_eq!(encode_int8(0).0, vec![tag::INT, 0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            encode_int8(-1).0,
            vec![tag::INT, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }

    /// Text ordering is lexicographic, shorter-prefix first, interior
    /// NULs included in the ordering (they are ordinary payload bytes).
    #[test]
    fn text_keys_order_like_the_strings() {
        let vs = ["", "a", "a\0", "a\0b", "ab", "b", "é", "🦀"];
        for w in vs.windows(2) {
            assert!(
                encode_text(w[0]) < encode_text(w[1]),
                "{:?} must sort below {:?}",
                w[0],
                w[1]
            );
        }
        assert_eq!(encode_text("ab").0, vec![tag::TEXT, b'a', b'b', 0]);
    }

    /// NULLS LAST is bytewise and global: the bare 0xFF marker sorts above
    /// every non-null key of every type.
    #[test]
    fn null_sorts_last_over_every_type() {
        let null = encode_null();
        assert!(null > encode_int8(i64::MAX));
        assert!(null > encode_text("🦀🦀🦀"));
        assert!(null > encode_bool(true));
        assert!(is_null(&null));
        assert!(!is_null(&encode_int8(0)));
    }

    /// Roundtrips: decode inverts encode for every sampled value; a key of
    /// the wrong shape decodes to None, never garbage.
    #[test]
    fn decodes_invert_the_encodings() {
        for v in [i64::MIN, -1, 0, 1, i64::MAX] {
            assert_eq!(decode_int8(&encode_int8(v)), Some(v));
        }
        for v in ["", "ab", "a\0b", "é🦀"] {
            assert_eq!(decode_text(&encode_text(v)), Some(v.to_owned()));
        }
        for v in [false, true] {
            assert_eq!(decode_bool(&encode_bool(v)), Some(v));
        }
        assert_eq!(decode_int8(&encode_text("not an int")), None);
        assert_eq!(decode_text(&encode_int8(5)), None);
        assert_eq!(decode_bool(&encode_null()), None);
    }

    /// Concatenation is lexicographic over columns: sampled pairs order by
    /// first column, ties break on the second — and the width
    /// canonicalization lets int4-shaped values ride the int8 encoding.
    #[test]
    fn concatenated_keys_order_lexicographically() {
        let k = |a: i64, b: &str| concat(&[encode_int8(a), encode_text(b)]);
        assert!(k(1, "a") < k(2, "a"), "first column dominates");
        assert!(k(1, "a") < k(1, "b"), "second column breaks ties");
        assert!(k(1, "a") < k(1, "a\0b"), "ties extend lexicographically");
        assert!(k(1, "z") < k(2, "a"), "second column never beats the first");
    }

    /// Prefix scans cover exactly the prefixed keys: every continuation is
    /// inside the range, every sampled non-prefixed key outside — including
    /// the tricky ones (longer prefixes, NUL-adjacent strings, NULLs).
    #[test]
    fn text_prefix_range_covers_exactly_the_prefix() {
        let r = KeyRange::text_prefix("ab");
        let inside = ["ab", "ab\0", "ab\0b", "abc", "abé🦀"];
        for s in inside {
            let k = encode_text(s);
            assert!(
                matches!(r.lower, Bound::Included(ref lo) if *lo <= k)
                    && matches!(r.upper, Bound::Excluded(ref hi) if k < *hi),
                "{s:?} must be inside the ab-prefix range"
            );
        }
        let outside = ["", "a", "a\0b", "ac", "b"];
        for s in outside {
            let k = encode_text(s);
            let below = matches!(r.lower, Bound::Included(ref lo) if k < *lo);
            let above = matches!(r.upper, Bound::Excluded(ref hi) if k >= *hi);
            assert!(
                below || above,
                "{s:?} must be outside the ab-prefix range"
            );
        }
    }

    /// The general multi-column prefix keeps NULL-valued continuations in
    /// range (the excluded upper's second 0xFF outranks the bare marker).
    #[test]
    fn column_prefix_keeps_null_continuations() {
        let fixed = concat(&[encode_int8(1), encode_text("a")]);
        let r = KeyRange::prefix_bytes(fixed);
        let k = concat(&[encode_int8(1), encode_text("a"), encode_null()]);
        assert!(matches!(r.lower, Bound::Included(ref lo) if *lo <= k));
        assert!(matches!(r.upper, Bound::Excluded(ref hi) if k < *hi));
        // A different second column is above the range ('b' > 'a': the key
        // outruns the excluded upper at the differing column byte).
        let above = concat(&[encode_int8(1), encode_text("b")]);
        assert!(matches!(r.upper, Bound::Excluded(ref hi) if above >= *hi));
        // A smaller first column is below it, never inside.
        let below = concat(&[encode_int8(0), encode_text("zzzz")]);
        assert!(matches!(r.lower, Bound::Included(ref lo) if below < *lo));
    }

    /// The one-way doors, pinned: integer flip, text terminator, the null
    /// marker, bool — and the logical index entry. Changing any byte here
    /// is a format decision, not a refactor.
    #[test]
    fn key_bytes_are_pinned() {
        assert_eq!(
            encode_int8(1).0,
            vec![tag::INT, 0x80, 0, 0, 0, 0, 0, 0, 1]
        );
        assert_eq!(encode_text("ab").0, vec![tag::TEXT, b'a', b'b', 0]);
        assert_eq!(encode_null().0, vec![0xFF]);
        assert_eq!(encode_bool(false).0, vec![tag::BOOL, 0]);
        let e = IndexEntry {
            key: encode_int8(1),
            locator: Locator::Heap {
                page_no: 5,
                slot: 2,
            },
            create: VersionWord::uncommitted(cascade_types::Xid(9)),
            delete: VersionWord::EMPTY,
        };
        assert_eq!(
            e.encode(),
            [
                tag::INT, 0x80, 0, 0, 0, 0, 0, 0, 1, // key: int8 = 1
                locator::HEAP, // locator: heap
                5, 0, 0, 0, // page_no = 5
                2, 0, // slot = 2
                9, 0, 0, 0, 0, 0, 0, 0x80, // create: uncommitted(9)
                0, 0, 0, 0, 0, 0, 0, 0, // delete: EMPTY (live)
            ]
        );
        let e = IndexEntry {
            key: encode_text("k"),
            locator: Locator::Pk(encode_text("k")),
            create: VersionWord::committed(cascade_types::Hlc(7)),
            delete: VersionWord::EMPTY,
        };
        let bytes = e.encode();
        assert_eq!(
            bytes,
            [
                tag::TEXT, b'k', 0, // key
                locator::PK, tag::TEXT, b'k', 0, // locator: the same key
                7, 0, 0, 0, 0, 0, 0, 0, // create: commit_ts 7
                0, 0, 0, 0, 0, 0, 0, 0, // delete: EMPTY
            ]
        );
    }
}
