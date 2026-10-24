//! The parsed trace event and its name interner.

use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;

// Malformed-field counter for the serde fallback path. `Deserialize` has
// no stats channel, so lenient coercions (numeric strings, clamped negative
// durations, unusable values replaced by defaults) bump this thread-local;
// the caller drains it with `take_serde_malformed` and folds it into
// `ParseStats::malformed`.
thread_local! {
    static SERDE_MALFORMED: Cell<usize> = const { Cell::new(0) };
}

fn bump_serde_malformed() {
    SERDE_MALFORMED.with(|c| c.set(c.get().saturating_add(1)));
}

/// Drain the serde-fallback malformed counter (see `bump_serde_malformed`).
pub(crate) fn take_serde_malformed() -> usize {
    SERDE_MALFORMED.with(|c| c.replace(0))
}

/// FNV-1a 64 over the id string bytes. Shared by the fast tokenizer and the
/// serde fallback so both paths map a non-numeric id to the same `u64`, and
/// `s`/`f` pairing by `(pid, id)` agrees across paths.
pub(crate) fn hash_id_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Normalize a string id (an `id` or an `id2` local/global) to `u64`:
/// decimal first (so `"42"` pairs with `42`), then `0x`-hex, else the stable
/// hash. Empty and negative strings are "no id" (matching integer ids,
/// where negatives are unpaired).
pub(crate) fn normalize_id_str(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() || t.starts_with('-') {
        return None;
    }
    if let Ok(u) = t.parse::<u64>() {
        return Some(u);
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))
        && let Ok(u) = u64::from_str_radix(hex, 16)
    {
        return Some(u);
    }
    Some(hash_id_str(t))
}

/// Byte-level variant of `normalize_id_str` for the zero-copy fast path.
pub(crate) fn normalize_id_bytes(b: &[u8]) -> Option<u64> {
    std::str::from_utf8(b).ok().and_then(normalize_id_str)
}

/// Normalize a JSON `id` value: only true integers pair (the fast path
/// accepts digits-only for the same reason); floats are "no id".
pub(crate) fn id_from_value(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) if n.is_u64() => n.as_u64(),
        Value::Number(_) => None,
        Value::String(s) => normalize_id_str(s),
        _ => None,
    }
}

/// Normalize `id2`: prefer `local`, fall back to `global`; a bare
/// string/number is treated like `id`.
pub(crate) fn id2_from_value(v: &Value) -> Option<u64> {
    match v {
        Value::Object(m) => m
            .get("local")
            .and_then(id_from_value)
            .or_else(|| m.get("global").and_then(id_from_value)),
        _ => id_from_value(v),
    }
}

/// Shared `tid`/`pid` string coercion (also used by the fast path):
/// signed, unsigned, then float truncation; 0 when unusable.
pub(crate) fn parse_tid_pid_str(s: &str) -> u64 {
    let t = s.trim();
    t.parse::<i64>()
        .map(|i| i as u64)
        .or_else(|_| t.parse::<u64>())
        .or_else(|_| t.parse::<f64>().map(|f| f as u64))
        .unwrap_or(0)
}

/// Intern an event name or category into a `&'static str`. Chrome traces reuse
/// a small set of names and categories across millions of events, so
/// interning turns one heap allocation per event into a shared reference —
/// a large memory win and better cache locality. Distinct strings are leaked
/// (bounded by the trace's vocabulary, a few hundred entries at most).
/// Lookups are cached in a per-thread memo, so the hot path never touches
/// the global lock; every cached value comes from the global table, so equal
/// strings still share one pointer across threads.
pub(crate) fn intern_name(s: &str) -> &'static str {
    thread_local! {
        // Direct-mapped 64-slot cache: ~95% of trace event names are RunTask,
        // UpdateLayoutTree, FunctionCall etc. This hits in 1-2ns without touching
        // TLS FxHashMap or locks.
        static CACHE: std::cell::UnsafeCell<[Option<(&'static str, &'static str)>; 64]> = const {
            std::cell::UnsafeCell::new([None; 64])
        };
        // Per-thread memo of the global intern table: the vocabulary is tiny
        // (a few hundred entries), so after warmup every lookup is a local
        // hash hit and the global RwLock is never touched on the hot path.
        // Values always come from the global table, so equal strings still
        // share one pointer across threads.
        static LOCAL: std::cell::RefCell<rustc_hash::FxHashMap<&'static str, &'static str>> =
            std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    }
    let bytes = s.as_bytes();
    let h = (bytes.len()
        ^ (if bytes.is_empty() {
            0
        } else {
            bytes[0] as usize
        }))
        & 63;
    CACHE.with(|cache| {
        let slot = unsafe { &mut (*cache.get())[h] };
        if let Some((k, v)) = *slot
            && k == s
        {
            return v;
        }
        let v = LOCAL.with(|m| {
            if let Some(&v) = m.borrow().get(s) {
                return v;
            }
            let v = intern_global(s);
            m.borrow_mut().insert(v, v);
            v
        });
        *slot = Some((v, v));
        v
    })
}

