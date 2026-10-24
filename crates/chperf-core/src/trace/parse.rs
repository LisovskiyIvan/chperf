//! The hand-rolled, block-parallel `traceEvents` tokenizer and `parse_trace`.

use super::event::{normalize_id_bytes, parse_tid_pid_str, take_serde_malformed};
use super::{FrameInfo, TraceEvent, TraceFile, TraceMetadata};
use std::io::Read;
use std::path::Path;

/// Counters for how a trace parsed. Filled by `parse_trace` (fast path,
/// parallel pipeline and serde fallbacks all fold into the same counters):
/// `parsed` per successfully parsed raw event, `skipped` per event dropped
/// (structurally unusable — never fails the file), `malformed` per event
/// whose fields were usable only after coercion (numeric strings, clamped
/// negative durations) or unusable (drops also count here). `parsed` counts
/// raw events as read — including unpaired `B`/`E` — and does not count the
/// synthetic `X` events `normalize_be_pairs` appends afterwards.
#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct ParseStats {
    pub parsed: usize,
    pub skipped: usize,
    pub malformed: usize,
}

/// Fast path: parse all elements in a chunk's byte range [s, e) without
/// serde. Elements are objects separated by commas (whitespace allowed).
/// Returns Err when ANY element uses a construct the fast parser bails on
/// (escaped strings, unterminated input) — the caller then falls back to a
/// tolerant per-element serde parse on the pristine bytes. Wrong-typed but
/// salvageable fields never bail: they coerce (counted in `stats`).
fn parse_events_fast(
    bytes: &[u8],
    s: usize,
    e: usize,
    stats: &mut ParseStats,
) -> Result<Vec<TraceEvent>, ()> {
    // ~200 bytes per event on Chrome traces: reserve to skip reallocs.
    let mut events: Vec<TraceEvent> = Vec::with_capacity((e - s).max(1) / 200);
    let mut p = s;
    p = skip_ws(bytes, p);
    if p < e && bytes[p] == b'[' {
        p += 1;
    }
    loop {
        p = skip_ws(bytes, p);
        if p >= e || (p < bytes.len() && bytes[p] == b']') {
            break;
        }
        if bytes[p] == b',' {
            p += 1;
            continue;
        }
        let (ev, next) = parse_event_fast(bytes, p, stats)?;
        events.push(ev);
        stats.parsed += 1;
        p = next;
    }
    Ok(events)
}

fn parse_events_array_fast(
    bytes: &[u8],
    s: usize,
    stats: &mut ParseStats,
) -> Result<(Vec<TraceEvent>, usize), ()> {
    let len = bytes.len();
    let mut p = skip_ws(bytes, s);
    if p >= len || bytes[p] != b'[' {
        return Err(());
    }
    p += 1;
    let mut events: Vec<TraceEvent> = Vec::with_capacity((len - p).max(1) / 200);
    loop {
        p = skip_ws(bytes, p);
        if p >= len {
            return Err(());
        }
        if bytes[p] == b']' {
            return Ok((events, p + 1));
        }
        if bytes[p] == b',' {
            p += 1;
            continue;
        }
        let (ev, next) = parse_event_fast(bytes, p, stats)?;
        events.push(ev);
        stats.parsed += 1;
        p = next;
    }
}

fn parse_trace_fast(bytes: &[u8]) -> Result<TraceFile, ()> {
    let len = bytes.len();
    let mut p = skip_ws(bytes, 0);
    if p >= len || bytes[p] != b'{' {
        return Err(());
    }
    p += 1;
    let mut trace_events: Option<Vec<TraceEvent>> = None;
    let mut metadata_span: Option<(usize, usize)> = None;
    let mut stats = ParseStats::default();

    loop {
        p = skip_ws(bytes, p);
        if p >= len {
            return Err(());
        }
        match bytes[p] {
            b'}' => break,
            b',' => {
                p += 1;
                continue;
            }
            b'"' => {}
            _ => return Err(()),
        }
        let (key, after_key) = parse_key(bytes, p)?;
        p = skip_ws(bytes, after_key);
        if p >= len || bytes[p] != b':' {
            return Err(());
        }
        p = skip_ws(bytes, p + 1);
        if p >= len {
            return Err(());
        }
        if key == b"traceEvents" {
            let (evs, next_p) = parse_events_array_fast(bytes, p, &mut stats)?;
            trace_events = Some(evs);
            p = next_p;
        } else if key == b"metadata" {
            if bytes[p] != b'{' {
                return Err(());
            }
            let next_p = skip_value(bytes, p)?;
            metadata_span = Some((p, next_p));
            p = next_p;
        } else {
            p = skip_value(bytes, p)?;
        }
    }

    let trace_events = trace_events.ok_or(())?;
    let metadata = metadata_span.and_then(|(s, e)| serde_json::from_slice(&bytes[s..e]).ok());
    Ok(TraceFile {
        trace_events,
        metadata,
        stats,
    })
}

