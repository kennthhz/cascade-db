//! T1.5: the F4 key contract as an executable spec (WP1 doc + the crate's
//! doc comments are the contract; these tests pin it). Written against the
//! *documented* laws, not the implementation — ordering laws, roundtrip
//! inverses, prefix exactness — sampled by a deterministic xorshift so a
//! failure reproduces from the seed in the assertion.
//!
//! The interior-NUL corner gets its own section: a TEXT value's bytes and
//! the terminator byte share the 0x00 value, so the contract refuses NULs
//! into keys (PG refuses them into text values); these tests pin both the
//! refusal and the raw-byte mechanism that makes the refusal necessary.

use cascade_pgtypes::{
    concat, decode_bool, decode_int8, decode_text, encode_bool, encode_int8, encode_null,
    encode_text, is_null, tag, Bound, KeyBytes, KeyRange, NulInText,
};

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
    fn any_i64(&mut self) -> i64 {
        self.next_u64() as i64
    }
}

/// Boundary-exact values plus pseudo-random samples — the sign flip makes
/// the extremes, the sign boundary, and the "boring middle" all load-bearing.
fn sampled_i64s(seed: u64, n: usize) -> Vec<i64> {
    let mut r = Rng::new(seed);
    let mut vs = vec![
        i64::MIN,
        i64::MIN + 1,
        -1,
        0,
        1,
        42,
        (1 << 40) - 1,
        1 << 40,
        i64::MAX - 1,
        i64::MAX,
    ];
    for _ in 0..n {
        vs.push(r.any_i64());
    }
    vs
}

/// Sampled strings over a multibyte alphabet — exercises UTF-8 byte order
/// (code-point order) without embedding NULs (the legal text domain).
fn sampled_strings(seed: u64, n: usize) -> Vec<String> {
    const ALPHABET: [&str; 5] = ["a", "b", "é", "🦀", "zz"];
    let mut r = Rng::new(seed);
    let mut vs: Vec<String> = [String::new(), "a".into(), "é🦀".into(), "zzzzzz".into()]
        .into_iter()
        .collect();
    for _ in 0..n {
        let len = r.below(7) as usize;
        let mut s = String::new();
        for _ in 0..len {
            s.push_str(ALPHABET[r.below(ALPHABET.len() as u64) as usize]);
        }
        vs.push(s);
    }
    vs
}

/// Roundtrip law: decode inverts encode everywhere on the sampled domain,
/// and every decode refuses a key of another shape (None, never garbage).
#[test]
fn decodes_invert_the_encodings_everywhere_sampled() {
    for v in sampled_i64s(0xA001, 300) {
        assert_eq!(decode_int8(&encode_int8(v)), Some(v), "int {v}");
    }
    for s in sampled_strings(0xA002, 300) {
        assert_eq!(
            decode_text(&encode_text(&s).expect("legal")),
            Some(s.clone()),
            "text {s:?}"
        );
    }
    assert_eq!(decode_bool(&encode_bool(false)), Some(false));
    assert_eq!(decode_bool(&encode_bool(true)), Some(true));
    // Shape refusals: each decoder accepts exactly its own encoding and
    // refuses every other shape (None, never garbage). NULL is no type's
    // value at all.
    let int_key = encode_int8(-9);
    let text_key = encode_text("nope").expect("legal");
    let bool_key = encode_bool(true);
    let null_key = encode_null();
    assert_eq!(decode_int8(&int_key), Some(-9), "own type decodes");
    assert_eq!(decode_text(&int_key), None);
    assert_eq!(decode_bool(&int_key), None);
    assert_eq!(decode_text(&text_key), Some("nope".to_string()));
    assert_eq!(decode_int8(&text_key), None);
    assert_eq!(decode_bool(&text_key), None);
    assert_eq!(decode_bool(&bool_key), Some(true), "own type decodes");
    assert_eq!(decode_int8(&bool_key), None);
    assert_eq!(decode_text(&bool_key), None);
    assert_eq!(decode_int8(&null_key), None);
    assert_eq!(decode_text(&null_key), None);
    assert_eq!(decode_bool(&null_key), None);
    assert!(is_null(&null_key));
}