/// Global intern table backing `intern_name`: the source of truth for
/// pointer identity. The thread-local memo only ever stores values returned
/// from here.
fn intern_global(s: &str) -> &'static str {
    use std::sync::{OnceLock, RwLock};
    static TABLE: OnceLock<RwLock<rustc_hash::FxHashSet<&'static str>>> = OnceLock::new();
    let table = TABLE.get_or_init(|| RwLock::new(rustc_hash::FxHashSet::default()));
    // Fast path: concurrent readers. The name vocabulary is tiny (a few
    // hundred entries), so once warm virtually every lookup hits and runs
    // under a shared read lock with no cross-thread serialization — the
    // parallel tokenizer was contending on a single Mutex here before.
    if let Ok(guard) = table.read()
        && let Some(&existing) = guard.get(s)
    {
        return existing;
    }
    // Slow path: a genuinely new name. Re-check under the write lock (two
    // threads can both miss and race here), then leak the interned copy.
    let mut guard = table.write().unwrap_or_else(|p| p.into_inner());
    if let Some(&existing) = guard.get(s) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// A single parsed event. `name` and `cat` are interned (see `intern_name`)
/// and `ph` is a single byte, so the struct is compact: no per-event
/// allocation for the name, the category, or the phase.
#[derive(Clone, Copy)]
pub struct TraceEvent {
    /// Interned event name (see `intern_name`): a shared `&'static str`
    /// instead of a per-event `String` allocation.
    pub name: &'static str,
    /// Async id: pairs `s` (start) / `f` (finish) events with the same
    /// `(pid, id)`. `0` unless `has_id` is set — `0` is a valid Chrome id
    /// (GC jobs use it), so it cannot serve as an absence sentinel. Placed
    /// right after `name` so the two one-byte flags pack into a single
    /// 8-byte slot and the struct only grows by 8 bytes vs. 16 for
    /// `Option<u64>`.
    pub id: u64,
    /// Whether the trace actually carried an `id` field on this event.
    pub has_id: bool,
    /// Event phase: a single ASCII byte (`X`, `b`, `e`, `P`, `M`, `I`, …),
    /// or `0` when absent/empty.
    pub ph: u8,
    pub ts: f64,
    pub dur: Option<f64>,
    pub tid: u64,
    #[allow(dead_code)]
    pub pid: u64,
    #[allow(dead_code)]
    /// Interned category (see `intern_name`): categories repeat heavily
    /// (a few dozen distinct values), so this is a shared `&'static str`
    /// instead of a per-event allocation.
    pub cat: Option<&'static str>,
    /// Raw `args` JSON text slice directly into the trace buffer (zero-copy).
    pub args: Option<&'static str>,
}

impl<'de> Deserialize<'de> for TraceEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Owned<'a> {
            #[serde(default)]
            name: String,
            #[serde(default)]
            id: Option<Value>,
            #[serde(default)]
            id2: Option<Value>,
            #[serde(default)]
            ph: String,
            #[serde(default, deserialize_with = "deserialize_f64_lenient")]
            ts: f64,
            #[serde(default, deserialize_with = "deserialize_opt_f64_lenient")]
            dur: Option<f64>,
            #[serde(default, deserialize_with = "deserialize_tid_pid")]
            tid: u64,
            #[serde(default, deserialize_with = "deserialize_tid_pid")]
            pid: u64,
            #[serde(default)]
            cat: Option<Box<str>>,
            #[serde(default, deserialize_with = "deserialize_args_raw", borrow)]
            args: Option<&'a serde_json::value::RawValue>,
        }
        let h = Owned::deserialize(deserializer)?;
        // A string-typed `id` is the wrong type but still usable (it hashes
        // to a stable u64); count the coercion. `id2` local/global strings
        // are the spec'd form, so they don't count.
        if matches!(h.id.as_ref(), Some(Value::String(_))) {
            bump_serde_malformed();
        }
        let id_opt =
            h.id.as_ref()
                .and_then(id_from_value)
                .or_else(|| h.id2.as_ref().and_then(id2_from_value));
        let args = h.args.map(|r| {
            let s: &'static str = Box::leak(r.get().to_string().into_boxed_str());
            s
        });
        Ok(TraceEvent {
            name: intern_name(&h.name),
            id: id_opt.unwrap_or(0),
            has_id: id_opt.is_some(),
            ph: h.ph.as_bytes().first().copied().unwrap_or(0),
            ts: h.ts,
            dur: h.dur,
            tid: h.tid,
            pid: h.pid,
            cat: h.cat.as_deref().map(intern_name),
            args,
        })
    }
}

