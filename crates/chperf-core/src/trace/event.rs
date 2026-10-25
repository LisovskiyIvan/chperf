//! The parsed trace event model (fully owned, no borrowed buffers).
//!
//! Every `TraceEvent` owns its strings (`name`/`cat` shared via refcounted
//! interning, `args` as buffer spans). Parsing shares the small slices it
//! needs out of the file buffer and drops the buffer with the last event,
//! so a parsed `TraceFile` is self-contained: no `Box::leak`, no
//! `mem::forget`, no `'static` transmutes anywhere on the parse path, and
//! repeated `parse_trace` calls retain no memory.

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

/// Reference-counted shared string for event `name`/`cat`.
///
/// Chrome traces reuse a tiny vocabulary (~300 names, ~30 cats across ~1M
/// events), so one `String` per event is ~2M small allocations of duplicated
/// bytes. `SharedStr` clones by bumping an `Arc` instead, and the fast
/// tokenizer interns through [`StrInterner`] so each distinct value allocates
/// exactly once per parse. Reads like `&str`: [`Self::as_str`], `Deref`,
/// `Display`, `PartialEq<&str>` and friends cover every existing use, and
/// `Debug`/`Serialize` render exactly like `String` so outputs are unchanged.
#[derive(Clone)]
pub struct SharedStr(std::sync::Arc<str>);

impl SharedStr {
    /// Empty string (no allocation: the empty `Arc<str>`).
    pub fn empty() -> Self {
        Self(std::sync::Arc::from(""))
    }