/// Parse one event object starting at bytes[p] == b'{'. Returns the event
/// and the position after its closing '}'. Wrong-typed but salvageable
/// fields coerce in place (numeric strings for ts/dur/pid/tid, clamped
/// negative dur) and count `malformed`; only constructs the serde fallback
/// handles (escaped strings, unterminated input) return Err.
fn parse_event_fast(
    bytes: &[u8],
    p: usize,
    stats: &mut ParseStats,
) -> Result<(TraceEvent, usize), ()> {
    let len = bytes.len();
    let mut i = p;
    if i >= len || bytes[i] != b'{' {
        return Err(());
    }
    let mut ev = TraceEvent {
        name: String::new(),
        id: 0,
        has_id: false,
        ph: 0,
        ts: 0.0,
        dur: None,
        tid: 0,
        pid: 0,
        cat: None,
        args: None,
    };
    i += 1;
    loop {
        i = skip_ws(bytes, i);
        if i >= len {
            return Err(());
        }
        match bytes[i] {
            b'}' => return Ok((ev, i + 1)),
            b',' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let (key, after) = parse_key(bytes, i)?;
        i = skip_ws(bytes, after);
        if i >= len || bytes[i] != b':' {
            return Err(());
        }
        i = skip_ws(bytes, i + 1);
        if i >= len {
            return Err(());
        }
        match key {
            b"name" => {
                let (v, a) = parse_string_owned(bytes, i)?;
                ev.name = v;
                i = a;
            }
            b"ph" => {
                let (v, a) = parse_phase(bytes, i)?;
                ev.ph = v;
                i = a;
            }
            b"id" => {
                // Integers pair directly. A string id normalizes the same
                // way as `id2` (decimal/hex/hash) so `s`/`f` pairing by
                // (pid, id) agrees with the serde fallback; negatives stay
                // unpaired. An object is parsed leniently as `id2`.
                if bytes[i] == b'"' {
                    let (inner, a) = parse_quoted_inner(bytes, i)?;
                    if let Some(v) = normalize_id_bytes(inner) {
                        ev.id = v;
                        ev.has_id = true;
                    }
                    stats.malformed += 1;
                    i = a;
                } else if bytes[i] == b'{' {
                    let (opt, a) = parse_id2_object(bytes, i)?;
                    if let Some(v) = opt {
                        ev.id = v;
                        ev.has_id = true;
                    }
                    i = a;
                } else {
                    match parse_num_u64(bytes, i) {
                        Ok((v, a)) => {
                            ev.id = v;
                            ev.has_id = true;
                            i = a;
                        }
                        Err(_) => {
                            i = skip_value(bytes, i)?;
                        }
                    }
                }
            }
            b"id2" => {
                // Chrome async ids: `{"local":"0x…","global":"…"}` (either
                // field may be a string or a number). Prefer `local`; the
                // normalization matches the serde fallback exactly.
                if bytes[i] == b'{' {
                    let (opt, a) = parse_id2_object(bytes, i)?;
                    if let Some(v) = opt {
                        ev.id = v;
                        ev.has_id = true;
                    }
                    i = a;
                } else if bytes[i] == b'"' {
                    let (inner, a) = parse_quoted_inner(bytes, i)?;
                    if let Some(v) = normalize_id_bytes(inner) {
                        ev.id = v;
                        ev.has_id = true;
                    }
                    i = a;
                } else {
                    match parse_num_u64(bytes, i) {
                        Ok((v, a)) => {
                            ev.id = v;
                            ev.has_id = true;
                            i = a;
                        }
                        Err(_) => {
                            i = skip_value(bytes, i)?;
                        }
                    }
                }
            }
            b"ts" => {
                // Numeric strings coerce (counted); explicit null reads as
                // absent; other wrong types keep the event at 0.0 (counted).
                if bytes[i] == b'n' {
                    if !is_literal_null(bytes, i) {
                        return Err(());
                    }
                    i = skip_value(bytes, i)?;
                } else if bytes[i] == b'"' {
                    let (inner, a) = parse_quoted_inner(bytes, i)?;
                    let s = std::str::from_utf8(inner).map_err(|_| ())?;
                    ev.ts = s.trim().parse::<f64>().map_err(|_| ())?;
                    stats.malformed += 1;
                    i = a;
                } else {
                    match parse_num_f64(bytes, i) {
                        Ok((v, a)) => {
                            ev.ts = v;
                            i = a;
                        }
                        Err(_) => {
                            i = skip_value(bytes, i)?;
                            stats.malformed += 1;
                        }
                    }
                }
            }
            b"dur" => {
                // Numeric strings coerce and negatives clamp to 0.0 (both
                // counted, event kept); other wrong types keep None (counted).
                if bytes[i] == b'n' {
                    if !is_literal_null(bytes, i) {
                        return Err(());
                    }
                    i = skip_value(bytes, i)?;
                    ev.dur = None;
                } else if bytes[i] == b'"' {
                    let (inner, a) = parse_quoted_inner(bytes, i)?;
                    let s = std::str::from_utf8(inner).map_err(|_| ())?;
                    stats.malformed += 1;
                    ev.dur = match s.trim().parse::<f64>() {
                        Ok(f) if f < 0.0 => Some(0.0),
                        Ok(f) => Some(f),
                        Err(_) => None,
                    };
                    i = a;
                } else {
                    match parse_num_f64(bytes, i) {
                        Ok((v, a)) => {
                            if v < 0.0 {
                                stats.malformed += 1;
                                ev.dur = Some(0.0);
                            } else {
                                ev.dur = Some(v);
                            }
                            i = a;
                        }
                        Err(_) => {
                            i = skip_value(bytes, i)?;
                            stats.malformed += 1;
                            ev.dur = None;
                        }
                    }
                }
            }
            b"tid" => {
                let (v, a) = parse_tid_pid_fast(bytes, i, stats)?;
                ev.tid = v;
                i = a;
            }
            b"pid" => {
                let (v, a) = parse_tid_pid_fast(bytes, i, stats)?;
                ev.pid = v;
                i = a;
            }
            b"cat" => {
                if bytes[i] == b'n' {
                    if !is_literal_null(bytes, i) {
                        return Err(());
                    }
                    i = skip_value(bytes, i)?;
                    ev.cat = None;
                } else {
                    let (v, a) = parse_string_owned(bytes, i)?;
                    ev.cat = Some(v);
                    i = a;
                }
            }
            b"args" => {
                if bytes[i] == b'n' {
                    if !is_literal_null(bytes, i) {
                        return Err(());
                    }
                    i = skip_value(bytes, i)?;
                    ev.args = None;
                } else {
                    let end = skip_value(bytes, i)?;
                    let s = std::str::from_utf8(&bytes[i..end]).map_err(|_| ())?;
                    // Owned copy: the file buffer is freed after parsing,
                    // so events must not borrow from it.
                    ev.args = Some(Box::<str>::from(s));
                    i = end;
                }
            }
            _ => {
                i = skip_value(bytes, i)?;
            }
        }
        i = skip_ws(bytes, i);
        if i >= len {
            return Err(());
        }
        match bytes[i] {
            b',' => i += 1,
            b'}' => return Ok((ev, i + 1)),
            _ => return Err(()),
        }
    }
}

fn skip_ws(bytes: &[u8], mut p: usize) -> usize {
    let len = bytes.len();
    while p < len && bytes[p].is_ascii_whitespace() {
        p += 1;
    }
    p
}

/// Raw byte slice of a quoted key (without the quotes). Err on escaped or
/// unterminated keys.
fn parse_key(bytes: &[u8], p: usize) -> Result<(&[u8], usize), ()> {
    let len = bytes.len();
    if p >= len || bytes[p] != b'"' {
        return Err(());
    }
    match memchr::memchr2(b'"', b'\\', &bytes[p + 1..]) {
        Some(rel) => {
            let q = p + 1 + rel;
            if bytes[q] == b'\\' {
                return Err(());
            }
            Ok((&bytes[p + 1..q], q + 1))
        }
        None => Err(()),
    }
}

/// Unescaped JSON string value (bytes between quotes), owned per event.
/// Escaped strings are handled by the serde fallback. Returns (owned string,
/// pos after closing quote).
fn parse_string_owned(bytes: &[u8], p: usize) -> Result<(String, usize), ()> {
    let len = bytes.len();
    if p >= len || bytes[p] != b'"' {
        return Err(());
    }
    match memchr::memchr2(b'"', b'\\', &bytes[p + 1..]) {
        Some(rel) => {
            let q = p + 1 + rel;
            if bytes[q] == b'\\' {
                return Err(());
            }
            let s = std::str::from_utf8(&bytes[p + 1..q]).map_err(|_| ())?;
            Ok((s.to_owned(), q + 1))
        }
        None => Err(()),
    }
}

/// Inner bytes of a quoted string value with no escapes. Escaped or
/// unterminated strings bail to the serde fallback (same contract as
/// `parse_string_intern`). Returns (inner bytes, pos after closing quote).
fn parse_quoted_inner(bytes: &[u8], p: usize) -> Result<(&[u8], usize), ()> {
    let len = bytes.len();
    if p >= len || bytes[p] != b'"' {
        return Err(());
    }
    match memchr::memchr2(b'"', b'\\', &bytes[p + 1..]) {
        Some(rel) => {
            let q = p + 1 + rel;
            if bytes[q] == b'\\' {
                return Err(());
            }
            Ok((&bytes[p + 1..q], q + 1))
        }
        None => Err(()),
    }
}

/// Parse an `id2` object (`{"local":"0x…","global":"…"}`), preferring
/// `local`. String and number forms normalize exactly like `id`; anything
/// else leaves "no id". Returns the id (if any) and the position after the
/// object.
fn parse_id2_object(bytes: &[u8], p: usize) -> Result<(Option<u64>, usize), ()> {
    let end = skip_value(bytes, p)?;
    let inner = &bytes[p..end];
    Ok((
        id2_field(inner, b"\"local\"").or_else(|| id2_field(inner, b"\"global\"")),
        end,
    ))
}

/// First `key` occurrence's value inside an `id2` object slice: quoted
/// strings normalize (decimal/hex/hash), integers pass through, anything
/// else (missing key, negative, nested value) is "no id".
fn id2_field(inner: &[u8], key: &[u8]) -> Option<u64> {
    let mut search = inner;
    loop {
        let rel = memchr::memmem::find(search, key)?;
        let mut j = rel + key.len();
        while j < search.len() && search[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= search.len() || search[j] != b':' {
            search = &search[rel + 1..];
            continue;
        }
        j += 1;
        while j < search.len() && search[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= search.len() {
            return None;
        }
        if search[j] == b'"' {
            let (str_inner, _) = parse_quoted_inner(search, j).ok()?;
            return normalize_id_bytes(str_inner);
        }
        if search[j].is_ascii_digit() {
            return parse_num_u64(search, j).ok().map(|(v, _)| v);
        }
        return None;
    }
}

/// tid/pid: integers (wrapping negatives), numeric strings, float truncation
/// — the same coercions as the serde fallback. Wrong types become 0
/// (counted); only structurally broken input bails out.
fn parse_tid_pid_fast(bytes: &[u8], p: usize, stats: &mut ParseStats) -> Result<(u64, usize), ()> {
    if p < bytes.len() && bytes[p] == b'"' {
        let (inner, a) = parse_quoted_inner(bytes, p)?;
        let s = std::str::from_utf8(inner).map_err(|_| ())?;
        stats.malformed += 1;
        return Ok((parse_tid_pid_str(s), a));
    }
    if let Ok((v, a)) = parse_num_i64(bytes, p) {
        return Ok((v, a));
    }
    // Float (1.5 → 1, matching the serde `as u64` cast) or wrong type.
    if let Ok((v, a)) = parse_num_f64(bytes, p) {
        stats.malformed += 1;
        return Ok((v as u64, a));
    }
    let end = skip_value(bytes, p)?;
    stats.malformed += 1;
    Ok((0, end))
}

/// Tolerant per-element parse of a slice known to hold `{…}` elements
/// (separator commas optional). Each element is parsed from its own
/// borrowed slice (events copy out the owned strings they need):
/// successes count `parsed` (plus any serde-fallback field
/// coercions drained into `malformed`), structural failures count
/// `skipped`/`malformed` and the walk continues — one bad event never fails
/// the file.
fn parse_slice_tolerant(bytes: &[u8], s: usize, e: usize) -> (Vec<TraceEvent>, ParseStats) {
    let mut stats = ParseStats::default();
    let mut events = Vec::new();
    let mut i = s;
    while i < e {
        i = skip_ws(bytes, i);
        if i >= e {
            break;
        }
        if bytes[i] == b',' || bytes[i] == b']' {
            i += 1;
            continue;
        }
        if bytes[i] != b'{' {
            // Non-object element (number, string, …): drop, keep going.
            match skip_value(bytes, i) {
                Ok(n) if n > i => {
                    i = n;
                    stats.skipped += 1;
                    stats.malformed += 1;
                }
                _ => break,
            }
            continue;
        }
        match skip_value(bytes, i) {
            Ok(end) if end > i => {
                let slice = &bytes[i..end.min(e)];
                match serde_json::from_slice::<TraceEvent>(slice) {
                    Ok(ev) => {
                        stats.parsed += 1;
                        stats.malformed += take_serde_malformed();
                        events.push(ev);
                    }
                    Err(_) => {
                        take_serde_malformed();
                        stats.skipped += 1;
                        stats.malformed += 1;
                    }
                }
                i = end;
            }
            _ => break,
        }
    }
    (events, stats)
}

/// Event phase: the first byte of a quoted, unescaped string (`0` when the
/// string is empty). Escaped phases bail to the serde fallback, same as
/// `parse_string_intern`. Returns (byte, pos after closing quote).
fn parse_phase(bytes: &[u8], p: usize) -> Result<(u8, usize), ()> {
    let len = bytes.len();
    if p >= len || bytes[p] != b'"' {
        return Err(());
    }
    match memchr::memchr2(b'"', b'\\', &bytes[p + 1..]) {
        Some(rel) => {
            let q = p + 1 + rel;
            if bytes[q] == b'\\' {
                return Err(());
            }
            let v = if q > p + 1 { bytes[p + 1] } else { 0 };
            Ok((v, q + 1))
        }
        None => Err(()),
    }
}

/// JSON number as f64. Fast path: pure integer literals (the overwhelmingly
/// common Chrome form) convert with a single overflow-checked u64→f64 cast
/// — one correctly-rounded conversion, bit-identical to `str::parse`. Other
/// forms (fraction, exponent) fall back to `str::parse` on the scanned
/// slice. Stops at the first char outside the number charset (so ',', '}',
/// ']' and whitespace end the number). Non-numbers error (serde would too).
fn parse_num_f64(bytes: &[u8], p: usize) -> Result<(f64, usize), ()> {
    let len = bytes.len();
    let mut j = p;
    let neg = j < len && bytes[j] == b'-';
    if neg {
        j += 1;
    }
    let dstart = j;
    while j < len && bytes[j].is_ascii_digit() {
        j += 1;
    }
    if j > dstart && !(j < len && matches!(bytes[j], b'.' | b'e' | b'E')) {
        // Pure integer: single correctly-rounded u64→f64 conversion.
        let mut acc: u64 = 0;
        let mut ok = true;
        for &b in &bytes[dstart..j] {
            let d = (b - b'0') as u64;
            if acc > (u64::MAX - d) / 10 {
                ok = false;
                break;
            }
            acc = acc * 10 + d;
        }
        if ok {
            let v = if neg { -(acc as f64) } else { acc as f64 };
            return Ok((v, j));
        }
    }
    // Generic: scan the full number charset and delegate to str::parse
    // (correctly rounded, same as serde_json on real Chrome values).
    let mut i = p;
    while i < len && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
        i += 1;
    }
    if i == p {
        return Err(());
    }
    let s = std::str::from_utf8(&bytes[p..i]).map_err(|_| ())?;
    let v: f64 = s.parse().map_err(|_| ())?;
    Ok((v, i))
}

/// Unsigned integer value: digits only, no float forms (serde would reject
/// floats for u64). Err on overflow or empty.
fn parse_num_u64(bytes: &[u8], p: usize) -> Result<(u64, usize), ()> {
    let len = bytes.len();
    let mut i = p;
    let mut acc: u64 = 0;
    while i < len && bytes[i].is_ascii_digit() {
        let d = (bytes[i] - b'0') as u64;
        if acc > (u64::MAX - d) / 10 {
            return Err(());
        }
        acc = acc * 10 + d;
        i += 1;
    }
    if i == p {
        return Err(());
    }
    if i < len && matches!(bytes[i], b'.' | b'e' | b'E' | b'-' | b'+') {
        return Err(());
    }
    Ok((acc, i))
}

/// Signed integer value: optional `-`, then digits. Used for tid/pid, which
/// Chromium's exporter can emit as negative i32 (Windows ids above 2³¹);
/// they wrap to the same u64 the serde path produces.
fn parse_num_i64(bytes: &[u8], p: usize) -> Result<(u64, usize), ()> {
    let len = bytes.len();
    let mut i = p;
    let neg = if i < len && bytes[i] == b'-' {
        i += 1;
        true
    } else {
        false
    };
    let (v, next) = parse_num_u64(bytes, i)?;
    Ok((
        if neg {
            (v as i64).wrapping_neg() as u64
        } else {
            v
        },
        next,
    ))
}

/// The value starting at p is exactly the literal `null` (not `nullx`).
fn is_literal_null(bytes: &[u8], p: usize) -> bool {
    let len = bytes.len();
    if p + 4 > len || &bytes[p..p + 4] != b"null" {
        return false;
    }
    p + 4 == len
        || matches!(
            bytes[p + 4],
            b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'
        )
}

/// Skip one JSON value starting at p (string, object, array, or number /
/// literal). Returns the position AFTER the value. Err on unterminated.
fn skip_value(bytes: &[u8], p: usize) -> Result<usize, ()> {
    let len = bytes.len();
    if p >= len {
        return Err(());
    }
    match bytes[p] {
        b'"' => {
            let mut q = p + 1;
            loop {
                let Some(rel) = memchr::memchr(b'"', &bytes[q..]) else {
                    return Err(());
                };
                let idx = q + rel;
                let mut run = 0usize;
                let mut k = idx;
                while k > p && bytes[k - 1] == b'\\' {
                    run += 1;
                    k -= 1;
                }
                if run.is_multiple_of(2) {
                    return Ok(idx + 1);
                }
                q = idx + 1;
            }
        }
        b'{' | b'[' => {
            let mut depth = 1i64;
            let mut in_str = false;
            let i = p + 1;
            let mut strings = memchr::memchr3_iter(b'"', b'{', b'[', &bytes[i..]);
            let mut closes = memchr::memchr2_iter(b'}', b']', &bytes[i..]);
            let mut q = strings.next();
            let mut c = closes.next();
            loop {
                let (rel, b) = match (q, c) {
                    (Some(pp), _) if c.is_none_or(|x| pp < x) => {
                        q = strings.next();
                        (pp, bytes[i + pp])
                    }
                    (_, Some(pp)) => {
                        c = closes.next();
                        (pp, bytes[i + pp])
                    }
                    _ => return Err(()),
                };
                let idx = i + rel;
                if in_str {
                    if b == b'"' {
                        let mut run = 0usize;
                        let mut k = idx;
                        while k > p && bytes[k - 1] == b'\\' {
                            run += 1;
                            k -= 1;
                        }
                        if run.is_multiple_of(2) {
                            in_str = false;
                        }
                    }
                    continue;
                }
                match b {
                    b'"' => in_str = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(idx + 1);
                        }
                    }
                    _ => return Err(()),
                }
            }
        }
        _ => {
            let mut i = p;
            while i < len && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                i += 1;
            }
            if i == p {
                return Err(());
            }
            Ok(i)
        }
    }
}

/// Byte ranges of the top-level `traceEvents` array and the `metadata`
/// object, located by a block-counting scan.
struct Layout {
    arr_open: usize,
    arr_close: usize,
    metadata: Option<(usize, usize)>,
}