/// Serde fallback for `ts`: numbers as-is, explicit null as absent (0.0),
/// numeric strings coerced (counted as malformed — wrong type but usable),
/// anything else 0.0 (counted). Never fails: one bad field must not fail the
/// whole trace.
fn deserialize_f64_lenient<'de, D>(d: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Value::deserialize(d)?;
    Ok(match &v {
        Value::Null => 0.0,
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => {
            bump_serde_malformed();
            s.trim().parse::<f64>().unwrap_or(0.0)
        }
        _ => {
            bump_serde_malformed();
            0.0
        }
    })
}

/// Serde fallback for `dur`: like `ts` but optional; negative durations
/// clamp to 0.0 (counted as malformed, event kept). Never fails.
fn deserialize_opt_f64_lenient<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v.as_ref() {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f < 0.0 {
                bump_serde_malformed();
                Some(0.0)
            } else {
                Some(f)
            }
        }
        Some(Value::String(s)) => {
            bump_serde_malformed();
            match s.trim().parse::<f64>() {
                Ok(f) if f < 0.0 => Some(0.0),
                Ok(f) => Some(f),
                Err(_) => None,
            }
        }
        _ => {
            bump_serde_malformed();
            None
        }
    })
}

/// tid/pid can legitimately be negative: Windows thread/process ids above 2³¹
/// come out of Chromium's exporter as negative i32s, and Chrome's own tooling
/// accepts them. Wrap through i64 instead of failing the whole trace.
/// Numeric strings coerce the same way (counted as malformed); floats
/// truncate like the fast path; anything else is 0 (counted). Never fails.
fn deserialize_tid_pid<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Value::deserialize(d)?;
    Ok(match &v {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                u
            } else if let Some(i) = n.as_i64() {
                i as u64
            } else {
                n.as_f64().map(|f| f as u64).unwrap_or(0)
            }
        }
        Value::String(s) => {
            bump_serde_malformed();
            parse_tid_pid_str(s)
        }
        Value::Null => 0,
        _ => {
            bump_serde_malformed();
            0
        }
    })
}

/// Serde fallback: capture the `args` field as raw JSON bytes (zero-copy via
/// RawValue). Only used when the fast tokenizer falls back to serde.
fn deserialize_args_raw<'de, D>(d: D) -> Result<Option<&'de serde_json::value::RawValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<&'de serde_json::value::RawValue>::deserialize(d)
}

/// Test helper: build an event `args` field from a JSON value (stored raw).
#[cfg(test)]
pub(crate) fn test_args(v: serde_json::Value) -> Option<&'static str> {
    if v.is_null() {
        None
    } else {
        Some(Box::leak(
            serde_json::to_string(&v).unwrap().into_boxed_str(),
        ))
    }
}