/// Ordering law: bytewise order agrees with value order, transitively,
/// across every sampled pair — with the boundary extremes included.
#[test]
fn int_keys_order_transitively_like_the_values() {
    let keys: Vec<KeyBytes> = sampled_i64s(0xA003, 400).into_iter().map(encode_int8).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    // sorted-bytes ⇒ sorted-values, element-wise (totality + agreement).
    let val = |k: &KeyBytes| decode_int8(k).expect("all keys are ints");
    for w in sorted.windows(2) {
        assert!(val(&w[0]) < val(&w[1]), "{} !< {}", val(&w[0]), val(&w[1]));
    }
    // And back: value order ⇒ byte order on every consecutive sampled pair.
    let mut vals = sampled_i64s(0xA003, 400);
    vals.sort();
    vals.dedup();
    for w in vals.windows(2) {
        assert!(encode_int8(w[0]) < encode_int8(w[1]));
    }
}

/// Ordering law: text keys order exactly as their strings do (str's Ord is
/// the contract), across multibyte samples and prefix/extension pairs.
#[test]
fn text_keys_order_exactly_like_the_strings() {
    let mut s = sampled_strings(0xA004, 300);
    s.sort();
    s.dedup();
    for w in s.windows(2) {
        let (k0, k1) = (
            encode_text(&w[0]).expect("legal"),
            encode_text(&w[1]).expect("legal"),
        );
        assert!(k0 < k1, "{:?} must sort below {:?}", w[0], w[1]);
    }
}

/// NULLS LAST: the bare marker sorts above every sampled non-null key of
/// every type — the global rule, bytewise, no per-type escape hatch.
#[test]
fn null_sorts_last_over_every_sampled_key() {
    let null = encode_null();
    for v in sampled_i64s(0xA005, 100) {
        assert!(null > encode_int8(v));
    }
    for s in sampled_strings(0xA006, 100) {
        assert!(null > encode_text(&s).expect("legal"), "null must outrank {s:?}");
    }
    assert!(null > encode_bool(true));
    assert!(is_null(&null));
}

/// Concatenation law: bytewise order over concatenated keys == lexicographic
/// tuple order — on the legal (NUL-free) text domain, across width-mixed
/// ints, multibyte text, and prefix/extension ties.
#[test]
fn concatenated_keys_order_like_tuples_on_the_legal_domain() {
    let mut r = Rng::new(0xA007);
    let strings = sampled_strings(0xA008, 40);
    let mut tuples: Vec<(i64, String, i64)> = Vec::new();
    for _ in 0..120 {
        tuples.push((
            sampled_i64s(0xA009, 0)[r.below(10) as usize],
            strings[r.below(strings.len() as u64) as usize].clone(),
            r.any_i64(),
        ));
    }
    let key = |t: &(i64, String, i64)| {
        concat(&[
            encode_int8(t.0),
            encode_text(&t.1).expect("legal"),
            encode_int8(t.2),
        ])
    };
    let mut keys: Vec<(KeyBytes, (i64, String, i64))> =
        tuples.iter().map(|t| (key(t), t.clone())).collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    for w in keys.windows(2) {
        assert!(
            w[0].1 <= w[1].1,
            "byte order claimed {:?} <= {:?}",
            w[0].1,
            w[1].1
        );
    }
}

/// text_prefix exactness: membership ⇔ the string really starts with the
/// prefix — across multibyte payloads, empty prefixes, and extensions.
#[test]
fn text_prefix_covers_exactly_its_strings() {
    let mut r = Rng::new(0xA00B);
    for _ in 0..60 {
        let strings = sampled_strings(0xA00C, 24);
        let p: String = {
            let full = strings[r.below(strings.len() as u64) as usize].clone();
            // Cut at a char boundary (sampled strings are multibyte).
            let cut_chars = r.below(full.chars().count() as u64 + 1) as usize;
            full.chars().take(cut_chars).collect()
        };
        let range = KeyRange::text_prefix(&p);
        let inside = |k: &KeyBytes| {
            matches!(range.lower, Bound::Included(ref lo) if *lo <= *k)
                && matches!(range.upper, Bound::Excluded(ref hi) if *k < *hi)
        };
        for t in &strings {
            assert_eq!(
                inside(&encode_text(t).expect("legal")),
                t.starts_with(&p),
                "prefix {p:?} vs {t:?}: membership must equal starts_with"
            );
        }
    }
}

// --- The interior-NUL corner -------------------------------------------------
//
// A TEXT value's payload byte and the terminator byte share the 0x00
// value, so `["a"]` and `["a\0"]` would produce byte-prefix-ambiguous
// encodings — and a byte-prefix-ambiguous column makes concatenated keys
// misorder (the T1.5 review's confirmed finding). The contract therefore
// refuses NULs into keys, exactly as PG refuses NULs into text values;
// these tests pin the refusal AND the raw-byte mechanism that demands it.

