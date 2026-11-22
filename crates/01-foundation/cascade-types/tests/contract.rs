//! T1.5: the F1 version-word contract as an executable spec (WP1 doc +
//! the crate's doc comments are the contract; these tests pin it). The
//! laws: the word space partitions into exactly three disjoint classes,
//! constructors roundtrip through their accessors, a legal committed word
//! can never collide with the uncommitted tag (the HLC epoch's whole
//! point), and the HLC orders by (ms, logical).

use cascade_types::{Hlc, VersionWord, Xid, HLC_EPOCH_UNIX_MS};

/// Deterministic xorshift64\* — reproducible samples, no dependency. Every
/// assertion names its seed so a red is replayable by hand.
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

/// The partition law: every raw word is exactly one of EMPTY /
/// uncommitted / committed-nonzero — the classes are total and disjoint.
/// Sampled words plus the class boundaries (0, 1, 2^63−1, 2^63, u64::MAX).
#[test]
fn the_word_space_partitions_exactly() {
    let mut r = Rng::new(0xB001);
    let mut words = vec![0u64, 1, (1 << 63) - 1, 1 << 63, u64::MAX];
    for _ in 0..500 {
        words.push(r.next_u64());
    }
    for w in words {
        let word = VersionWord::from_raw(w);
        let classes = [
            word.is_empty(),
            word.is_uncommitted(),
            !word.is_empty() && !word.is_uncommitted(),
        ];
        assert_eq!(
            classes.iter().filter(|b| **b).count(),
            1,
            "word {w:#018x} must be exactly one class"
        );
        // And the class definitions hold exactly:
        assert_eq!(word.is_empty(), w == 0);
        assert_eq!(word.is_uncommitted(), w & (1 << 63) != 0);
    }
}

/// Roundtrip law: `uncommitted(x).xid() == x` and `committed(ts)
/// .commit_ts() == ts` across the sampled domains — xid through the whole
/// 63-bit range, ts through values the epoch can actually issue.
#[test]
fn constructors_roundtrip_through_their_accessors() {
    let mut r = Rng::new(0xB002);
    let mut xids = vec![0u64, 1, (1 << 63) - 1];
    let mut tss = vec![Hlc(1), Hlc(1 << 16), Hlc((1 << 47) - 1)];
    for _ in 0..200 {
        xids.push(r.next_u64() & ((1 << 63) - 1));
        tss.push(Hlc(r.next_u64() & ((1 << 63) - 1)));
    }
    for x in xids {
        let w = VersionWord::uncommitted(Xid(x));
        assert!(w.is_uncommitted());
        assert_eq!(w.xid(), Xid(x), "xid {x} roundtrips");
        assert_ne!(w, VersionWord::EMPTY, "uncommitted is never EMPTY");
    }
    for ts in tss {
        let w = VersionWord::committed(ts);
        assert!(!w.is_uncommitted());
        assert!(!w.is_empty());
        assert_eq!(w.commit_ts(), ts, "commit_ts {} roundtrips", ts.0);
    }
}

/// The collision law — the F1 one-way door's load-bearing claim: every
/// legal committed word has bit 63 clear, every uncommitted word has it
/// set, so the two populations can never share a word and a reader can
/// classify from the bit alone.
#[test]
fn committed_and_uncommitted_words_never_collide() {
    let mut r = Rng::new(0xB003);
    for _ in 0..300 {
        let x = r.next_u64() & ((1 << 63) - 1);
        assert!(VersionWord::uncommitted(Xid(x)).0 >= 1 << 63);
        let ts = r.next_u64() & ((1 << 63) - 1);
        assert!(VersionWord::committed(Hlc(ts)).0 < 1 << 63);
    }
}

/// The tag bit is not for callers: an xid or a commit_ts that would set it
/// panics at the constructor — a clock that ignored the epoch contract or
/// an xid space that grew past 63 bits must fail here, not corrupt every
/// row it stamps.
#[test]
#[should_panic(expected = "overflows 63 bits")]
fn uncommitted_refuses_an_xid_into_the_tag_bit() {
    VersionWord::uncommitted(Xid(1 << 63));
}