/// Pair `B` (begin) / `E` (end) events into synthetic `X` (complete)
/// events, for old-style Chrome exporters that emit `ph:"B"`/`"E"` instead
/// of `X` (every duration analysis filters `ph == b'X'` and would otherwise
/// see zero durations).
///
/// Stack-based pairing per `(pid, tid)`: `B` pushes, `E` pops the most recent
/// unmatched `B` with the same name on the same thread (nesting-safe).
/// Each pair appends one synthetic `X` event (`ts = b.ts`,
/// `dur = (e.ts - b.ts).max(0.0)`, same name/pid/tid/cat/args as the `B`
/// event). Returns the number of synthesized pairs.
///
/// INVARIANT — no double counting: the original `B`/`E` events are left in
/// place (not removed or rewritten) because `paired_durations`
/// (windowed.rs) still needs them for frame-event pairing. That is safe
/// because `B`/`E` phases are never `X`: every `X`-only analysis sees
/// exactly the synthetic event, and `paired_durations` (which only looks at
/// lowercase `b`/`e` frame ticks, a different phase) never sees the
/// synthetic `X`. Unpaired `B`/`E` stay as-is and are ignored by `X`-only
/// passes. `dur` on the synthetic clamps clock-skew (`e.ts < b.ts`) to 0.
pub fn normalize_be_pairs(events: &mut Vec<TraceEvent>) -> usize {
    let orig_len = events.len();
    // Per-thread stack of unmatched begins: (name, event copy).
    let mut stacks: rustc_hash::FxHashMap<(u64, u64), Vec<TraceEvent>> =
        rustc_hash::FxHashMap::default();
    let mut made = 0usize;
    for i in 0..orig_len {
        let e = events[i];
        match e.ph {
            b'B' => {
                stacks.entry((e.pid, e.tid)).or_default().push(e);
            }
            b'E' => {
                if let Some(st) = stacks.get_mut(&(e.pid, e.tid))
                    && let Some(pos) = st.iter().rposition(|b| b.name == e.name)
                {
                    let b = st.remove(pos);
                    events.push(TraceEvent {
                        name: b.name,
                        id: 0,
                        has_id: false,
                        ph: b'X',
                        ts: b.ts,
                        dur: Some((e.ts - b.ts).max(0.0)),
                        tid: b.tid,
                        pid: b.pid,
                        cat: b.cat,
                        args: b.args,
                    });
                    made += 1;
                }
            }
            _ => {}
        }
    }
    made
}

impl TraceEvent {
    /// Parsed `args` JSON, or `None` when the event carries no args.
    /// Parsed on-demand; no heavy JSON DOM trees retained in memory.
    #[inline]
    pub fn args_value(&self) -> Option<serde_json::Value> {
        self.args_raw().and_then(|r| serde_json::from_str(r).ok())
    }

    /// Raw `args` JSON text, or `None`.
    #[inline]
    pub fn args_raw(&self) -> Option<&'static str> {
        self.args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be(
        name: &str,
        ph: u8,
        ts: f64,
        pid: u64,
        tid: u64,
        cat: Option<&'static str>,
        args: Option<&'static str>,
    ) -> TraceEvent {
        TraceEvent {
            name: intern_name(name),
            id: 0,
            has_id: false,
            ph,
            ts,
            dur: None,
            tid,
            pid,
            cat,
            args,
        }
    }

    /// Nested same-thread pairs synthesize one X each; originals stay.
    #[test]
    fn be_pairs_synthesize_x_without_removing_originals() {
        let cat: &'static str = intern_name("devtools.timeline");
        let args: &'static str = Box::leak(r#"{"x":1}"#.to_string().into_boxed_str());
        let mut events = vec![
            be("RunTask", b'B', 0.0, 1, 1, Some(cat), Some(args)),
            be("FunctionCall", b'B', 10_000.0, 1, 1, None, None),
            be("FunctionCall", b'E', 30_000.0, 1, 1, None, None),
            be("RunTask", b'E', 100_000.0, 1, 1, None, None),
        ];
        assert_eq!(normalize_be_pairs(&mut events), 2);
        assert_eq!(events.len(), 6);
        assert!(
            events[..4].iter().all(|e| e.ph == b'B' || e.ph == b'E'),
            "originals must be untouched"
        );
        let xs: Vec<&TraceEvent> = events.iter().filter(|e| e.ph == b'X').collect();
        assert_eq!(xs.len(), 2);
        let rt = xs.iter().find(|e| e.name == "RunTask").unwrap();
        assert_eq!(rt.ts, 0.0);
        assert_eq!(rt.dur, Some(100_000.0));
        assert_eq!(
            (rt.pid, rt.tid, rt.cat, rt.args),
            (1, 1, Some(cat), Some(args))
        );
        let fc = xs.iter().find(|e| e.name == "FunctionCall").unwrap();
        assert_eq!((fc.ts, fc.dur), (10_000.0, Some(20_000.0)));
    }