/// The refusal law: no NUL anywhere in the value becomes a key, in any
/// position, and the decoder agrees (it never manufactures a value for a
/// NUL-bearing key).
#[test]
fn text_keys_refuse_nuls_in_every_position() {
    for s in ["\0", "a\0", "\0a", "ab\0cd", "é\0🦀"] {
        assert_eq!(encode_text(s), Err(NulInText), "{s:?} must refuse");
    }
    // [TEXT, 0] is not NUL-bearing: its single 0x00 is the terminator, so it
    // is exactly the encoding of "" and decodes as such.
    assert_eq!(
        decode_text(&KeyBytes(vec![tag::TEXT, 0])),
        Some(String::new()),
        "the terminator alone is the empty string, not a NUL payload"
    );
    // Payload NULs never decode, even when the shape is valid:
    assert_eq!(decode_text(&KeyBytes(vec![tag::TEXT, 0, b'a'])), None);
    let illegal = KeyBytes(vec![tag::TEXT, b'a', 0, 0]);
    assert_eq!(decode_text(&illegal), None, "shape-valid, domain-illegal");
    // The clean boundary: the byte above every legal payload still encodes.
    assert!(encode_text("a").is_ok());
}

/// Why the refusal exists — kept as a raw-bytes demonstration (the API can
/// no longer violate this): hand-built NUL-bearing "keys" misorder against
/// their clean twins. Tuple order says (1,"a",5) < (1,"a\0",3) because "a"
/// is a strict prefix of "a\0"; the bytes say the opposite, because the
/// terminator (0x00) collides with the payload NUL.
#[test]
fn interior_nul_would_break_concat_order_the_refusal_prevents_it() {
    let clean = concat(&[encode_int8(1), KeyBytes(vec![tag::TEXT, b'a', 0]), encode_int8(5)]);
    let nul = concat(&[encode_int8(1), KeyBytes(vec![tag::TEXT, b'a', 0, 0]), encode_int8(3)]);
    assert!(
        nul < clean,
        "raw bytes would misorder the tuple (terminator vs payload NUL) — \
         the reason NulInText exists"
    );
    // And the encoders refuse to ever produce these keys:
    assert_eq!(encode_text("a\0"), Err(NulInText));
}

/// Prefix law: `prefix_bytes` is exact column equality on the legal
/// domain. A byte-prefix twin of the fixed part (a last column that
/// extends the scanned value) can only arise through an interior NUL —
/// which the encoders refuse — so no legal key aliases another column's
/// prefix: same columns + any continuation inside, every different column
/// value outside.
#[test]
fn column_prefix_is_exact_column_equality() {
    let in_range = |r: &KeyRange, k: &KeyBytes| {
        matches!(r.lower, Bound::Included(ref lo) if *lo <= *k)
            && matches!(r.upper, Bound::Excluded(ref hi) if *k < *hi)
    };
    let mut r = Rng::new(0xA00D);
    let ints = sampled_i64s(0xA00E, 0);
    let strings = sampled_strings(0xA00F, 20);
    for _ in 0..80 {
        let a = ints[r.below(ints.len() as u64) as usize];
        let s = strings[r.below(strings.len() as u64) as usize].clone();
        let fixed = concat(&[encode_int8(a), encode_text(&s).expect("legal")]);
        let range = KeyRange::prefix_bytes(fixed);

        // The exact column pair itself, and it plus ANY legal continuation
        // — including the NULL continuation (the bare marker is shorter
        // than the bound's two 0xFFs) and the empty one (the key *is* the
        // lower bound) — is inside.
        for cont in [
            KeyBytes(Vec::new()),
            concat(&[encode_null()]),
            concat(&[encode_int8(3)]),
            concat(&[encode_text("zzz").expect("legal")]),
            concat(&[encode_bool(true)]),
        ] {
            let k = concat(&[
                encode_int8(a),
                encode_text(&s).expect("legal"),
                cont,
            ]);
            assert!(
                in_range(&range, &k),
                "({a}, {s:?}, …) must be inside its own prefix: {k:?}"
            );
        }

        // A different second column — shorter, longer, or disjoint — is
        // outside, whichever side it lands on.
        for s2 in &strings {
            if *s2 == s {
                continue;
            }
            let k = concat(&[encode_int8(a), encode_text(s2).expect("legal")]);
            assert!(
                !in_range(&range, &k),
                "({a}, {s2:?}) must be outside the ({a}, {s:?}, …) prefix"
            );
        }
        // A different first column is outside (int keys are fixed-width).
        let a2 = a.wrapping_add(1);
        let k = concat(&[encode_int8(a2), encode_text(&s).expect("legal")]);
        assert!(!in_range(&range, &k), "first column must dominate");
    }
}
