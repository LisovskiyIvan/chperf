//! The parsed trace event and its name interner.

use serde::Deserialize;

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
    let h = (bytes.len() ^ (if bytes.is_empty() { 0 } else { bytes[0] as usize })) & 63;
    CACHE.with(|cache| {
        let slot = unsafe { &mut (*cache.get())[h] };
        if let Some((k, v)) = *slot {
            if k == s {
                return v;
            }
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
        && let Some(&existing) = guard.get(s) {
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
            #[serde(default, deserialize_with = "deserialize_id")]
            id: Option<u64>,
            #[serde(default)]
            ph: String,
            #[serde(default)]
            ts: f64,
            #[serde(default)]
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
        let args = h.args.map(|r| {
            let s: &'static str = Box::leak(r.get().to_string().into_boxed_str());
            s
        });
        Ok(TraceEvent {
            name: intern_name(&h.name),
            id: h.id.unwrap_or(0),
            has_id: h.id.is_some(),
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

/// Serde fallback for `id`: Chrome traces use an integer id (the common
/// case) but occasionally a string or object id (`id2`). Anything that isn't
/// a non-negative integer is treated as "no id" rather than failing the whole
/// trace — async pairing simply skips those events.
fn deserialize_id<'de, D>(d: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| v.as_u64()))
}

/// tid/pid can legitimately be negative: Windows thread/process ids above 2³¹
/// come out of Chromium's exporter as negative i32s, and Chrome's own tooling
/// accepts them. Wrap through i64 instead of failing the whole trace.
fn deserialize_tid_pid<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(i64::deserialize(d)? as u64)
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
        Some(Box::leak(serde_json::to_string(&v).unwrap().into_boxed_str()))
    }
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

    fn event_from_json(s: &str) -> TraceEvent {
        serde_json::from_str(s).expect("test event must deserialize")
    }

    /// The thread-local memo must not break global pointer identity: the
    /// same string interned on two different threads yields one pointer.
    #[test]
    fn intern_name_shares_pointer_across_threads() {
        let a = intern_name("chperf-test-cross-thread-cat");
        let b = std::thread::spawn(|| intern_name("chperf-test-cross-thread-cat"))
            .join()
            .expect("worker must not panic");
        assert!(std::ptr::eq(a, b), "interned pointers must match across threads");
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
        let a = event_from_json(r#"{"name":"RunTask","cat":"devtools.timeline","ph":"X","ts":1.0}"#);
        let b = event_from_json(r#"{"name":"RunTask","cat":"devtools.timeline","ph":"X","ts":2.0}"#);
        let c = event_from_json(r#"{"name":"RunTask","cat":"v8","ph":"X","ts":3.0}"#);
        let (Some(ca), Some(cb), Some(cc)) = (a.cat, b.cat, c.cat) else {
            panic!("cats must survive the serde fallback path");
        };
        assert_eq!(ca, "devtools.timeline");
        assert!(std::ptr::eq(ca, cb), "same cat must share one interned pointer");
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
        assert!(std::ptr::eq(ca, cb), "same cat must share one interned pointer");
        assert_eq!(ca, cb);
        assert_ne!(ca, cc);
    }

    /// `args_value()` parses on demand without retaining memory;
    /// missing or invalid raw args give `None`.
    #[test]
    fn args_value_parses_on_demand() {
        let with_args = event_from_json(
            r#"{"name":"E","ph":"X","ts":1.0,"args":{"foo":42,"bar":[1,2]}}"#,
        );
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
        let a = event_from_json(r#"{"name":"RunTask","cat":"DevTools.Timeline","ph":"X","ts":1.0}"#);
        let b = event_from_json(r#"{"name":"RunTask","cat":"DevTools.Timeline","ph":"X","ts":2.0}"#);
        assert_eq!(a.cat, b.cat);
        assert_eq!(a.cat, Some("DevTools.Timeline"));
        assert!(
            crate::inspect::contains_ignore_case(a.cat.unwrap(), "devtools.timeline")
        );
    }
}