#[test]
#[should_panic(expected = "has bit 63 set")]
fn committed_refuses_a_timestamp_with_the_tag_bit_set() {
    VersionWord::committed(Hlc(1 << 63));
}

/// EMPTY is reserved zero: never produced by either constructor (both
/// domains exclude 0 — the xid/tag law makes uncommitted words ≥ 2^63, and
/// the HLC epoch keeps physical time strictly positive).
#[test]
fn empty_is_never_produced_by_a_constructor() {
    assert_ne!(VersionWord::uncommitted(Xid(0)), VersionWord::EMPTY);
    assert_ne!(VersionWord::committed(Hlc(1)), VersionWord::EMPTY);
    assert!(VersionWord::EMPTY.is_empty());
    assert_eq!(VersionWord::from_raw(0), VersionWord::EMPTY);
}

/// HLC ordering: physical ms dominates, the logical counter breaks ties —
/// the derived Ord over the packed word must equal (ms, logical) tuple
/// order across sampled pairs.
#[test]
fn hlc_orders_by_ms_then_logical() {
    let mut r = Rng::new(0xB004);
    let ms_hi = (HLC_EPOCH_UNIX_MS + 1_000) << 16;
    let ms_lo = HLC_EPOCH_UNIX_MS << 16;
    for _ in 0..300 {
        let l1 = r.below(1 << 16) as u64;
        let l2 = r.below(1 << 16) as u64;
        assert_eq!(
            Hlc(ms_lo | l1) < Hlc(ms_lo | l2),
            l1 < l2,
            "same ms: logical {l1} vs {l2} must decide"
        );
        let stray = r.next_u64();
        assert!(
            Hlc(ms_lo | stray) < Hlc(ms_hi | stray),
            "a later ms always dominates, whatever the logical counter"
        );
    }
}

/// The epoch law (the cross-WP contract on WP2's clock): the epoch is the
/// documented ms value, it sits well inside the 48-bit physical field, and
/// the tag headroom it buys — bit 63 of every issued word stays clear
/// until the physical part reaches 2^47, about 4 400 years out — is what
/// the committed/uncommitted disjointness rests on. A clock that moved the
/// epoch (or dropped it for Unix time) would fail `committed`'s guard
/// within decades; this test is the early warning at the type boundary.
#[test]
fn the_hlc_epoch_keeps_the_tag_bit_free() {
    assert_eq!(HLC_EPOCH_UNIX_MS, 1_767_225_600_000, "2026-01-01T00:00:00Z");
    // Physical field: ms must stay under 2^47 for bit 63 of the packed
    // word to stay clear (bit 63 of the word = bit 47 of the ms part).
    assert!(
        HLC_EPOCH_UNIX_MS < 1 << 47,
        "the epoch itself must sit inside the tag-free region"
    );
    // Headroom in ms: (2^47 − epoch) / 1000 / 86400 / 365.25 ≈ 4 400 years.
    let headroom_ms = (1u64 << 47) - HLC_EPOCH_UNIX_MS;
    assert!(
        headroom_ms > 4_000u64 * 365 * 24 * 3600 * 1000,
        "the tag-free region must outlast the century: {headroom_ms} ms"
    );
    // End to end: the latest issuable timestamp inside the region packs to
    // a word `committed` accepts.
    let last = ((1u64 << 47) - 1) << 16 | (1 << 16) - 1;
    let _ = VersionWord::committed(Hlc(last));
    // And the first one past it is refused, not silently tagged.
    let overflow = (1u64 << 47) << 16;
    assert!(
        std::panic::catch_unwind(|| VersionWord::committed(Hlc(overflow))).is_err(),
        "a timestamp whose ms reaches bit 47 must refuse, not corrupt the word space"
    );
}