/// Per-64KB-block JSON structure: brace depth and in-string state at each
/// block start. Both are exact (string-aware): depth deltas come from a
/// byte walk that skips string contents — raw brace counts drift on real
/// traces (e.g. `"origin":"null [internally: ..."` has unbalanced brackets
/// inside a string) — and in-string state from exact (escape-aware) quote
/// parity. The full element walk only runs inside the parse chunks.
struct Blocks {
    depth_at: Vec<i64>,
    in_string_at: Vec<bool>,
    /// Escape-pending state at each block start: inside a string whose last
    /// byte of the previous block was a backslash opening an escape. Without
    /// it, an escaped quote split across a block boundary (`\` last byte of
    /// block k, `"` first byte of block k+1) is misread as closing the
    /// string, and depth drifts for every block after it.
    esc_at: Vec<bool>,
}

const BLOCK: usize = 64 * 1024;

/// Count unescaped quotes in `chunk` for parity. A quote is escaped iff
/// preceded by an odd run of backslashes; a run reaching the chunk start
/// continues into `prev_trail` (backslashes ending the previous block).
fn count_unescaped_quotes(chunk: &[u8], prev_trail: usize) -> (usize, usize) {
    let mut nq = 0usize;
    for q in memchr::memchr_iter(b'"', chunk) {
        let mut run = 0usize;
        let mut kk = q;
        while kk > 0 && chunk[kk - 1] == b'\\' {
            run += 1;
            kk -= 1;
        }
        let escaped = if kk == 0 {
            (run + prev_trail) % 2 == 1
        } else {
            run % 2 == 1
        };
        if !escaped {
            nq += 1;
        }
    }
    let mut trail = 0usize;
    let mut kk = chunk.len();
    while kk > 0 && chunk[kk - 1] == b'\\' {
        trail += 1;
        kk -= 1;
    }
    (nq, trail)
}