    /// Interleaved names pair by name (LIFO per name); unpaired and
    /// cross-thread events are left alone; skew clamps to 0.
    #[test]
    fn be_pairing_edge_cases() {
        let mut events = vec![
            be("A", b'B', 0.0, 1, 1, None, None),
            be("B", b'B', 1_000.0, 1, 1, None, None),
            be("A", b'E', 5_000.0, 1, 1, None, None), // pairs A[0,5000]
            be("B", b'E', 3_000.0, 1, 1, None, None), // pairs B[1000,3000]
            be("C", b'B', 0.0, 1, 1, None, None),     // unpaired B
            be("D", b'E', 9_000.0, 1, 1, None, None), // unpaired E
            be("A", b'B', 0.0, 1, 2, None, None),     // other thread
            be("A", b'E', 7_000.0, 1, 9, None, None), // other thread
            be("S", b'B', 10_000.0, 1, 1, None, None),
            be("S", b'E', 9_000.0, 1, 1, None, None), // skew -> 0
        ];
        assert_eq!(normalize_be_pairs(&mut events), 3);
        let mut durs: Vec<f64> = events
            .iter()
            .filter(|e| e.ph == b'X')
            .map(|e| e.dur.unwrap())
            .collect();
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(durs, vec![0.0, 2_000.0, 5_000.0]);
    }

    /// Lowercase frame phases (`b`/`e`) are a different phase and untouched.
    #[test]
    fn be_leaves_frame_phases_alone() {
        let mut events = vec![
            be("DrawFrame", b'b', 0.0, 1, 1, None, None),
            be("DrawFrame", b'e', 8_000.0, 1, 1, None, None),
        ];
        assert_eq!(normalize_be_pairs(&mut events), 0);
        assert_eq!(events.len(), 2);
    }

    fn event_from_json(s: &str) -> TraceEvent {
        serde_json::from_str(s).expect("test event must deserialize")
    }

    /// `normalize_id_str` is stable and platform-independent: decimal, then
    /// `0x`-hex, then FNV-1a over the string bytes with explicit `u64`
    /// arithmetic (no `usize`, so identical on 32- and 64-bit). The fast
    /// byte path and the serde `id`/`id2` wrappers must agree exactly —
    /// `s`/`f` pairing by `(pid, id)` depends on it.
    #[test]
    fn normalize_id_str_is_stable_across_paths() {
        assert_eq!(normalize_id_str("42"), Some(42));
        assert_eq!(normalize_id_str("0xABC"), Some(0xABC));
        assert_eq!(normalize_id_str("0Xabc"), Some(0xABC));
        assert_eq!(normalize_id_str("  7  "), Some(7));
        assert_eq!(normalize_id_str(""), None);
        assert_eq!(normalize_id_str("   "), None);
        assert_eq!(normalize_id_str("-3"), None);
        assert_eq!(normalize_id_str("-0x1"), None);
        // Non-numeric strings hash; all three spellings agree.
        let h = normalize_id_str("x7").expect("hashable");
        assert_eq!(h, hash_id_str("x7"));
        assert_eq!(normalize_id_bytes(b"x7"), Some(h));
        assert_eq!(normalize_id_bytes(b"42"), Some(42));
        assert_eq!(normalize_id_bytes(b""), None);
        assert_eq!(normalize_id_bytes(b"-3"), None);
        assert_eq!(id_from_value(&serde_json::json!("x7")), Some(h));
        assert_eq!(id_from_value(&serde_json::json!(42)), Some(42));
        assert_eq!(id_from_value(&serde_json::json!(-3)), None);
        assert_eq!(id_from_value(&serde_json::json!(4.5)), None);
        assert_eq!(
            id2_from_value(&serde_json::json!({"local": "x7", "global": "other"})),
            Some(h)
        );
        assert_eq!(
            id2_from_value(&serde_json::json!({"global": "other"})),
            normalize_id_str("other")
        );
    }

    /// The thread-local memo must not break global pointer identity: the
    /// same string interned on two different threads yields one pointer.
    #[test]
    fn intern_name_shares_pointer_across_threads() {
        let a = intern_name("chperf-test-cross-thread-cat");
        let b = std::thread::spawn(|| intern_name("chperf-test-cross-thread-cat"))
            .join()
            .expect("worker must not panic");
        assert!(
            std::ptr::eq(a, b),
            "interned pointers must match across threads"
        );
        // And the main thread's memo serves the same pointer on repeat.
        assert!(std::ptr::eq(a, intern_name("chperf-test-cross-thread-cat")));
    }

