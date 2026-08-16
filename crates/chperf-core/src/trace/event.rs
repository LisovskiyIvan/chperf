//! The parsed trace event and its name interner.

use serde::Deserialize;

/// Intern an event name into a `&'static str`. Chrome traces reuse a small
/// set of names across millions of events, so interning turns one heap
/// allocation per event into a shared reference — a large memory win and
/// better cache locality. Distinct names are leaked (bounded by the trace's
/// vocabulary, a few hundred entries at most).
pub(crate) fn intern_name(s: &str) -> &'static str {
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

/// A single parsed event. `name` is interned (see `intern_name`) and `ph` is
/// a single byte, so the struct is compact: no per-event allocation for the
/// name or the phase.
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
    pub cat: Option<Box<str>>,
    /// Raw `args` JSON text. The fast tokenizer captures the byte range
    /// without parsing; serde_json's RawValue would validate (full parse) on
    /// construction, which is exactly what we're avoiding. Owned as `Box<str>`
    /// (16 bytes vs 24 for `String`, immutable after parse).
    pub args: Option<Box<str>>,
    pub(crate) args_cache: std::sync::OnceLock<Option<serde_json::Value>>,
}

impl<'de> Deserialize<'de> for TraceEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Serde fallback (the fast tokenizer is the primary path): deserialize
        // into an owned shadow struct, then intern the name and fold the phase
        // to its first byte. A derived impl would treat `name: &'static str`
        // as a borrowed `&str` field and force `'de: 'static`, which breaks
        // the streaming `Deserializer::from_slice` borrow.
        #[derive(Deserialize)]
        struct Owned {
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
            #[serde(default)]
            tid: u64,
            #[serde(default)]
            pid: u64,
            #[serde(default)]
            cat: Option<Box<str>>,
            #[serde(default, deserialize_with = "deserialize_args_raw")]
            args: Option<Box<str>>,
        }
        let h = Owned::deserialize(deserializer)?;
        Ok(TraceEvent {
            name: intern_name(&h.name),
            id: h.id.unwrap_or(0),
            has_id: h.id.is_some(),
            ph: h.ph.as_bytes().first().copied().unwrap_or(0),
            ts: h.ts,
            dur: h.dur,
            tid: h.tid,
            pid: h.pid,
            cat: h.cat,
            args: h.args,
            args_cache: std::sync::OnceLock::new(),
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

/// Serde fallback: capture the `args` field as raw JSON bytes (zero-copy via
/// RawValue) and own them as a `Box<str>`. Only used when the fast tokenizer
/// falls back to serde.
fn deserialize_args_raw<'de, D>(d: D) -> Result<Option<Box<str>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<&'de serde_json::value::RawValue>::deserialize(d)?;
    Ok(raw.map(|r| r.get().into()))
}

/// Test helper: build an event `args` field from a JSON value (stored raw).
#[cfg(test)]
pub(crate) fn test_args(v: serde_json::Value) -> Option<Box<str>> {
    serde_json::to_string(&v).ok().map(String::into_boxed_str)
}

impl Clone for TraceEvent {
    fn clone(&self) -> Self {
        TraceEvent {
            name: self.name,
            id: self.id,
            has_id: self.has_id,
            ph: self.ph,
            ts: self.ts,
            dur: self.dur,
            tid: self.tid,
            pid: self.pid,
            cat: self.cat.clone(),
            args: self.args.clone(),
            args_cache: std::sync::OnceLock::new(),
        }
    }
}

impl TraceEvent {
    /// Parsed `args` JSON, or `None` when the event carries no args. The
    /// first call per event parses and caches the raw bytes.
    pub fn args_value(&self) -> Option<&serde_json::Value> {
        self.args_cache
            .get_or_init(|| {
                self.args
                    .as_deref()
                    .and_then(|r| serde_json::from_str(r).ok())
            })
            .as_ref()
    }

    /// Raw `args` JSON text, or `None`.
    #[allow(dead_code)]
    pub fn args_raw(&self) -> Option<&str> {
        self.args.as_deref()
    }
}