fn build_blocks(bytes: &[u8]) -> Blocks {
    let nb = bytes.len().div_ceil(BLOCK);
    let _threads = std::thread::available_parallelism()
        .map(|t| t.get())
        .unwrap_or(1)
        .max(1);
    let chunk_count = 1;

    // Phase 1: per-block quote parity is independent — count in parallel.
    let mut quote_parity = vec![false; nb];
    let mut trails = vec![0usize; nb];
    if chunk_count > 1 {
        let ptr_q: usize = quote_parity.as_mut_ptr() as usize;
        let ptr_t: usize = trails.as_mut_ptr() as usize;
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunk_count);
            for t in 0..chunk_count {
                let lo = nb * t / chunk_count;
                let hi = nb * (t + 1) / chunk_count;
                let bstart = lo * BLOCK;
                let bend = (hi * BLOCK).min(bytes.len());
                let seg = &bytes[bstart..bend];
                // Seed the backslash run from the preceding byte range so a
                // `\"` split across a thread-chunk boundary counts correctly.
                let mut prev_trail = 0usize;
                let mut kk = bstart;
                while kk > 0 && bytes[kk - 1] == b'\\' {
                    prev_trail += 1;
                    kk -= 1;
                }
                handles.push(s.spawn(move || {
                    let ptr_q = ptr_q as *mut bool;
                    let ptr_t = ptr_t as *mut usize;
                    let mut prev_trail = prev_trail;
                    for (k, chunk) in seg.chunks(BLOCK).enumerate() {
                        let (nq, trail) = count_unescaped_quotes(chunk, prev_trail);
                        unsafe {
                            *ptr_q.add(lo + k) = nq % 2 == 1;
                        }
                        unsafe {
                            *ptr_t.add(lo + k) = trail;
                        }
                        prev_trail = trail;
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });
    } else {
        let mut prev_trail = 0usize;
        for (k, chunk) in bytes.chunks(BLOCK).enumerate() {
            let (nq, trail) = count_unescaped_quotes(chunk, prev_trail);
            quote_parity[k] = nq % 2 == 1;
            trails[k] = trail;
            prev_trail = trail;
        }
    }

    // Prefix: exact in-string state at each block start.
    let mut in_string_at = Vec::with_capacity(nb + 1);
    let mut s = false;
    in_string_at.push(false);
    for par in &quote_parity {
        s ^= *par;
        in_string_at.push(s);
    }

    // Escape-pending state at each block start: in a string whose previous
    // block ends with an odd trailing backslash run (that backslash escapes
    // this block's first byte).
    let mut esc_at = vec![false; nb];
    for k in 1..nb {
        esc_at[k] = in_string_at[k] && trails[k - 1] % 2 == 1;
    }

    // Phase 2: exact per-block depth deltas via a string-aware walk starting
    // from each block's known in-string and escape state. Independent per
    // block, so parallel; the net delta doesn't depend on the starting depth.
    let mut deltas = vec![0i64; nb];
    if chunk_count > 1 {
        let ptr_d: usize = deltas.as_mut_ptr() as usize;
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunk_count);
            for t in 0..chunk_count {
                let lo = nb * t / chunk_count;
                let hi = nb * (t + 1) / chunk_count;
                let bstart = lo * BLOCK;
                let bend = (hi * BLOCK).min(bytes.len());
                let seg = &bytes[bstart..bend];
                let states = &in_string_at[lo..hi];
                let escs = &esc_at[lo..hi];
                handles.push(s.spawn(move || {
                    let ptr_d = ptr_d as *mut i64;
                    for (k, chunk) in seg.chunks(BLOCK).enumerate() {
                        let mut dd: i64 = 0;
                        let mut st = states[k];
                        let mut esc = escs[k];
                        walk_range(chunk, &mut dd, &mut st, &mut esc);
                        unsafe {
                            *ptr_d.add(lo + k) = dd;
                        }
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });
    } else {
        for (k, chunk) in bytes.chunks(BLOCK).enumerate() {
            let mut dd: i64 = 0;
            let mut st = in_string_at[k];
            let mut esc = esc_at[k];
            walk_range(chunk, &mut dd, &mut st, &mut esc);
            deltas[k] = dd;
        }
    }

    // Sequential prefix sum: exact depth at each block start.
    let mut depth_at = Vec::with_capacity(nb + 1);
    let mut d = 0i64;
    depth_at.push(0);
    for dd in &deltas {
        d += *dd;
        depth_at.push(d);
    }
    Blocks {
        depth_at,
        in_string_at,
        esc_at,
    }
}

/// Exact (string-aware) depth and in-string state at byte `pos`, by walking
/// only the (≤64KB) block prefix. `d`/`s` are the block-start state.
fn state_at(bytes: &[u8], blocks: &Blocks, pos: usize) -> (i64, bool) {
    let block = pos / BLOCK;
    let mut d = blocks.depth_at[block];
    let mut s = blocks.in_string_at[block];
    let mut esc = blocks.esc_at[block];
    walk_range(&bytes[block * BLOCK..pos], &mut d, &mut s, &mut esc);
    (d, s)
}

/// String-aware walk over a byte range, updating depth and in-string state.
/// `esc` carries the escape-pending state in/out (a backslash whose escaped
/// byte may lie past the end of the range).
fn walk_range(r: &[u8], d: &mut i64, s: &mut bool, esc: &mut bool) {
    let mut in_str = *s;
    let mut e = *esc;
    for &b in r {
        if in_str {
            if e {
                e = false;
            } else if b == b'\\' {
                e = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' | b'[' => *d += 1,
                b'}' | b']' => *d -= 1,
                _ => {}
            }
        }
    }
    *s = in_str;
    *esc = e;
}

/// Find the `[` (traceEvents) or `{` (metadata) opening the value of a
/// top-level member key, at depth 1. Uses SIMD substring search + local
/// probes; returns (position, byte-of-opener).
fn find_top_level_open(bytes: &[u8], blocks: &Blocks, needle: &[u8]) -> Option<(usize, u8)> {
    for m in memchr::memmem::find_iter(bytes, needle) {
        if m == 0 || bytes[m - 1] != b'"' {
            continue;
        }
        let after = m + needle.len();
        if after >= bytes.len() || bytes[after] != b'"' {
            continue;
        }
        let mut j = after + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() {
            continue;
        }
        let opener = bytes[j];
        if opener != b'[' && opener != b'{' {
            continue;
        }
        let (d, _) = state_at(bytes, blocks, j);
        if d == 1 {
            return Some((j, opener));
        }
    }
    None
}

/// Block-accelerated layout scan: array bounds via targeted probes, array
/// close via the last block at array depth, metadata via key probe.
/// Falls back to `None` when the structure isn't recognized.
fn scan_layout(bytes: &[u8]) -> Option<(Layout, Blocks)> {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let blocks = build_blocks(bytes);
    let (arr_open, opener) = find_top_level_open(bytes, &blocks, b"traceEvents")?;
    // `traceEvents` must open an array. A non-array value (object, null,
    // number, …) is genuinely non-trace input: fall through to the tolerant
    // path, which errors with a clear "missing top-level traceEvents array"
    // message instead of mis-parsing a member object as events.
    if opener != b'[' {
        return None;
    }
    if debug {
        eprintln!("  [scan] arr_open at {}", arr_open);
    }
    if arr_open + 1 >= bytes.len() {
        return None;
    }
    // Array close `]`: walk blocks from the end of the file backwards. The
    // block containing the `]` starts somewhere inside the last element
    // (depth ≥ 2) and the walk reaches depth 2 exactly at the array close.
    let nb = blocks.depth_at.len() - 1;
    let mut arr_close = None;
    for k in (0..nb).rev().take(16) {
        let bs = k * BLOCK;
        let be = (bs + BLOCK).min(bytes.len());
        let mut d = blocks.depth_at[k];
        let mut s = blocks.in_string_at[k];
        let mut esc = blocks.esc_at[k];
        let mut i = bs;
        while i < be {
            let b = bytes[i];
            if s {
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    s = false;
                }
            } else {
                match b {
                    b'"' => s = true,
                    b'{' | b'[' => d += 1,
                    b'}' | b']' => {
                        if b == b']' && d == 2 {
                            arr_close = Some(i);
                            break;
                        }
                        d -= 1;
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        if arr_close.is_some() {
            break;
        }
    }
    if debug {
        eprintln!("  [scan] blocks={}, arr_close={:?}", nb, arr_close);
    }
    let arr_close = arr_close?;
    if arr_close <= arr_open {
        return None;
    }

    let metadata = find_top_level_open(bytes, &blocks, b"metadata").and_then(|(pos, opener)| {
        if opener != b'{' {
            return None;
        }
        // Closing brace of the metadata object: walk from its block.
        let block = pos / BLOCK;
        let bs = block * BLOCK;
        let mut d = blocks.depth_at[block];
        let mut s = blocks.in_string_at[block];
        let mut esc = blocks.esc_at[block];
        let mut end = None;
        for (i, &b) in bytes[bs..].iter().enumerate() {
            let idx = bs + i;
            if s {
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    s = false;
                }
            } else {
                match b {
                    b'"' => s = true,
                    b'{' | b'[' => d += 1,
                    b'}' | b']' => {
                        if b == b'}' && d == 2 {
                            end = Some(idx + 1);
                            break;
                        }
                        d -= 1;
                    }
                    _ => {}
                }
            }
        }
        end.map(|e| (pos, e))
    });

    Some((
        Layout {
            arr_open,
            arr_close,
            metadata,
        },
        blocks,
    ))
}

fn parse_thread_count(n: &usize) -> usize {
    std::env::var("CHPERF_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|t| t.get())
                .unwrap_or(1)
        })
        .min(*n)
        .max(1)
}

/// One thread's parse result: events plus this chunk's parse counters.
type ChunkResult = Result<(Vec<TraceEvent>, ParseStats), String>;

/// Whole parallel parse result: events, top-level metadata, parse counters.
type ParallelResult =
    Result<(Vec<TraceEvent>, Option<TraceMetadata>, ParseStats), Box<dyn std::error::Error>>;

/// Parse the `traceEvents` array in parallel. Each thread walks its own byte
/// range (starting from a block boundary with known exact state), finds the
/// element ranges, then parses its slice directly from the buffer,
/// copying the small owned strings each event needs. Chunks never mutate
/// the buffer: the fast tokenizer runs on the pristine bytes, and any slice
/// it bails on goes through a tolerant per-element serde parse that skips
/// structurally broken events (counted in `ParseStats`) instead of failing
/// the chunk. The buffer is freed when `parse_trace` returns; events own
/// their data.
fn parse_parallel(
    bytes: &[u8],
    layout: &Layout,
    blocks: &Blocks,
    threads: usize,
) -> ParallelResult {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let threads = threads.max(1);
    let span = layout.arr_close - layout.arr_open;

    let t_scope = std::time::Instant::now();

    // Per-unit: (unit index, chunk parse result).
    let results: Vec<(usize, ChunkResult)> = if threads <= 1 {
        let r = chunk_work(bytes, blocks, layout.arr_open, layout.arr_close);
        vec![(0, r)]
    } else {
        let unit = (span / (threads * 8)).clamp(BLOCK, 4 * 1024 * 1024);
        let n_units = span.div_ceil(unit).max(1);
        if n_units <= 1 {
            let r = chunk_work(bytes, blocks, layout.arr_open, layout.arr_close);
            vec![(0, r)]
        } else {
            use std::sync::atomic::{AtomicUsize, Ordering};
            // More workers than fast cores hurts: on heterogeneous chips
            // (4P+6E here) threads spilling onto efficiency cores run ~3x
            // slower and gate wall time (measured: 10 workers ~148ms vs 4
            // workers ~74ms on a 120MB trace). Cap workers; the many small
            // units still balance the remaining threads via work-stealing.
            let workers = threads.min(n_units).min(6);
            let cursor = AtomicUsize::new(0);
            let out: std::sync::Mutex<Vec<(usize, ChunkResult)>> =
                std::sync::Mutex::new(Vec::with_capacity(n_units));
            let total_len = bytes.len();
            let arr_open = layout.arr_open;
            let arr_close = layout.arr_close;
            std::thread::scope(|s| {
                let ptr: usize = bytes.as_ptr() as usize;
                let cursor_ref = &cursor;
                let out_ref = &out;
                for _ in 0..workers {
                    s.spawn(move || {
                        loop {
                            let idx = cursor_ref.fetch_add(1, Ordering::Relaxed);
                            if idx >= n_units {
                                break;
                            }
                            let from = arr_open + idx * unit;
                            let to = (from + unit).min(arr_close);
                            let t0 = std::time::Instant::now();
                            // Units are disjoint read-only slices sharing the
                            // buffer through a raw pointer (scope-confined).
                            let bytes =
                                unsafe { std::slice::from_raw_parts(ptr as *const u8, total_len) };
                            let r = chunk_work(bytes, blocks, from, to);
                            if debug {
                                eprintln!(
                                    "  [parse] chunk {}..{}: {:.1}ms, {} events",
                                    from,
                                    to,
                                    t0.elapsed().as_secs_f64() * 1000.0,
                                    r.as_ref().map(|(v, _)| v.len()).unwrap_or(0)
                                );
                            }
                            out_ref.lock().unwrap().push((idx, r));
                        }
                    });
                }
            });
            out.into_inner().unwrap()
        }
    };

    // Sort units into file order before merging.
    let mut results = results;
    results.sort_by_key(|(idx, _)| *idx);

    // Unwrap chunk results and fold their stats.
    let mut parsed: Vec<Vec<TraceEvent>> = Vec::with_capacity(results.len());
    let mut stats = ParseStats::default();
    for (_, r) in results {
        match r {
            Ok((events, chunk_stats)) => {
                stats.parsed += chunk_stats.parsed;
                stats.skipped += chunk_stats.skipped;
                stats.malformed += chunk_stats.malformed;
                parsed.push(events);
            }
            Err(e) => {
                return Err(Box::<dyn std::error::Error>::from(format!(
                    "chunk parse failed: {}",
                    e
                )));
            }
        }
    }
    if debug {
        eprintln!(
            "  [parse] scope: {:.1}ms",
            t_scope.elapsed().as_secs_f64() * 1000.0
        );
    }

    let t_merge = std::time::Instant::now();
    let total: usize = parsed.iter().map(|p| p.len()).sum();
    // Move (not clone!) chunk elements into one contiguous Vec. `append`
    // transfers ownership with a shallow memcpy and never drops the source
    // elements — deep-cloning 1.35M events would be ~10x slower.
    let mut events: Vec<TraceEvent> = Vec::with_capacity(total);
    for mut p in parsed {
        events.append(&mut p);
    }
    if debug {
        eprintln!(
            "  [parse] merge: {:.1}ms",
            t_merge.elapsed().as_secs_f64() * 1000.0
        );
    }

    let metadata = match layout.metadata {
        Some((s, e)) => serde_json::from_slice::<TraceMetadata>(&bytes[s..e]).ok(),
        None => None,
    };
    Ok((events, metadata, stats))
}

/// Walk `[from_block_start, to)` (extended to finish the element containing
/// `to`), collecting the owned element range, and parsing the slice. The
/// walk visits only structural positions (memchr-accelerated), not every
/// byte. The buffer is never mutated; owned ranges are disjoint per chunk.
fn chunk_work(
    bytes: &[u8],
    blocks: &Blocks,
    from: usize,
    to: usize,
) -> Result<(Vec<TraceEvent>, ParseStats), String> {
    let block = from / BLOCK;
    let start = block * BLOCK;
    let mut d = blocks.depth_at[block];
    let mut in_string = blocks.in_string_at[block];

    let mut first_start: Option<usize> = None;
    let mut last_end: usize = 0;
    let mut elem_start: Option<usize> = None;
    let mut finished_last = false; // the element containing `to` was completed

    // Walk to the end of the buffer so the element containing `to` always
    // reaches its closing `}` (elements can exceed 64KB). The walk still
    // stops at the first element boundary at/after `to` (or at EOF on
    // truncated input), and the memchr iterators are lazy, so this costs
    // nothing for normal chunks.
    let end = bytes.len();
    let mut strings = memchr::memchr3_iter(b'"', b'{', b'[', &bytes[start..end]);
    let mut closes = memchr::memchr2_iter(b'}', b']', &bytes[start..end]);
    let mut q = strings.next();
    let mut c = closes.next();
    loop {
        let (rel, b) = match (q, c) {
            (Some(p), _) if c.is_none_or(|x| p < x) => {
                q = strings.next();
                (p, bytes[start + p])
            }
            (_, Some(p)) => {
                c = closes.next();
                (p, bytes[start + p])
            }
            _ => break,
        };
        let i = start + rel;
        if in_string {
            if b == b'"' {
                let mut run = 0usize;
                let mut k = i;
                while k > start && bytes[k - 1] == b'\\' {
                    run += 1;
                    k -= 1;
                }
                // A run reaching the block start continues the previous
                // block's trailing backslash run; `esc_at` encodes its parity.
                let escaped = if k == start {
                    (run + blocks.esc_at[block] as usize) % 2 == 1
                } else {
                    run % 2 == 1
                };
                if !escaped {
                    in_string = false;
                }
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                if b == b'{' && d == 2 && i >= from {
                    elem_start = Some(i);
                }
                d += 1;
            }
            b'}' | b']' => {
                if b == b'}' && d == 3 {
                    if let Some(e) = elem_start.take() {
                        // A chunk owns elements whose start is inside
                        // [from, to). An element starting at/after `to`
                        // belongs to the next chunk — stopping here keeps
                        // its separator comma outside this slice.
                        if e >= to {
                            break;
                        }
                        if first_start.is_none() {
                            first_start = Some(e);
                        }
                        last_end = i + 1;
                        // Stop at the first element boundary at/after `to`
                        // even when the element *containing* `to` started
                        // before `from` (it belongs to the previous chunk):
                        // otherwise the walk grabs the next element, which
                        // the following chunk also owns — boundary elements
                        // get parsed twice.
                        if i >= to {
                            finished_last = true;
                        }
                    } else if i >= to {
                        // Element started before `from` (previous chunk's)
                        // but spans at least to `to`: stop here.
                        finished_last = true;
                    }
                }
                d -= 1;
            }
            _ => {}
        }
        if finished_last {
            break;
        }
    }

    let (s, e) = match (first_start, last_end) {
        (Some(s), e) if e > s => (s, e),
        _ => return Ok((Vec::new(), ParseStats::default())),
    };

    // Fast path: hand-rolled tokenizer on the pristine bytes. Field
    // coercions are counted in a temp (discarded when the fast path bails).
    let t_fast = std::time::Instant::now();
    let mut fast_stats = ParseStats::default();
    if let Ok(events) = parse_events_fast(bytes, s, e, &mut fast_stats) {
        if std::env::var("CHPERF_DEBUG").is_ok() {
            eprintln!(
                "  [parse] fast {}..{}: {:.1}ms, {} events",
                s,
                e,
                t_fast.elapsed().as_secs_f64() * 1000.0,
                events.len()
            );
        }
        return Ok((events, fast_stats));
    }

    // Tolerant per-element serde parse on the pristine slice (no comma
    // mutation): structurally broken events are skipped and counted, the
    // rest are kept — a single bad event never fails the chunk.
    if std::env::var("CHPERF_DEBUG").is_ok() {
        eprintln!("  [parse] fast {}..{} failed, tolerant fallback", s, e);
    }
    Ok(parse_slice_tolerant(bytes, s, e))
}

/// Test-only counter for how many `parse_trace` calls were served by the
/// fast single-pass tokenizer (no block scan, no serde). Lets throughput /
/// fast-path tests assert the hot path was taken without parsing logs.
#[cfg(test)]
pub(crate) static FAST_SINGLE_PASS_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn parse_trace(path: &Path) -> Result<TraceFile, Box<dyn std::error::Error>> {
    // Drop any stale serde-fallback counts (unit tests deserialize events
    // directly without draining).
    take_serde_malformed();
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    // Decompress (if .gz) and parse from an in-memory slice. This is far
    // faster than streaming serde_json through the gzip decoder: zlib-rs
    // inflates at ~1.5GB/s, and `from_slice` skips reader indirection.
    let mut bytes = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        // Preallocate from the gzip footer ISIZE (last 4 bytes, LE u32):
        // a hint for the uncompressed size. Never fail because of it.
        let hint: Option<usize> = (|| {
            use std::io::Seek;
            let len = file.metadata().ok()?.len();
            if len < 4 {
                return None;
            }
            file.seek(std::io::SeekFrom::End(-4)).ok()?;
            let mut footer = [0u8; 4];
            std::io::Read::read_exact(&mut file, &mut footer).ok()?;
            file.seek(std::io::SeekFrom::Start(0)).ok()?;
            let isize = u32::from_le_bytes(footer) as usize;
            if isize == 0 {
                return None;
            }
            Some(isize.min(1 << 30))
        })();
        let mut out = Vec::with_capacity(hint.map(|h| h + 32).unwrap_or(0));
        flate2::read::GzDecoder::new(file)
            .read_to_end(&mut out)
            .map_err(|e| format!("{}: {}", path.display(), e))?;
        out
    } else {
        let mut out = Vec::with_capacity(file.metadata().map(|m| m.len() as usize).unwrap_or(0));
        file.take(u64::MAX)
            .read_to_end(&mut out)
            .map_err(|e| format!("{}: {}", path.display(), e))?;
        out
    };
    // Strip a UTF-8 BOM so every downstream path (fast single-pass, block
    // scan, serde) sees the same `{`-first buffer. Without this the fast
    // path bails (first byte isn't `{`) and relies on the slower fallbacks.
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
    }
    // LIFETIME NOTE — no leaks on this path by design.
    //
    // Every `TraceEvent` owns its strings: the fast tokenizer copies each
    // event's `name`/`cat`/`args` slices into owned `String`/`Box<str>`
    // values, and the serde fallback does the same from its owned `Value`s.
    // Nothing borrows from the file buffer, so the buffer below is an
    // ordinary local and is freed when this function returns. Repeated
    // `parse_trace` calls retain no memory.
    //
    // (History: `args` used to be `Option<&'static str>` borrowed zero-copy
    // from a `Box::leak`ed whole-file buffer, plus a leaked name/category
    // interner and per-event leaks on the serde path — one file-sized leak
    // per call. The owned model costs one small copy per event string;
    // see the throughput test below for the measured budget.)

    // Fast path: single-pass top-level + traceEvents parse without
    // pre-scanning 64KB blocks across the buffer. Events own their
    // strings, so the buffer below is freed when parsing finishes.
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let mut trace: Option<TraceFile> = None;
    let mut scan_none = false;
    let t_fast = std::time::Instant::now();
    if let Ok(tf) = parse_trace_fast(&bytes) {
        #[cfg(test)]
        FAST_SINGLE_PASS_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if debug {
            eprintln!(
                "  [parse] fast single-pass {:.1}ms: {} events",
                t_fast.elapsed().as_secs_f64() * 1000.0,
                tf.trace_events.len()
            );
        }
        trace = Some(tf);
    } else {
        let t_scan = std::time::Instant::now();
        if let Some((layout, blocks)) = scan_layout(&bytes) {
            if debug {
                eprintln!(
                    "  [parse] scan {:.1}ms: array {}..{}, metadata={}",
                    t_scan.elapsed().as_secs_f64() * 1000.0,
                    layout.arr_open,
                    layout.arr_close,
                    layout.metadata.is_some()
                );
            }
            let threads = parse_thread_count(&(layout.arr_close - layout.arr_open));
            let t_par = std::time::Instant::now();
            if debug {
                eprintln!("  [parse] threads: {}", threads);
            }
            match parse_parallel(&bytes, &layout, &blocks, threads) {
                Ok((trace_events, metadata, stats)) => {
                    if debug {
                        eprintln!(
                            "  [parse] parallel {:.1}ms: {} events",
                            t_par.elapsed().as_secs_f64() * 1000.0,
                            trace_events.len()
                        );
                    }
                    trace = Some(TraceFile {
                        trace_events,
                        metadata,
                        stats,
                    });
                }
                Err(e) if debug => eprintln!("  [parse] parallel failed: {}", e),
                _ => {}
            }
        } else {
            scan_none = true;
            if debug {
                eprintln!("  [parse] scan returned None, falling back");
            }
        }
    }
    // Fallback: whole-buffer parse. First a direct decode (covers unusual but
    // valid layouts); if any single event breaks it, a tolerant per-element
    // pass keeps the good events and counts the bad ones — only a missing
    // top-level "traceEvents" still fails the file.
    let mut trace = match trace {
        Some(t) => t,
        None => {
            if scan_none && memchr::memmem::find(&bytes, b"traceEvents").is_none() {
                return Err(format!(
                    "{}: not a Chrome trace (missing top-level \"traceEvents\" array)",
                    path.display()
                )
                .into());
            }
            match serde_json::from_slice::<TraceFile>(&bytes) {
                Ok(mut t) => {
                    t.stats.parsed = t.trace_events.len();
                    t.stats.malformed += take_serde_malformed();
                    t
                }
                Err(_) => parse_trace_tolerant(&bytes, path)?,
            }
        }
    };
    if debug {
        eprintln!("  [parse] trace ready: {} events", trace.trace_events.len());
    }

    // Pair old-exporter B/E events into synthetic X completes. Runs once
    // here so every parse path (fast single-pass, parallel, serde and
    // tolerant fallbacks) funnels through it. Originals are kept (see the
    // invariant on `normalize_be_pairs`); `stats.parsed` still counts raw
    // events only. B/E-heavy traces analyzed before this step saw zero
    // durations everywhere, since all analysis filters `ph == b'X'`.
    super::normalize_be_pairs(&mut trace.trace_events);

    // An empty trace has no time base (downstream would compute -inf), so it
    // is an error rather than a valid empty TraceFile.
    if trace.trace_events.is_empty() {
        return Err(format!("{}: trace has no events", path.display()).into());
    }

    // Extract page URL and frames from TracingStartedInBrowser events.
    // Frames accumulate across ALL such events, merged by frame id (later
    // events refresh url/name/process_id); page_url is the outermost main
    // frame's URL, with a legacy `isMainFrame` fallback and a final
    // first-non-blank fallback when no frame carries a main marker.
    {
        let meta = trace.metadata.get_or_insert(TraceMetadata {
            cpu_throttling: None,
            source: None,
            start_time: None,
            network_throttling: None,
            hardware_concurrency: None,
            host_dpr: None,
            page_url: None,
            frames: Vec::new(),
        });
        if meta.page_url.is_none() || meta.frames.is_empty() {
            let mut index: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for e in &trace.trace_events {
                if e.name != "TracingStartedInBrowser" {
                    continue;
                }
                let Some(args) = e.args_value() else {
                    continue;
                };
                let Some(frames) = args
                    .get("data")
                    .and_then(|d| d.get("frames"))
                    .and_then(|f| f.as_array())
                else {
                    continue;
                };
                for frame in frames {
                    let Some(id) = frame.get("frame").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if id.is_empty() {
                        continue;
                    }
                    let parent_id = frame
                        .get("parent")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let process_id = frame.get("processId").map(frame_process_id).unwrap_or(0);
                    let url = frame
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = frame
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let is_main_frame = [
                        "isInPrimaryMainFrame",
                        "isOutermostMainFrame",
                        "isMainFrame",
                    ]
                    .iter()
                    .any(|k| frame.get(k).and_then(|v| v.as_bool()).unwrap_or(false));
                    if let Some(&k) = index.get(id) {
                        let f = &mut meta.frames[k];
                        if !url.is_empty() {
                            f.url = url;
                        }
                        if !name.is_empty() {
                            f.name = name;
                        }
                        if process_id != 0 {
                            f.process_id = process_id;
                        }
                        if parent_id.is_some() {
                            f.parent_id = parent_id;
                        }
                        f.is_main_frame |= is_main_frame;
                    } else {
                        index.insert(id.to_string(), meta.frames.len());
                        meta.frames.push(FrameInfo {
                            id: id.to_string(),
                            parent_id,
                            process_id,
                            url,
                            name,
                            is_main_frame,
                        });
                    }
                }
            }
            if meta.page_url.is_none() && !meta.frames.is_empty() {
                let ids: std::collections::HashSet<&str> =
                    meta.frames.iter().map(|f| f.id.as_str()).collect();
                // Outermost main frame: a main frame whose parent is unknown.
                let mut chosen = meta.frames.iter().position(|f| {
                    f.is_main_frame && f.parent_id.as_deref().is_none_or(|p| !ids.contains(p))
                });
                // Any main frame (legacy markers included).
                if chosen.is_none() {
                    chosen = meta.frames.iter().position(|f| f.is_main_frame);
                }
                if let Some(k) = chosen {
                    let url = meta.frames[k].url.clone();
                    if !url.is_empty() && url != "about:blank" {
                        meta.page_url = Some(url);
                    }
                }
                // No main marker at all: first non-blank URL.
                if meta.page_url.is_none()
                    && let Some(url) = meta
                        .frames
                        .iter()
                        .find(|f| !f.url.is_empty() && f.url != "about:blank")
                        .map(|f| f.url.clone())
                {
                    meta.page_url = Some(url);
                }
            }
        }
    }

    Ok(trace)
}

/// Whole-buffer tolerant fallback: decode the top-level value, then parse
/// `traceEvents` element by element. Good events are kept, broken ones are
/// skipped and counted (`skipped`/`malformed`) — only a missing/non-array
/// `traceEvents` still fails the file (with path context).
///
/// When the root value itself won't decode (truncated write, trailing
/// garbage), fall back to `parse_trace_truncated`, which salvages the
/// complete-event prefix instead of failing the whole file.
fn parse_trace_tolerant(
    bytes: &[u8],
    path: &Path,
) -> Result<TraceFile, Box<dyn std::error::Error>> {
    take_serde_malformed();
    let root: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return parse_trace_truncated(bytes, path),
    };
    let not_trace = || {
        Box::<dyn std::error::Error>::from(format!(
            "{}: not a Chrome trace (missing top-level \"traceEvents\" array)",
            path.display()
        ))
    };
    // Take ownership of the array (no per-element `Value::clone`): each item
    // is moved into the per-event deserializer exactly once.
    let mut map = match root {
        serde_json::Value::Object(m) => m,
        _ => return Err(not_trace()),
    };
    let arr = match map.remove("traceEvents") {
        Some(serde_json::Value::Array(a)) => a,
        _ => return Err(not_trace()),
    };
    let mut trace_events = Vec::with_capacity(arr.len());
    let mut stats = ParseStats::default();
    for item in arr {
        match serde_json::from_value::<TraceEvent>(item) {
            Ok(ev) => {
                stats.parsed += 1;
                stats.malformed += take_serde_malformed();
                trace_events.push(ev);
            }
            Err(_) => {
                take_serde_malformed();
                stats.skipped += 1;
                stats.malformed += 1;
            }
        }
    }
    let metadata = map
        .remove("metadata")
        .and_then(|m| serde_json::from_value::<TraceMetadata>(m).ok());
    Ok(TraceFile {
        trace_events,
        metadata,
        stats,
    })
}

/// Salvage path for input whose root JSON won't decode (truncated mid-event
/// write, trailing garbage after the array). Locates the top-level
/// `"traceEvents": [` opener with a quote/colon structural probe (first
/// plausible candidate — the header is intact in truncation scenarios) and
/// parses the complete-event prefix with the tolerant per-element walk:
/// unterminated tail input simply stops the walk, keeping what parsed.
/// Still errors (clearly) when no array opener exists — that input was never
/// a trace — and yields zero events only for the caller to report "no
/// events".
fn parse_trace_truncated(
    bytes: &[u8],
    path: &Path,
) -> Result<TraceFile, Box<dyn std::error::Error>> {
    take_serde_malformed();
    let open = find_trace_events_open(bytes).ok_or_else(|| {
        Box::<dyn std::error::Error>::from(format!(
            "{}: not a Chrome trace (missing top-level \"traceEvents\" array)",
            path.display()
        ))
    })?;
    let (trace_events, stats) = parse_slice_tolerant(bytes, open + 1, bytes.len());
    Ok(TraceFile {
        trace_events,
        metadata: None,
        stats,
    })
}

/// Byte offset of the `[` opening the top-level `traceEvents` value: the
/// first `"traceEvents"` occurrence followed by `"`, `:`, `[` whose
/// preceding non-whitespace byte is `{` or `,` (a member position, not
/// string content). `None` when no such opener exists.
fn find_trace_events_open(bytes: &[u8]) -> Option<usize> {
    for m in memchr::memmem::find_iter(bytes, b"traceEvents") {
        if m == 0 || bytes[m - 1] != b'"' {
            continue;
        }
        let after = m + b"traceEvents".len();
        if after >= bytes.len() || bytes[after] != b'"' {
            continue;
        }
        let mut j = after + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'[' {
            // Byte before the key (skipping whitespace) must be `{` or `,`
            // — a member position, not string content.
            let mut k = m - 1;
            while k > 0 {
                k -= 1;
                if !bytes[k].is_ascii_whitespace() {
                    break;
                }
            }
            if bytes[k] == b'{' || bytes[k] == b',' {
                return Some(j);
            }
        }
    }
    None
}

/// `processId`: integer or numeric string, else 0.
fn frame_process_id(v: &serde_json::Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An escaped quote split across a 64KB block boundary (`\` last byte of
    /// block 0, `"` first byte of block 1) used to reset the walk's escape
    /// state at the boundary: the quote was misread as closing the string,
    /// depth drifted, and every event after a `]]]`-style trap inside that
    /// string was silently dropped. The block-start escape state must carry.
    #[test]
    fn escape_split_across_block_boundary_does_not_truncate() {
        let mut json =
            String::from(r#"{"traceEvents":[{"name":"trap","ph":"X","ts":1,"dur":1,"args":{"s":""#);
        // 16 (prefix) + 52 (trap event head) puts the next byte at 68.
        json.push_str(&"x".repeat(65_467));
        // Backslash now at byte 65535 (last of block 0), quote at 65536.
        json.push_str("\\\"");
        json.push_str("y]]]y");
        // Close the string with a plain quote, close args + event.
        json.push_str("\",\"z\":1}}");
        for i in 0..300 {
            json.push_str(&format!(
                r#",{{"name":"tail{i}","ph":"X","ts":{},"dur":1}}"#,
                10 + i
            ));
        }
        json.push_str("]}");

        // Pin the boundary condition: the split escape straddles block 0/1.
        let b = json.as_bytes();
        assert_eq!(b[65_535], b'\\');
        assert_eq!(b[65_536], b'"');
        assert_eq!(b[65_537], b'y');

        let path =
            std::env::temp_dir().join(format!("chperf-esc-split-{}.json", std::process::id()));
        std::fs::write(&path, json).unwrap();
        let trace = parse_trace(&path).expect("parse must succeed");
        std::fs::remove_file(&path).ok();
        // 1 trap + 300 tail events; the old walk saw 763/1063-style loss.
        assert_eq!(trace.trace_events.len(), 301);
        assert_eq!(trace.trace_events[0].name, "trap");
        assert_eq!(trace.trace_events[300].name, "tail299");
    }

    /// The fast tokenizer must capture async `id`s (integer and string) for
    /// s/f pairing, leave `has_id` false when the field is absent or
    /// negative, and not bail the chunk on either.
    #[test]
    fn fast_path_parses_async_ids_leniently() {
        let json = serde_json::json!({
            "traceEvents": [
                {"name": "AnimationFrame", "ph": "s", "ts": 1_000.0, "pid": 1, "tid": 1, "id": 42},
                {"name": "AnimationFrame", "ph": "f", "ts": 5_000.0, "pid": 1, "tid": 1, "id": 42},
                {"name": "AnimationFrame", "ph": "s", "ts": 2_000.0, "pid": 1, "tid": 1},
                {"name": "AnimationFrame", "ph": "s", "ts": 3_000.0, "pid": 1, "tid": 1, "id": "x7"},
                {"name": "AnimationFrame", "ph": "f", "ts": 3_500.0, "pid": 1, "tid": 1, "id": "x7"},
                {"name": "AnimationFrame", "ph": "s", "ts": 4_000.0, "pid": 1, "tid": 1, "id": -3}
            ]
        });
        let path =
            std::env::temp_dir().join(format!("chperf-async-id-{}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let evs = &trace.trace_events;
        assert_eq!(evs.len(), 6);
        assert_eq!(evs[0].id, 42);
        assert!(evs[0].has_id);
        assert_eq!(evs[1].id, 42);
        assert!(evs[1].has_id);
        // Absent id: present event, no id.
        assert!(!evs[2].has_id);
        // String ids hash to a stable u64, so s/f with the same string pair.
        assert!(evs[3].has_id);
        assert!(evs[4].has_id);
        assert_eq!(evs[3].id, evs[4].id);
        // Negative ids stay unpaired.
        assert!(!evs[5].has_id);
    }

    /// Old-exporter B/E trace: synthetic X events make summary/jank see
    /// nonzero durations; `stats.parsed` still counts raw events; lowercase
    /// frame b/e pairs still work in frames_section.
    #[test]
    fn old_exporter_be_trace_analyzes_with_synthetic_x() {
        let body = r#"{"traceEvents":[
            {"name":"thread_name","ph":"M","ts":0,"pid":1,"tid":1,"args":{"name":"CrRendererMain"}},
            {"name":"RunTask","ph":"B","ts":1000,"pid":1,"tid":1},
            {"name":"FunctionCall","ph":"B","ts":2000,"pid":1,"tid":1,"args":{"data":{"functionName":"f"}}},
            {"name":"FunctionCall","ph":"E","ts":22000,"pid":1,"tid":1},
            {"name":"Layout","ph":"B","ts":30000,"pid":1,"tid":1},
            {"name":"Layout","ph":"E","ts":45000,"pid":1,"tid":1},
            {"name":"RunTask","ph":"E","ts":61000,"pid":1,"tid":1},
            {"name":"DrawFrame","ph":"b","ts":1000,"pid":2,"tid":7},
            {"name":"DrawFrame","ph":"e","ts":9000,"pid":2,"tid":7},
            {"name":"DrawFrame","ph":"b","ts":17000,"pid":2,"tid":7},
            {"name":"DrawFrame","ph":"e","ts":25000,"pid":2,"tid":7}
        ]}"#;
        let path = write_trace("be", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // 7 raw + 4 frame ticks = 11 raw events; 3 synthetic X appended.
        assert_eq!(trace.stats.parsed, 11);
        assert_eq!(
            trace.trace_events.iter().filter(|e| e.ph == b'X').count(),
            3
        );
        let main = super::super::detect_main_thread(&trace.trace_events);
        assert_eq!(main, 1);
        // RunTask synthetic is 60ms -> one long task; busy is the 60ms union.
        let res = crate::analysis::analyze_summary(&trace.trace_events, main);
        assert_eq!(res.long_task_count, 1);
        assert_eq!(res.main_thread_busy_us, 60_000.0);
        assert_eq!(res.runtask_total_us, 60_000.0);
        assert_eq!(res.long_tasks_total_us, 60_000.0);
        let jk = crate::analysis::analyze_jank(&trace.trace_events, main, None);
        assert!(!jk.clusters.is_empty(), "60ms task must cluster");
        // Lowercase frame pairing is unaffected by B/E normalization.
        let scope = crate::inspect::Scope {
            window: None,
            tid: None,
            pid: None,
            cat: None,
        };
        let (_, json) =
            crate::windowed::frames_section(&trace.trace_events, &scope, "DrawFrame", 1000.0);
        assert_eq!(json[0]["count"], 2);
    }

    fn tiny_event(ts: f64) -> String {
        format!("{{\"name\":\"Tiny\",\"ph\":\"X\",\"ts\":{ts},\"pid\":1,\"tid\":1}}")
    }

    fn giant_event(ts: f64, blob_len: usize) -> String {
        format!(
            "{{\"name\":\"BigEvent\",\"ph\":\"X\",\"ts\":{ts},\"pid\":1,\"tid\":1,\"args\":{{\"blob\":\"{}\"}}}}",
            "A".repeat(blob_len)
        )
    }

    fn histogram(evs: &[super::super::TraceEvent]) -> std::collections::HashMap<&str, usize> {
        let mut m = std::collections::HashMap::new();
        for e in evs {
            *m.entry(e.name.as_str()).or_insert(0) += 1;
        }
        m
    }

    /// A >64KB event starting just before `to` but closing well beyond
    /// `to + BLOCK` must still be owned by the chunk starting before `to`
    /// (regression: the old `to + BLOCK` walk cap dropped it entirely).
    #[test]
    fn chunk_work_keeps_giant_event_across_block_cap() {
        let t1 = tiny_event(1.0);
        let g = giant_event(2.0, 100 * 1024);
        let t2 = tiny_event(3.0);
        let bytes = format!("{{\"traceEvents\":[{t1},{g},{t2}]}}").into_bytes();
        assert!(g.len() > BLOCK + 1024, "giant event must exceed BLOCK");
        let (layout, blocks) = scan_layout(&bytes).expect("layout");
        // Giant element offsets within the buffer.
        let prefix = "{\"traceEvents\":[".len() + t1.len() + 1;
        let giant_start = prefix;
        let giant_end = giant_start + g.len();
        let to = giant_start + 50;
        assert!(
            giant_end > to + BLOCK,
            "giant must close well beyond to + BLOCK"
        );
        let buf = bytes.clone();
        let (first, _) = chunk_work(&buf, &blocks, layout.arr_open, to).expect("chunk 1");
        assert_eq!(first.len(), 2, "first chunk owns tiny + giant");
        assert_eq!(first[0].name, "Tiny");
        assert_eq!(first[1].name, "BigEvent");
        let args_len = first[1].args_raw().map(|s| s.len()).unwrap_or(0);
        assert!(args_len > BLOCK, "giant args preserved, got {args_len}");
        let (second, _) = chunk_work(&buf, &blocks, to, layout.arr_close).expect("chunk 2");
        assert_eq!(second.len(), 1, "second chunk owns only the trailing tiny");
        assert_eq!(second[0].name, "Tiny");
        assert_eq!(second[0].ts, 3.0);
        let big_total = first
            .iter()
            .chain(&second)
            .filter(|e| e.name == "BigEvent")
            .count();
        assert_eq!(
            big_total, 1,
            "giant event exactly once across adjacent ranges"
        );
        assert_eq!(first.len() + second.len(), 3);
    }

    /// `parse_parallel` with different thread counts must agree exactly
    /// (1000 events incl. one 120KB BigEvent, mirroring big_elem.json).
    #[test]
    fn parse_parallel_thread_counts_agree_on_big_elem() {
        let mut body = String::with_capacity(300 * 1024);
        for i in 0..1000 {
            if i > 0 {
                body.push(',');
            }
            if i == 500 {
                body.push_str(&giant_event(i as f64, 120 * 1024));
            } else {
                body.push_str(&tiny_event(i as f64));
            }
        }
        let bytes = format!("{{\"traceEvents\":[{body}]}}").into_bytes();
        let (layout, blocks) = scan_layout(&bytes).expect("layout");
        let mut all: Vec<Vec<super::super::TraceEvent>> = Vec::new();
        for threads in [1usize, 3, 4] {
            let buf = bytes.clone();
            let (evs, _, _) = parse_parallel(&buf, &layout, &blocks, threads).expect("parallel");
            assert_eq!(evs.len(), 1000, "threads={threads}");
            all.push(evs);
        }
        let h0 = histogram(&all[0]);
        assert_eq!(h0.get("BigEvent"), Some(&1));
        assert_eq!(h0.get("Tiny"), Some(&999));
        for (k, evs) in all.iter().enumerate().skip(1) {
            assert_eq!(histogram(evs), h0, "histogram differs for run {k}");
            assert_eq!(evs.len(), all[0].len());
            let ts0: Vec<f64> = all[0].iter().map(|e| e.ts).collect();
            let tsk: Vec<f64> = evs.iter().map(|e| e.ts).collect();
            assert_eq!(tsk, ts0, "event order differs for run {k}");
        }
    }

    fn write_trace(name: &str, body: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("chperf-{}-{}.json", name, std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// `id2` objects pair like `id`: `local` (string or number) wins,
    /// `global` is the fallback, and numeric locals equal plain integer ids.
    #[test]
    fn id2_pairs_like_id() {
        let body = r#"{"traceEvents":[
            {"name":"A","ph":"s","ts":1,"pid":1,"tid":1,"id2":{"local":"0xABC","global":"0xDEF"}},
            {"name":"A","ph":"f","ts":5,"pid":1,"tid":1,"id2":{"local":"0xABC"}},
            {"name":"B","ph":"s","ts":2,"pid":1,"tid":1,"id2":{"global":"g1"}},
            {"name":"B","ph":"f","ts":6,"pid":1,"tid":1,"id2":{"global":"g1"}},
            {"name":"C","ph":"s","ts":3,"pid":1,"tid":1,"id2":{"local":42}},
            {"name":"C","ph":"f","ts":7,"pid":1,"tid":1,"id":42}
        ]}"#;
        let path = write_trace("id2", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let evs = &trace.trace_events;
        assert_eq!(evs.len(), 6);
        for e in evs {
            assert!(e.has_id, "id2 event must carry an id");
        }
        assert_eq!(evs[0].id, evs[1].id, "same local string must pair");
        assert_eq!(evs[2].id, evs[3].id, "same global string must pair");
        assert_ne!(evs[0].id, evs[2].id, "local must win over global");
        assert_eq!(evs[4].id, 42, "numeric local must equal the integer");
        assert_eq!(
            evs[4].id, evs[5].id,
            "numeric id2 local must pair with plain id"
        );
        assert_eq!(trace.stats.parsed, 6);
    }

    /// `id2` ids are identical whether the event went through the fast
    /// tokenizer or the serde fallback (forced here with an escaped name).
    #[test]
    fn id2_consistent_across_fallback() {
        let fast_body = r#"{"traceEvents":[
            {"name":"A","ph":"s","ts":1,"pid":1,"tid":1,"id2":{"local":"0xABC"}}
        ]}"#;
        // The escaped quote bails the fast single-pass into the parallel +
        // tolerant-serde pipeline; the id2 event itself has no escapes.
        let slow_body = r#"{"traceEvents":[
            {"name":"A \"quoted\"","ph":"X","ts":0,"pid":1,"tid":1},
            {"name":"A","ph":"s","ts":1,"pid":1,"tid":1,"id2":{"local":"0xABC"}}
        ]}"#;
        let fast_path = write_trace("id2-fast", fast_body);
        let slow_path = write_trace("id2-slow", slow_body);
        let fast = parse_trace(&fast_path).unwrap();
        let slow = parse_trace(&slow_path).unwrap();
        let _ = std::fs::remove_file(&fast_path);
        let _ = std::fs::remove_file(&slow_path);
        assert_eq!(fast.trace_events.len(), 1);
        assert_eq!(slow.trace_events.len(), 2);
        assert!(fast.trace_events[0].has_id);
        assert!(slow.trace_events[1].has_id);
        assert_eq!(
            fast.trace_events[0].id, slow.trace_events[1].id,
            "id2 must normalize identically on both parse paths"
        );
    }

    /// Numeric strings for ts/dur/pid/tid coerce on both parse paths.
    #[test]
    fn string_numbers_accepted_for_ts_dur_pid_tid() {
        let body = r#"{"traceEvents":[
            {"name":"A","ph":"X","ts":"456.5","dur":"123","pid":"7","tid":"8"}
        ]}"#;
        let path = write_trace("strnum", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 1);
        let e = &trace.trace_events[0];
        assert_eq!(e.ts, 456.5);
        assert_eq!(e.dur, Some(123.0));
        assert_eq!(e.pid, 7);
        assert_eq!(e.tid, 8);
        assert!(
            trace.stats.malformed >= 1,
            "wrong-typed fields count as malformed"
        );
    }

    /// Negative durations clamp to 0.0, count as malformed, keep the event.
    #[test]
    fn negative_dur_clamps_to_zero_and_counts_malformed() {
        let body = r#"{"traceEvents":[
            {"name":"A","ph":"X","ts":1,"pid":1,"tid":1,"dur":-5},
            {"name":"B","ph":"X","ts":2,"pid":1,"tid":1,"dur":"-3"}
        ]}"#;
        let path = write_trace("negdur", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 2);
        assert_eq!(trace.trace_events[0].dur, Some(0.0));
        assert_eq!(trace.trace_events[1].dur, Some(0.0));
        assert_eq!(trace.stats.parsed, 2);
        assert!(trace.stats.malformed >= 2);
        assert_eq!(trace.stats.skipped, 0);
    }

    /// One structurally broken event is skipped and counted; the file still
    /// parses and the good events keep their order.
    #[test]
    fn single_bad_event_skipped_not_fatal() {
        let body = r#"{"traceEvents":[
            {"name":"A","ph":"X","ts":1,"pid":1,"tid":1},
            {"name": },
            {"name":"B","ph":"X","ts":2,"pid":1,"tid":1},
            42,
            {"name":"C","ph":"X","ts":3,"pid":1,"tid":1}
        ]}"#;
        let path = write_trace("badevent", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let names: Vec<&str> = trace.trace_events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
        assert_eq!(trace.stats.parsed, 3);
        assert_eq!(trace.stats.skipped, 2);
        assert!(trace.stats.malformed >= 2);
    }

    /// Frames from every TracingStartedInBrowser event merge by frame id;
    /// later events refresh url/name/process_id, and page_url is the
    /// outermost main frame's URL (string processId coerces).
    #[test]
    fn multi_tracing_started_in_browser_merges() {
        let body = r#"{"traceEvents":[
            {"name":"TracingStartedInBrowser","ph":"I","ts":1,"pid":1,"tid":1,"args":{"data":{"frames":[
                {"frame":"F1","url":"about:blank","name":"","processId":11,"isInPrimaryMainFrame":true},
                {"frame":"F2","parent":"F1","url":"https://child.example/","name":"c","processId":11}
            ]}}},
            {"name":"TracingStartedInBrowser","ph":"I","ts":2,"pid":1,"tid":1,"args":{"data":{"frames":[
                {"frame":"F1","url":"https://app.example/","name":"main","processId":"11","isOutermostMainFrame":true}
            ]}}},
            {"name":"RunTask","ph":"X","ts":3,"pid":1,"tid":1,"dur":5}
        ]}"#;
        let path = write_trace("frames", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let meta = trace.metadata.as_ref().expect("metadata must exist");
        assert_eq!(meta.frames.len(), 2, "frames merge by id across events");
        let main = meta.frames.iter().find(|f| f.id == "F1").unwrap();
        assert_eq!(main.url, "https://app.example/");
        assert_eq!(main.name, "main");
        assert_eq!(main.process_id, 11);
        assert!(main.is_main_frame);
        assert_eq!(meta.page_url.as_deref(), Some("https://app.example/"));
    }

    /// The legacy `isMainFrame` field marks the main frame when the newer
    /// markers are absent.
    #[test]
    fn legacy_is_main_frame_marks_main() {
        let body = r#"{"traceEvents":[
            {"name":"TracingStartedInBrowser","ph":"I","ts":1,"pid":1,"tid":1,"args":{"data":{"frames":[
                {"frame":"OLD","url":"https://old.example/","processId":9,"isMainFrame":true}
            ]}}}
        ]}"#;
        let path = write_trace("legacyframe", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let meta = trace.metadata.as_ref().expect("metadata must exist");
        assert_eq!(meta.frames.len(), 1);
        assert!(meta.frames[0].is_main_frame);
        assert_eq!(meta.page_url.as_deref(), Some("https://old.example/"));
    }

    /// An empty trace is an error, not a valid empty TraceFile.
    #[test]
    fn empty_trace_is_error() {
        let path = write_trace("empty", r#"{"traceEvents":[]}"#);
        let err = match parse_trace(&path) {
            Err(e) => e,
            Ok(_) => panic!("empty trace must fail"),
        };
        let _ = std::fs::remove_file(&path);
        assert!(
            err.to_string().contains("no events"),
            "unexpected error: {err}"
        );
    }

    /// A clean trace reports parsed == events with no skips or malformed.
    #[test]
    fn clean_parse_stats_are_zero_except_parsed() {
        let body = r#"{"traceEvents":[
            {"name":"A","ph":"X","ts":1,"pid":1,"tid":1,"dur":2},
            {"name":"B","ph":"X","ts":2,"pid":1,"tid":1}
        ]}"#;
        let path = write_trace("cleanstats", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.stats.parsed, 2);
        assert_eq!(trace.stats.skipped, 0);
        assert_eq!(trace.stats.malformed, 0);
    }

    /// Lenient fields (string ts/dur/pid/tid, negative-dur clamp, string
    /// id) agree field-for-field between the fast tokenizer and the serde
    /// fallback — including the malformed counts.
    #[test]
    fn lenient_fields_consistent_across_fallback() {
        let target =
            r#"{"name":"A","ph":"X","ts":"456.5","dur":"-3","pid":"7","tid":"8.9","id":"x7"}"#;
        let fast_body = format!(r#"{{"traceEvents":[{target}]}}"#);
        // The escaped quote bails the fast single-pass into the parallel +
        // tolerant-serde pipeline; the target event itself has no escapes.
        let slow_body = format!(
            r#"{{"traceEvents":[{{"name":"A \"quoted\"","ph":"X","ts":0,"pid":1,"tid":1}},{target}]}}"#
        );
        let fast_path = write_trace("lenient-fast", &fast_body);
        let slow_path = write_trace("lenient-slow", &slow_body);
        let fast = parse_trace(&fast_path).unwrap();
        let slow = parse_trace(&slow_path).unwrap();
        let _ = std::fs::remove_file(&fast_path);
        let _ = std::fs::remove_file(&slow_path);
        assert_eq!(fast.trace_events.len(), 1);
        assert_eq!(slow.trace_events.len(), 2);
        let (f, s) = (&fast.trace_events[0], &slow.trace_events[1]);
        assert_eq!(f.ts, 456.5);
        assert_eq!((f.ts, f.dur, f.pid, f.tid), (s.ts, s.dur, s.pid, s.tid));
        assert_eq!(f.dur, Some(0.0), "negative string dur clamps to 0");
        assert_eq!((f.pid, f.tid), (7, 8));
        assert!(f.has_id && s.has_id);
        assert_eq!(f.id, s.id, "string id must hash identically");
        assert_eq!(fast.stats.parsed, 1);
        assert_eq!(slow.stats.parsed, 2);
        // id + ts + dur + pid + tid coercions on both paths.
        assert_eq!(fast.stats.malformed, slow.stats.malformed);
        assert!(fast.stats.malformed >= 5, "got {}", fast.stats.malformed);
        assert_eq!(fast.stats.skipped, 0);
        assert_eq!(slow.stats.skipped, 0);
    }

    /// Mixed fast + tolerant chunks account every input element exactly
    /// once: ~135KB body forces 2 chunks at 64KB units; the first 64KB are
    /// all plain events (fast path) while escaped names in the tail push
    /// chunk 2 through the tolerant path. No event is double-counted on
    /// fallback, no serde thread-local count is lost across worker threads,
    /// and every string-ts coercion is counted wherever it lands.
    #[test]
    fn mixed_chunk_stats_account_every_element_once() {
        let mut parts: Vec<String> = Vec::with_capacity(2000);
        for i in 0..2000 {
            if i == 1999 {
                parts.push(r#"{"name": }"#.to_string()); // broken: skip
            } else if i >= 1300 {
                parts.push(format!(
                    r#"{{"name":"Ev \"q{i}\"","ph":"X","ts":{i},"pid":1,"tid":1}}"#
                ));
            } else {
                parts.push(format!(
                    r#"{{"name":"Ev{i}","ph":"X","ts":"{i}","pid":1,"tid":1}}"#
                ));
            }
        }
        let body = format!("{{\"traceEvents\":[{}]}}", parts.join(","));
        // Two 64KB chunks; the 1300 plain events (~70KB) cover chunk 1 so
        // it stays on the fast path, escapes land in chunk 2 (tolerant).
        assert!(body.len() > 64 * 1024, "need 2 chunks, got {}", body.len());
        let path = write_trace("mixedstats", &body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.stats.parsed + trace.stats.skipped, 2000);
        assert_eq!(trace.stats.parsed, 1999);
        assert_eq!(trace.stats.skipped, 1);
        // 1300 string-ts coercions (fast or tolerant chunk alike) + 1 skip.
        assert_eq!(trace.stats.malformed, 1301);
    }

    /// Truncated mid-event: the complete-event prefix is salvaged, the torn
    /// tail stops the walk. No panic, no whole-file failure.
    #[test]
    fn truncated_mid_event_salvages_complete_prefix() {
        let body = concat!(
            r#"{"traceEvents":["#,
            r#"{"name":"A","ph":"X","ts":1,"pid":1,"tid":1},"#,
            r#"{"name":"B","ph":"X","ts":2,"pid":1,"tid":1},"#,
            r#"{"name":"C","ph":"X","ts":3,"pid":1,"ti"#,
        );
        let path = write_trace("truncated", body);
        let trace = parse_trace(&path).expect("truncated prefix must salvage");
        let _ = std::fs::remove_file(&path);
        let names: Vec<&str> = trace.trace_events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
        assert_eq!(trace.stats.parsed, 2);
        assert_eq!(trace.stats.parsed + trace.stats.skipped, 2);
    }

    /// Truncated with zero complete events is still an error (nothing to
    /// analyze) — but a clear one, not a panic.
    #[test]
    fn truncated_with_no_complete_event_errors_clearly() {
        let path = write_trace(
            "truncated-none",
            r#"{"traceEvents":[{"name":"A","ph":"X","ts"#,
        );
        let err = match parse_trace(&path) {
            Err(e) => e,
            Ok(_) => panic!("must fail"),
        };
        let _ = std::fs::remove_file(&path);
        assert!(err.to_string().contains("no events"), "unexpected: {err}");
    }

    /// Trailing garbage after the array close is ignored: all events parse,
    /// stats stay clean.
    #[test]
    fn trailing_garbage_after_array_ignored() {
        let body = concat!(
            r#"{"traceEvents":["#,
            r#"{"name":"A","ph":"X","ts":1,"pid":1,"tid":1},"#,
            r#"{"name":"B","ph":"X","ts":2,"pid":1,"tid":1}]"#,
            r#"} this is not json at all"#,
        );
        let path = write_trace("trailing", body);
        let trace = parse_trace(&path).expect("trailing garbage must not fail");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 2);
        assert_eq!(trace.stats.parsed, 2);
        assert_eq!(trace.stats.skipped, 0);
    }

    /// Missing / null / non-array `traceEvents` is genuinely non-trace
    /// input: a clear error naming `traceEvents`, never a panic.
    #[test]
    fn non_array_trace_events_errors_clearly() {
        for (tag, body) in [
            ("missing", r#"{"metadata":{}}"#),
            ("null", r#"{"traceEvents":null}"#),
            ("object", r#"{"traceEvents":{}}"#),
            ("string", r#"{"traceEvents":"nope"}"#),
            ("number", r#"{"traceEvents":42}"#),
            ("toplevel-array", r#"[1,2,3]"#),
        ] {
            let path = write_trace(&format!("nonarray-{tag}"), body);
            let err = match parse_trace(&path) {
                Err(e) => e,
                Ok(_) => panic!("{tag}: must fail"),
            };
            let _ = std::fs::remove_file(&path);
            assert!(
                err.to_string().contains("traceEvents"),
                "{tag}: unexpected error: {err}"
            );
        }
    }

    /// One >1MB event parses intact (exceeds any chunk/walk cap), args kept.
    #[test]
    fn single_giant_event_over_1mb() {
        let blob_len = 1_200_000;
        let body = format!(
            "{{\"traceEvents\":[{{\"name\":\"Huge\",\"ph\":\"X\",\"ts\":1,\"pid\":1,\"tid\":1,\"args\":{{\"blob\":\"{}\"}}}}]}}",
            "B".repeat(blob_len)
        );
        let path = write_trace("giant1m", &body);
        let trace = parse_trace(&path).expect("giant event must parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 1);
        assert_eq!(trace.trace_events[0].name, "Huge");
        assert_eq!(trace.stats.parsed, 1);
        assert_eq!(trace.stats.skipped, 0);
        let raw = trace.trace_events[0].args_raw().expect("args kept");
        assert!(raw.len() > blob_len, "args preserved, got {}", raw.len());
    }

    /// `args` as string / number / null / missing: all kept, none fatal.
    #[test]
    fn args_wrong_types_kept_not_fatal() {
        let body = r#"{"traceEvents":[
            {"name":"S","ph":"X","ts":1,"pid":1,"tid":1,"args":"just a string"},
            {"name":"N","ph":"X","ts":2,"pid":1,"tid":1,"args":42},
            {"name":"Z","ph":"X","ts":3,"pid":1,"tid":1,"args":null},
            {"name":"M","ph":"X","ts":4,"pid":1,"tid":1}
        ]}"#;
        let path = write_trace("argstypes", body);
        let trace = parse_trace(&path).expect("args types must not fail");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 4);
        assert_eq!(trace.stats.parsed + trace.stats.skipped, 4);
        let evs = &trace.trace_events;
        assert_eq!(
            evs[0]
                .args_value()
                .and_then(|v| v.as_str().map(str::to_string))
                .as_deref(),
            Some("just a string")
        );
        assert_eq!(evs[1].args_value().and_then(|v| v.as_u64()), Some(42));
        assert!(evs[2].args_value().is_none());
        assert!(evs[3].args_value().is_none());
    }

    /// 200-deep nested `args` parses without stack overflow or panic (the
    /// fast path skips values iteratively; serde never recurses into
    /// `RawValue`).
    #[test]
    fn deeply_nested_args_no_panic() {
        let mut inner = String::from("1");
        for _ in 0..200 {
            inner = format!("{{\"a\":{inner}}}");
        }
        let body = format!(
            "{{\"traceEvents\":[{{\"name\":\"Deep\",\"ph\":\"X\",\"ts\":1,\"pid\":1,\"tid\":1,\"args\":{inner}}}]}}"
        );
        let path = write_trace("deepnest", &body);
        let trace = parse_trace(&path).expect("deep nesting must not fail");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 1);
        assert_eq!(trace.stats.parsed + trace.stats.skipped, 1);
    }

    /// `ts` as string / float / negative / null coerces or defaults, file
    /// survives with exact accounting.
    #[test]
    fn duplicate_ts_types_coerce() {
        let body = r#"{"traceEvents":[
            {"name":"S","ph":"X","ts":"100.5","pid":1,"tid":1},
            {"name":"F","ph":"X","ts":200.25,"pid":1,"tid":1},
            {"name":"G","ph":"X","ts":-5,"pid":1,"tid":1},
            {"name":"U","ph":"X","ts":null,"pid":1,"tid":1}
        ]}"#;
        let path = write_trace("tstypes", body);
        let trace = parse_trace(&path).expect("ts types must not fail");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 4);
        assert_eq!(trace.stats.parsed, 4);
        assert_eq!(trace.stats.skipped, 0);
        let evs = &trace.trace_events;
        assert_eq!(evs[0].ts, 100.5);
        assert_eq!(evs[1].ts, 200.25);
        assert_eq!(evs[2].ts, -5.0);
        assert_eq!(evs[3].ts, 0.0);
    }

    /// `ph` empty / missing / two-byte: empty and missing read as 0,
    /// two-byte keeps the first byte. No bail-outs.
    #[test]
    fn ph_empty_missing_two_byte() {
        let body = r#"{"traceEvents":[
            {"name":"E","ph":"","ts":1,"pid":1,"tid":1},
            {"name":"M","ts":2,"pid":1,"tid":1},
            {"name":"T","ph":"Xy","ts":3,"pid":1,"tid":1}
        ]}"#;
        let path = write_trace("phtypes", body);
        let trace = parse_trace(&path).expect("ph variants must not fail");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 3);
        assert_eq!(trace.stats.parsed, 3);
        assert_eq!(trace.stats.skipped, 0);
        assert_eq!(trace.trace_events[0].ph, 0);
        assert_eq!(trace.trace_events[1].ph, 0);
        assert_eq!(trace.trace_events[2].ph, b'X');
    }

    /// Literal control bytes in strings don't panic the tokenizer (lenient
    /// fast path keeps the event).
    #[test]
    fn control_chars_in_strings_no_panic() {
        let body =
            "{\"traceEvents\":[{\"name\":\"A\x01B\",\"ph\":\"X\",\"ts\":1,\"pid\":1,\"tid\":1}]}";
        let path = write_trace("ctrlchars", body);
        let trace = parse_trace(&path).expect("control chars must not fail");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 1);
        assert_eq!(trace.stats.parsed + trace.stats.skipped, 1);
    }

    /// A lone-surrogate escape (`\uD800`) split across a 64KB block boundary
    /// parses without panic; every input element is accounted exactly once.
    #[test]
    fn lone_surrogate_split_across_block_boundary() {
        // Layout: prefix + padded args put `\` at byte 65535 of block 0.
        let mut json =
            String::from(r#"{"traceEvents":[{"name":"pad","ph":"X","ts":0,"args":{"s":""#);
        let prefix_len = json.len();
        // `\uD800` is 6 bytes; pad so `\` lands on the last byte of block 0.
        let pad = 65_535 - prefix_len;
        json.push_str(&"x".repeat(pad));
        json.push_str("\\uD800");
        json.push_str("rest");
        json.push_str("\"}}");
        for i in 0..10 {
            json.push_str(&format!(r#",{{"name":"tail{i}","ph":"X","ts":{i}}}"#));
        }
        json.push_str("]}");
        let b = json.as_bytes();
        assert_eq!(b[65_535], b'\\', "escape must straddle the boundary");
        let path = write_trace("surrogate-split", &json);
        let trace = parse_trace(&path).expect("surrogate split must not fail");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 11);
        assert_eq!(trace.stats.parsed + trace.stats.skipped, 11);
    }

    /// `\r\n` line endings parse like `\n`.
    #[test]
    fn crlf_line_endings() {
        let body = "{\r\n\"traceEvents\": [\r\n{\"name\":\"A\",\"ph\":\"X\",\"ts\":1,\"pid\":1,\"tid\":1},\r\n{\"name\":\"B\",\"ph\":\"X\",\"ts\":2,\"pid\":1,\"tid\":1}\r\n]\r\n}";
        let path = write_trace("crlf", body);
        let trace = parse_trace(&path).expect("crlf must parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 2);
        assert_eq!(trace.stats.parsed, 2);
        assert_eq!(trace.stats.skipped, 0);
    }

    /// A UTF-8 BOM at the file start is stripped: clean stats, fast path.
    /// Parsing twice yields two independent, fully-owned traces: dropping
    /// the first must not affect the second (no borrowed file buffer, no
    /// leaked interner — memory is freed with each `TraceFile`).
    #[test]
    fn bom_at_start_stripped() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice(
            br#"{"traceEvents":[
            {"name":"A","ph":"X","ts":1,"pid":1,"tid":1},
            {"name":"B","ph":"X","ts":2,"pid":1,"tid":1}
        ]}"#,
        );
        let path = std::env::temp_dir().join(format!("chperf-bom-{}.json", std::process::id()));
        std::fs::write(&path, body).unwrap();
        let trace = parse_trace(&path).expect("BOM must be stripped");
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), 2);
        assert_eq!(trace.stats.parsed, 2);
        assert_eq!(trace.stats.skipped, 0);
        assert_eq!(trace.stats.malformed, 0);
    }

    /// Parsing the same file twice yields two independent, fully-owned
    /// traces: dropping the first (freeing all its memory) leaves the
    /// second intact. Guards the no-leak model — events borrow nothing, so
    /// there is no shared buffer whose lifetime could couple parses.
    #[test]
    fn double_parse_yields_independent_traces() {
        let body = r#"{"traceEvents":[
            {"name":"RunTask","cat":"devtools.timeline","ph":"X","ts":1,"pid":1,"tid":1,"dur":5,"args":{"x":1}},
            {"name":"Paint","ph":"X","ts":2,"pid":1,"tid":1}
        ]}"#;
        let path = write_trace("twice", body);
        let first = parse_trace(&path).expect("first parse must succeed");
        let second = parse_trace(&path).expect("second parse must succeed");
        let _ = std::fs::remove_file(&path);
        assert_eq!(first.trace_events.len(), 2);
        assert_eq!(second.trace_events.len(), 2);
        for (a, b) in first.trace_events.iter().zip(second.trace_events.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.cat, b.cat);
            assert_eq!(a.args_raw(), b.args_raw());
        }
        drop(first);
        assert_eq!(second.trace_events[0].name, "RunTask");
        assert_eq!(
            second.trace_events[0].args_value().unwrap()["x"],
            serde_json::json!(1)
        );
    }

    /// Throughput sanity: 200k clean events parse fast on the fast
    /// single-pass path (well under 3s even in debug) with exact stats.
    #[test]
    fn perf_200k_events_sane_throughput() {
        use std::sync::atomic::Ordering;
        let n = 200_000usize;
        let mut body = String::with_capacity(n * 56);
        body.push_str("{\"traceEvents\":[");
        for i in 0..n {
            if i > 0 {
                body.push(',');
            }
            body.push_str(&format!(
                "{{\"name\":\"Ev{}\",\"ph\":\"X\",\"ts\":{},\"pid\":1,\"tid\":1}}",
                i % 50,
                i
            ));
        }
        body.push_str("]}");
        let path = write_trace("perf200k", &body);
        drop(body);
        let before = FAST_SINGLE_PASS_HITS.load(Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        let trace = parse_trace(&path).expect("200k must parse");
        let dt = t0.elapsed();
        let _ = std::fs::remove_file(&path);
        assert_eq!(trace.trace_events.len(), n);
        assert_eq!(trace.stats.parsed, n);
        assert_eq!(trace.stats.skipped, 0);
        assert_eq!(trace.stats.malformed, 0);
        assert!(
            FAST_SINGLE_PASS_HITS.load(Ordering::Relaxed) > before,
            "clean input must take the fast single-pass path"
        );
        assert!(
            dt.as_secs_f64() < 3.0,
            "200k events took {:.2}s, expected < 3s (fast path regression?)",
            dt.as_secs_f64()
        );
    }

    /// `normalize_be_pairs` results flow through `parse_trace` with raw-only
    /// stats, and `detect_main_thread` counts exactly one vote per pair
    /// (synthetic `X` only — original `B`/`E` never vote, no doubling).
    #[test]
    fn be_synthetic_x_counts_once_in_main_detection() {
        // Thread 1: three genuine X RunTasks. Thread 2: two B/E pairs
        // (2 synthetic X after normalize). B/E originals must not vote.
        let body = r#"{"traceEvents":[
            {"name":"thread_name","ph":"M","ts":0,"pid":1,"tid":1,"args":{"name":"Worker"}},
            {"name":"thread_name","ph":"M","ts":0,"pid":1,"tid":2,"args":{"name":"Worker"}},
            {"name":"RunTask","ph":"X","ts":1,"pid":1,"tid":1,"dur":10},
            {"name":"RunTask","ph":"X","ts":2,"pid":1,"tid":1,"dur":10},
            {"name":"RunTask","ph":"X","ts":3,"pid":1,"tid":1,"dur":10},
            {"name":"RunTask","ph":"B","ts":4,"pid":1,"tid":2},
            {"name":"RunTask","ph":"E","ts":14,"pid":1,"tid":2},
            {"name":"RunTask","ph":"B","ts":5,"pid":1,"tid":2},
            {"name":"RunTask","ph":"E","ts":15,"pid":1,"tid":2}
        ]}"#;
        let path = write_trace("bexcount", body);
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // 9 raw events in, 2 synthetics appended, stats count raw only.
        assert_eq!(trace.stats.parsed, 9);
        assert_eq!(trace.trace_events.len(), 11);
        assert_eq!(super::super::detect_main_thread(&trace.trace_events), 1);
        // Flip the majority: four B/E pairs on thread 2 (4 synthetic X)
        // beat three genuine X on thread 1 — synthetics elect, originals
        // don't double-vote.
        let body2 = r#"{"traceEvents":[
            {"name":"RunTask","ph":"X","ts":1,"pid":1,"tid":1,"dur":10},
            {"name":"RunTask","ph":"X","ts":2,"pid":1,"tid":1,"dur":10},
            {"name":"RunTask","ph":"X","ts":3,"pid":1,"tid":1,"dur":10},
            {"name":"RunTask","ph":"B","ts":4,"pid":1,"tid":2},
            {"name":"RunTask","ph":"E","ts":14,"pid":1,"tid":2},
            {"name":"RunTask","ph":"B","ts":5,"pid":1,"tid":2},
            {"name":"RunTask","ph":"E","ts":15,"pid":1,"tid":2},
            {"name":"RunTask","ph":"B","ts":6,"pid":1,"tid":2},
            {"name":"RunTask","ph":"E","ts":16,"pid":1,"tid":2},
            {"name":"RunTask","ph":"B","ts":7,"pid":1,"tid":2},
            {"name":"RunTask","ph":"E","ts":17,"pid":1,"tid":2}
        ]}"#;
        let path2 = write_trace("bexcount2", body2);
        let trace2 = parse_trace(&path2).unwrap();
        let _ = std::fs::remove_file(&path2);
        assert_eq!(trace2.stats.parsed, 11);
        assert_eq!(trace2.trace_events.len(), 15);
        assert_eq!(super::super::detect_main_thread(&trace2.trace_events), 2);
    }
}