    /// Equal `cat` values intern to the same pointer (serde fallback path),
    /// and a different category compares unequal.
    #[test]
    fn cat_interning_shares_pointer_for_equal_values() {
        // Individually-built JSON strings so each `cat` starts as its own
        // allocation before interning (serde always allocates fresh Strings
        // while deserializing, whatever the source literal).
        let a =
            event_from_json(r#"{"name":"RunTask","cat":"devtools.timeline","ph":"X","ts":1.0}"#);
        let b =
            event_from_json(r#"{"name":"RunTask","cat":"devtools.timeline","ph":"X","ts":2.0}"#);
        let c = event_from_json(r#"{"name":"RunTask","cat":"v8","ph":"X","ts":3.0}"#);
        let (Some(ca), Some(cb), Some(cc)) = (a.cat, b.cat, c.cat) else {
            panic!("cats must survive the serde fallback path");
        };
        assert_eq!(ca, "devtools.timeline");
        assert!(
            std::ptr::eq(ca, cb),
            "same cat must share one interned pointer"
        );
        assert_eq!(ca, cb);
        assert_ne!(ca, cc);
        assert!(!std::ptr::eq(ca, cc));
        // Missing `cat` stays `None`.
        let n = event_from_json(r#"{"name":"RunTask","ph":"X","ts":4.0}"#);
        assert!(n.cat.is_none());
    }

    /// The fast tokenizer path interns `cat` too: same-cat events from a
    /// real `parse_trace` share one pointer.
    #[test]
    fn cat_interned_on_fast_path() {
        let json = serde_json::json!({
            "traceEvents": [
                {"name": "RunTask", "cat": "devtools.timeline", "ph": "X", "ts": 1.0, "pid": 1, "tid": 1},
                {"name": "RunTask", "cat": "devtools.timeline", "ph": "X", "ts": 2.0, "pid": 1, "tid": 1},
                {"name": "V8", "cat": "v8", "ph": "X", "ts": 3.0, "pid": 1, "tid": 1}
            ]
        });
        let path =
            std::env::temp_dir().join(format!("chperf-cat-intern-{}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        let trace = super::super::parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let evs = &trace.trace_events;
        assert_eq!(evs.len(), 3);
        let (Some(ca), Some(cb), Some(cc)) = (evs[0].cat, evs[1].cat, evs[2].cat) else {
            panic!("cats must survive the fast tokenizer path");
        };
        assert!(
            std::ptr::eq(ca, cb),
            "same cat must share one interned pointer"
        );
        assert_eq!(ca, cb);
        assert_ne!(ca, cc);
    }

    /// `args_value()` parses on demand without retaining memory;
    /// missing or invalid raw args give `None`.
    #[test]
    fn args_value_parses_on_demand() {
        let with_args =
            event_from_json(r#"{"name":"E","ph":"X","ts":1.0,"args":{"foo":42,"bar":[1,2]}}"#);
        let first = with_args.args_value().expect("args must parse");
        assert_eq!(first.get("foo").and_then(|v| v.as_u64()), Some(42));
        let second = with_args.args_value().expect("args must parse again");
        assert_eq!(first, second);

        let without_args = event_from_json(r#"{"name":"E","ph":"X","ts":1.0}"#);
        assert!(without_args.args_value().is_none());

        let invalid = TraceEvent {
            name: intern_name("E"),
            id: 0,
            has_id: false,
            ph: b'X',
            ts: 0.0,
            dur: None,
            tid: 0,
            pid: 0,
            cat: None,
            args: Some("{not valid json"),
        };
        assert!(invalid.args_value().is_none());
        assert!(invalid.args_value().is_none());
    }

    /// Two events with individually-built equal `cat` values compare equal
    /// by value (the `contains_ignore_case` filtering in inspect.rs relies
    /// on value comparison, not pointer identity).
    #[test]
    fn equal_cats_compare_equal_by_value() {
        let a =
            event_from_json(r#"{"name":"RunTask","cat":"DevTools.Timeline","ph":"X","ts":1.0}"#);
        let b =
            event_from_json(r#"{"name":"RunTask","cat":"DevTools.Timeline","ph":"X","ts":2.0}"#);
        assert_eq!(a.cat, b.cat);
        assert_eq!(a.cat, Some("DevTools.Timeline"));
        assert!(crate::inspect::contains_ignore_case(
            a.cat.unwrap(),
            "devtools.timeline"
        ));
    }
}