    /// Borrow as `&str`.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SharedStr {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::ops::Deref for SharedStr {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for SharedStr {
    #[inline]
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for SharedStr {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Debug for SharedStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for SharedStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for SharedStr {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SharedStr {}

impl std::hash::Hash for SharedStr {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl PartialOrd for SharedStr {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SharedStr {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

macro_rules! shared_str_eq {
    ($($rhs:ty),*) => {$(
        impl PartialEq<$rhs> for SharedStr {
            #[inline]
            fn eq(&self, other: &$rhs) -> bool {
                self.as_str() == &other[..]
            }
        }
        impl PartialEq<SharedStr> for $rhs {
            #[inline]
            fn eq(&self, other: &SharedStr) -> bool {
                &self[..] == other.as_str()
            }
        }
    )*};
}

shared_str_eq!(str, &'_ str, String, &'_ String);

impl From<&str> for SharedStr {
    #[inline]
    fn from(s: &str) -> Self {
        Self(std::sync::Arc::from(s))
    }
}

impl From<String> for SharedStr {
    #[inline]
    fn from(s: String) -> Self {
        Self(std::sync::Arc::from(s))
    }
}

impl From<Box<str>> for SharedStr {
    #[inline]
    fn from(s: Box<str>) -> Self {
        Self(std::sync::Arc::from(s))
    }
}

impl serde::Serialize for SharedStr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Per-parse name/category interner: one allocation per distinct value.
/// Keys borrow the parse buffer (valid for the whole parse); values own
/// their `Arc<str>`, so interned strings outlive the parse. Single-threaded
/// by design — each parse thread (fast path, parallel chunks) holds its own.
#[derive(Default)]
pub(crate) struct StrInterner<'a> {
    map: rustc_hash::FxHashMap<&'a str, SharedStr>,
}

impl<'a> StrInterner<'a> {
    pub(crate) fn with_capacity(n: usize) -> Self {
        Self {
            map: rustc_hash::FxHashMap::with_capacity_and_hasher(
                n,
                rustc_hash::FxBuildHasher,
            ),
        }
    }

    /// The shared string for `s` (already UTF-8 validated by the caller):
    /// a clone on hit, a single allocation on first sight.
    pub(crate) fn intern(&mut self, s: &'a str) -> SharedStr {
        if let Some(v) = self.map.get(s) {
            return v.clone();
        }
        let v = SharedStr::from(s);
        self.map.insert(s, v.clone());
        v
    }
}

/// Zero-copy `args` text: a span of a shared JSON buffer.
///
/// The fast tokenizer never copies `args` (often the largest per-event
/// payload — ~140MB on a 663K-event trace): each event holds an `Arc` to the
/// whole uncompressed buffer plus its byte span. The buffer lives exactly as
/// long as any event references it and is freed with the last event — no
/// leaks, no lifetime parameters on `TraceEvent`. Cloning an event (B/E
/// pairing, synthetic `X`) bumps a refcount instead of copying kilobytes.
///
/// The serde fallback and test paths own small dedicated buffers instead of
/// sharing (see `ArgsText::copy`).
#[derive(Clone, Debug)]
pub struct ArgsText {
    buf: std::sync::Arc<[u8]>,
    start: u32,
    len: u32,
}

impl ArgsText {
    /// Span of a shared buffer (the fast parse path). Falls back to a
    /// dedicated copy when the span exceeds `u32` (a >4GB buffer — never in
    /// practice; Chrome traces are orders of magnitude smaller).
    pub(crate) fn shared(buf: &std::sync::Arc<[u8]>, start: usize, end: usize) -> Self {
        match (u32::try_from(start), u32::try_from(end.saturating_sub(start))) {
            (Ok(s), Ok(l)) => Self {
                buf: std::sync::Arc::clone(buf),
                start: s,
                len: l,
            },
            _ => Self::copy(std::str::from_utf8(buf.get(start..end).unwrap_or(b"")).unwrap_or("")),
        }
    }

    /// Owned copy of a small string (serde fallback, tests).
    pub(crate) fn copy(s: &str) -> Self {
        debug_assert!(s.len() <= u32::MAX as usize);
        Self {
            buf: std::sync::Arc::from(s.as_bytes()),
            start: 0,
            len: u32::try_from(s.len()).unwrap_or(u32::MAX),
        }
    }

    /// The text, or `None` when the span is out of bounds or not UTF-8
    /// (cannot happen for parser-built values — the fast path validates —
    /// but cheap to guarantee).
    pub fn as_str(&self) -> Option<&str> {
        let s = self.start as usize;
        let l = self.len as usize;
        std::str::from_utf8(self.buf.get(s..s.saturating_add(l))?).ok()
    }
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

/// A single parsed event. `name`/`cat` are interned shared strings (see
/// `SharedStr`/`StrInterner`): cloning an event bumps refcounts instead of
/// copying, and `args` is a zero-copy span of the shared parse buffer (see
/// `ArgsText`) — parsing copies only the small slices it needs, and the
/// buffer is freed with the last event referencing it, so a parsed
/// `TraceFile` is self-contained: no `Box::leak`, no `mem::forget`, no
/// `'static` transmutes anywhere on the parse path, and repeated
/// `parse_trace` calls retain no memory.
/// `name`/`cat` values repeat heavily across a trace; the per-event cost is
/// a refcount bump each (see the parse-throughput test in `parse.rs`).
/// `ph` is a single byte, so the struct stays compact.
#[derive(Clone, Debug)]
pub struct TraceEvent {
    /// Event name (interned; Chrome traces reuse a small vocabulary, so
    /// these compress well and are shared between events).
    pub name: SharedStr,
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
    /// Event category (interned; a few dozen distinct values repeat across
    /// the trace, so this compresses well and is shared between events).
    pub cat: Option<SharedStr>,
    /// Raw `args` JSON text (zero-copy span of the shared parse buffer).
    /// Parsed on demand via `args_value`; no JSON DOM is retained.
    pub args: Option<ArgsText>,
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
            cat: Option<String>,
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
        // Zero-copy span of the raw `args` slice (the source buffer outlives
        // the events via `Arc`; see `ArgsText`).
        let args = h.args.map(|r| ArgsText::copy(r.get()));
        Ok(TraceEvent {
            name: SharedStr::from(h.name),
            id: id_opt.unwrap_or(0),
            has_id: id_opt.is_some(),
            ph: h.ph.as_bytes().first().copied().unwrap_or(0),
            ts: h.ts,
            dur: h.dur,
            tid: h.tid,
            pid: h.pid,
            cat: h.cat.map(SharedStr::from),
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

/// Test helper: build an event `args` field from a JSON value (owned copy).
#[cfg(test)]
pub(crate) fn test_args(v: serde_json::Value) -> Option<ArgsText> {
    if v.is_null() {
        None
    } else {
        Some(ArgsText::copy(&serde_json::to_string(&v).unwrap()))
    }
}

/// Pair `B` (begin) / `E` (end) events into synthetic `X` (complete)
/// events, for old-style Chrome exporters that emit `ph:"B"`/`"E"` instead
/// of `X` (every duration analysis filters `ph == b'X'` and would otherwise
/// see zero durations).
///
/// Pairing is per `(pid, tid, name)`: each `B` pushes, each `E` pops the most
/// recent unmatched `B` with the same name on the same thread. This is
/// exactly the old single-per-thread-stack `rposition`-by-name rule (the
/// most recent same-name `B` is this name-stack's top), but O(1) per event
/// instead of O(stack) scan + O(stack) `remove` shift — the old form was
/// O(n²) when many distinct names piled up unmatched.
///
/// Each pair appends one synthetic `X` event (`ts = b.ts`,
/// `dur = (e.ts - b.ts).max(0.0)`, same name/pid/tid/cat/args as the `B`
/// event). Returns the number of synthesized pairs.
///
/// NOT idempotent — call exactly once per event vec (`parse_trace` does).
/// Originals are kept by design (see below), so a second call re-pairs the
/// same `B`/`E`s and appends duplicates. This function cannot touch
/// `ParseStats` (it only sees the vec): `stats.parsed` still counts raw
/// events only, never the appended synthetics.
///
/// INVARIANT — no double counting: the original `B`/`E` events are left in
/// place (not removed or rewritten) because `paired_durations`
/// (windowed.rs) still needs them for frame-event pairing. That is safe
/// because `B`/`E` phases are never `X`: every `X`-only analysis sees
/// exactly the synthetic event, and `paired_durations` (which only looks at
/// lowercase `b`/`e` frame ticks, a different phase) never sees the
/// synthetic `X`. Unpaired `B`/`E` stay as-is and are ignored by `X`-only
/// passes. `dur` on the synthetic clamps clock-skew (`e.ts < b.ts`) to 0.
///
/// `detect_main_thread` interplay: it counts `RunTask` with `ph == b'X'`
/// only, so unpaired `B`/`E` originals never vote — each pair contributes
/// exactly one synthetic `X`. No double counting, uniform weighting.
pub fn normalize_be_pairs(events: &mut Vec<TraceEvent>) -> usize {
    let orig_len = events.len();
    // Per-(thread, name) stack of unmatched begins (owned event copies).
    let mut stacks: rustc_hash::FxHashMap<(u64, u64, SharedStr), Vec<TraceEvent>> =
        rustc_hash::FxHashMap::default();
    let mut made = 0usize;
    for i in 0..orig_len {
        // Non-begin/end events never touch the stacks: skip the deep clone
        // (typical traces carry a handful of B/E events among ~1M X events).
        let ph = events[i].ph;
        if ph != b'B' && ph != b'E' {
            continue;
        }
        // Clone the begin/end event: the stacks own their entries and the
        // synthetic `X` clones the begin's strings.
        let e = events[i].clone();
        match e.ph {
            b'B' => {
                stacks
                    .entry((e.pid, e.tid, e.name.clone()))
                    .or_default()
                    .push(e);
            }
            b'E' => {
                if let Some(st) = stacks.get_mut(&(e.pid, e.tid, e.name.clone()))
                    && let Some(b) = st.pop()
                {
                    events.push(TraceEvent {
                        name: b.name.clone(),
                        id: 0,
                        has_id: false,
                        ph: b'X',
                        ts: b.ts,
                        dur: Some((e.ts - b.ts).max(0.0)),
                        tid: b.tid,
                        pid: b.pid,
                        cat: b.cat.clone(),
                        args: b.args.clone(),
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
    pub fn args_raw(&self) -> Option<&str> {
        self.args.as_ref().and_then(|a| a.as_str())
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
        cat: Option<&str>,
        args: Option<&str>,
    ) -> TraceEvent {
        TraceEvent {
            name: SharedStr::from(name),
            id: 0,
            has_id: false,
            ph,
            ts,
            dur: None,
            tid,
            pid,
            cat: cat.map(SharedStr::from),
            args: args.map(ArgsText::copy),
        }
    }

    /// Nested same-thread pairs synthesize one X each; originals stay.
    #[test]
    fn be_pairs_synthesize_x_without_removing_originals() {
        let cat = "devtools.timeline".to_owned();
        let args = r#"{"x":1}"#;
        let mut events = vec![
            be("RunTask", b'B', 0.0, 1, 1, Some(&cat), Some(&args)),
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
        assert_eq!((rt.pid, rt.tid), (1, 1));
        assert_eq!(rt.cat.as_deref(), Some(cat.as_str()));
        assert_eq!(rt.args.as_ref().and_then(|a| a.as_str()), Some(args));
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

    /// Many distinct names paired FIFO: per-(thread,name) stacks keep this
    /// linear (the old single-stack `rposition` + `remove` was quadratic
    /// here). Bounds the regression with a generous wall-clock cap.
    #[test]
    fn be_many_distinct_names_scales_linearly() {
        let n = 20_000usize;
        let mut events = Vec::with_capacity(2 * n);
        for i in 0..n {
            let name = format!("Fn{i}");
            events.push(TraceEvent {
                name: SharedStr::from(name),
                id: 0,
                has_id: false,
                ph: b'B',
                ts: i as f64,
                dur: None,
                tid: 1,
                pid: 1,
                cat: None,
                args: None,
            });
        }
        for i in 0..n {
            let name = format!("Fn{i}");
            events.push(TraceEvent {
                name: SharedStr::from(name),
                id: 0,
                has_id: false,
                ph: b'E',
                ts: i as f64 + 5.0,
                dur: None,
                tid: 1,
                pid: 1,
                cat: None,
                args: None,
            });
        }
        let t0 = std::time::Instant::now();
        let made = normalize_be_pairs(&mut events);
        let dt = t0.elapsed();
        assert_eq!(made, n);
        assert_eq!(events.len(), 3 * n);
        assert_eq!(
            events.iter().filter(|e| e.ph == b'X').count(),
            n,
            "one synthetic X per pair"
        );
        assert!(
            dt.as_secs_f64() < 5.0,
            "20k pairs took {:.2}s, expected linear (< 5s)",
            dt.as_secs_f64()
        );
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

    /// Owned names compare equal by value across threads: each parse owns
    /// its strings, so two threads parsing the same name get equal (but
    /// independent) allocations.
    #[test]
    fn owned_names_compare_equal_across_threads() {
        let a = event_from_json(r#"{"name":"RunTask","ph":"X","ts":1.0}"#);
        let b = std::thread::spawn(|| event_from_json(r#"{"name":"RunTask","ph":"X","ts":2.0}"#))
            .join()
            .expect("worker must not panic");
        assert_eq!(a.name, b.name);
        // ... but they are independent allocations, not shared pointers.
        assert!(!std::ptr::eq(a.name.as_str(), b.name.as_str()));
    }

    /// Equal `cat` values compare equal by value on the serde fallback path,
    /// and a different category compares unequal.
    #[test]
    fn cat_values_compare_equal_by_value() {
        let a =
            event_from_json(r#"{"name":"RunTask","cat":"devtools.timeline","ph":"X","ts":1.0}"#);
        let b =
            event_from_json(r#"{"name":"RunTask","cat":"devtools.timeline","ph":"X","ts":2.0}"#);
        let c = event_from_json(r#"{"name":"RunTask","cat":"v8","ph":"X","ts":3.0}"#);
        let (Some(ca), Some(cb), Some(cc)) = (a.cat, b.cat, c.cat) else {
            panic!("cats must survive the serde fallback path");
        };
        assert_eq!(ca, "devtools.timeline");
        assert_eq!(ca, cb);
        assert_ne!(ca, cc);
        // Missing `cat` stays `None`.
        let n = event_from_json(r#"{"name":"RunTask","ph":"X","ts":4.0}"#);
        assert!(n.cat.is_none());
    }

    /// The fast tokenizer path keeps `cat` values: equal cats from a real
    /// `parse_trace` compare equal by value.
    #[test]
    fn cat_kept_on_fast_path() {
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
        let (Some(ca), Some(cb), Some(cc)) =
            (evs[0].cat.clone(), evs[1].cat.clone(), evs[2].cat.clone())
        else {
            panic!("cats must survive the fast tokenizer path");
        };
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
            name: "E".into(),
            id: 0,
            has_id: false,
            ph: b'X',
            ts: 0.0,
            dur: None,
            tid: 0,
            pid: 0,
            cat: None,
            args: Some(ArgsText::copy("{not valid json")),
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
        assert_eq!(a.cat.as_deref(), Some("DevTools.Timeline"));
        assert!(crate::inspect::contains_ignore_case(
            a.cat.as_deref().unwrap(),
            "devtools.timeline"
        ));
    }
}
