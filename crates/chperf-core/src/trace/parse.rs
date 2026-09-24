//! The hand-rolled, block-parallel `traceEvents` tokenizer and `parse_trace`.

use super::{TraceEvent, TraceFile, TraceMetadata, intern_name};
use std::io::Read;
use std::path::Path;

/// Fast path: parse all elements in a chunk's byte range [s, e) without
/// serde. Elements are objects separated by commas (whitespace allowed).
/// Returns Err when ANY element uses a construct the fast parser bails on
/// (escaped strings, wrong-typed fields, malformed shapes) — the caller
/// then falls back to the serde path on the pristine bytes.
fn parse_events_fast(bytes: &[u8], s: usize, e: usize) -> Result<Vec<TraceEvent>, ()> {
    // ~200 bytes per event on Chrome traces: reserve to skip reallocs.
    let mut events: Vec<TraceEvent> = Vec::with_capacity((e - s).max(1) / 200);
    let mut p = s;
    loop {
        p = skip_ws(bytes, p);
        if p >= e {
            break;
        }
        if bytes[p] == b',' {
            p += 1;
            continue;
        }
        let (ev, next) = parse_event_fast(bytes, p)?;
        events.push(ev);
        p = next;
    }
    Ok(events)
}

/// Parse one event object starting at bytes[p] == b'{'. Returns the event
/// and the position after its closing '}'.
fn parse_event_fast(bytes: &[u8], p: usize) -> Result<(TraceEvent, usize), ()> {
    let len = bytes.len();
    let mut i = p;
    if i >= len || bytes[i] != b'{' {
        return Err(());
    }
    let mut ev = TraceEvent {
        name: "",
        id: 0,
        has_id: false,
        ph: 0,
        ts: 0.0,
        dur: None,
        tid: 0,
        pid: 0,
        cat: None,
        args: None,
        args_cache: std::sync::OnceLock::new(),
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
                let (v, a) = parse_string_intern(bytes, i)?;
                ev.name = v;
                i = a;
            }
            b"ph" => {
                let (v, a) = parse_phase(bytes, i)?;
                ev.ph = v;
                i = a;
            }
            b"id" => {
                // Lenient: async ids are ints in practice, but a string /
                // negative / float id must not fail the whole chunk — it just
                // leaves `has_id` false and pairing skips the event.
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
            b"ts" => {
                let (v, a) = parse_num_f64(bytes, i)?;
                ev.ts = v;
                i = a;
            }
            b"dur" => {
                if bytes[i] == b'n' {
                    if !is_literal_null(bytes, i) {
                        return Err(());
                    }
                    i = skip_value(bytes, i)?;
                    ev.dur = None;
                } else {
                    let (v, a) = parse_num_f64(bytes, i)?;
                    ev.dur = Some(v);
                    i = a;
                }
            }
            b"tid" => {
                let (v, a) = parse_num_i64(bytes, i)?;
                ev.tid = v;
                i = a;
            }
            b"pid" => {
                let (v, a) = parse_num_i64(bytes, i)?;
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
                    let (v, a) = parse_string_intern(bytes, i)?;
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
                    ev.args = Some(std::str::from_utf8(&bytes[i..end]).map_err(|_| ())?.into());
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

/// Unescaped JSON string value (bytes between quotes), interned instead of
/// allocating a new `String` per event (event names and categories repeat
/// heavily across a trace). Escaped strings are handled by the serde
/// fallback. Returns (interned str, pos after closing quote).
fn parse_string_intern(bytes: &[u8], p: usize) -> Result<(&'static str, usize), ()> {
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
            Ok((intern_name(s), q + 1))
        }
        None => Err(()),
    }
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
    Ok((if neg { (v as i64).wrapping_neg() as u64 } else { v }, next))
}

/// The value starting at p is exactly the literal `null` (not `nullx`).
fn is_literal_null(bytes: &[u8], p: usize) -> bool {
    let len = bytes.len();
    if p + 4 > len || &bytes[p..p + 4] != b"null" {
        return false;
    }
    p + 4 == len || matches!(bytes[p + 4], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
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
            while i < len && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
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
    let threads = std::thread::available_parallelism()
        .map(|t| t.get())
        .unwrap_or(1)
        .max(1);
    // Full fan-out here: both phases are streaming and bandwidth-bound, so
    // even efficiency cores help (unlike the parse workers, capped at 6).
    let chunk_count = if threads >= 4 && nb >= 64 { threads } else { 1 };

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
                        unsafe { *ptr_q.add(lo + k) = nq % 2 == 1; }
                        unsafe { *ptr_t.add(lo + k) = trail; }
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
                        unsafe { *ptr_d.add(lo + k) = dd; }
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
    let (arr_open, _opener) = find_top_level_open(bytes, &blocks, b"traceEvents")?;
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

/// One thread's parse result: events plus the separator-comma positions it
/// replaced (for restore on failure).
type ChunkResult = Result<(Vec<TraceEvent>, Vec<usize>), String>;

/// Parse the `traceEvents` array in parallel. Each thread walks its own byte
/// range (starting from a block boundary with known exact state), finds the
/// element ranges, replaces their separator commas with spaces in-place
/// (StreamDeserializer rejects commas), then parses the slice directly from
/// the original buffer — zero copying. On failure all commas are restored
/// and the caller falls back to a whole-buffer parse.
fn parse_parallel(
    bytes: &mut [u8],
    layout: &Layout,
    blocks: &Blocks,
    threads: usize,
) -> Result<(Vec<TraceEvent>, Option<TraceMetadata>), Box<dyn std::error::Error>> {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let threads = threads.max(1);
    let span = layout.arr_close - layout.arr_open;

    let t_scope = std::time::Instant::now();

    // Per-unit: (unit index, events, comma positions for restore).
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
                            // Units are disjoint, so the serde-fallback comma
                            // mutation is safe despite the shared raw pointer.
                            let bytes = unsafe {
                                std::slice::from_raw_parts_mut(ptr as *mut u8, total_len)
                            };
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

    // Unwrap chunk results; on any failure restore commas and propagate.
    let mut parsed: Vec<Vec<TraceEvent>> = Vec::with_capacity(results.len());
    let mut replaced: Vec<usize> = Vec::new();
    for (_, r) in results {
        match r {
            Ok((events, commas)) => {
                parsed.push(events);
                replaced.extend(commas);
            }
            Err(e) => {
                for j in replaced {
                    bytes[j] = b',';
                }
                return Err(Box::<dyn std::error::Error>::from(format!("chunk parse failed: {}", e)));
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
        eprintln!("  [parse] merge: {:.1}ms", t_merge.elapsed().as_secs_f64() * 1000.0);
    }

    let metadata = match layout.metadata {
        Some((s, e)) => serde_json::from_slice::<TraceMetadata>(&bytes[s..e]).ok(),
        None => None,
    };
    Ok((events, metadata))
}

/// Walk `[from_block_start, to)` (extended to finish the element containing
/// `to`), collecting element ranges starting ≥ `from`, mutating their
/// separator commas, and parsing the slice. Returns the events and the
/// mutated comma positions. The walk visits only structural positions
/// (memchr-accelerated), not every byte.
fn chunk_work(
    bytes: &mut [u8],
    blocks: &Blocks,
    from: usize,
    to: usize,
) -> Result<(Vec<TraceEvent>, Vec<usize>), String> {
    let block = from / BLOCK;
    let start = block * BLOCK;
    let mut d = blocks.depth_at[block];
    let mut in_string = blocks.in_string_at[block];
    let mut replaced: Vec<usize> = Vec::new();

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
                        // Record the separator comma (skip whitespace);
                        // mutation happens after the walk (the memchr
                        // iterators hold an immutable borrow).
                        let mut j = i + 1;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if j < bytes.len() && bytes[j] == b',' && j < to {
                            replaced.push(j);
                        }
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
        _ => return Ok((Vec::new(), replaced)),
    };

    // Fast path: hand-rolled tokenizer on the pristine bytes (separator commas
    // intact). Any failure falls back to the serde stream path below.
    let t_fast = std::time::Instant::now();
    let fast = parse_events_fast(bytes, s, e);
    if let Ok(events) = fast {
        if std::env::var("CHPERF_DEBUG").is_ok() {
            eprintln!("  [parse] fast {}..{}: {:.1}ms, {} events", s, e, t_fast.elapsed().as_secs_f64() * 1000.0, events.len());
        }
        return Ok((events, Vec::new()));
    }

    // Separator commas → spaces, in place (disjoint per chunk).
    for j in &replaced {
        bytes[*j] = b' ';
    }

    let events = serde_json::Deserializer::from_slice(&bytes[s..e])
        .into_iter()
        .collect::<Result<Vec<TraceEvent>, _>>()
        .map_err(|err| {
            // Restore this chunk's commas before returning: parse_parallel's
            // Err arm only restores chunks that finished BEFORE this one, so
            // a `?` here would leave the buffer corrupted for the
            // whole-buffer fallback — its error would then point at spaces
            // instead of the real defect.
            for j in &replaced {
                bytes[*j] = b',';
            }
            if std::env::var("CHPERF_DEBUG").is_ok() {
                eprintln!("  [dbg] slice [{s}..{e}): {}", String::from_utf8_lossy(&bytes[s..e]));
            }
            format!("{}", err)
        })?;
    Ok((events, replaced))
}


pub fn parse_trace(path: &Path) -> Result<TraceFile, Box<dyn std::error::Error>> {
    let mut file = std::fs::File::open(path)?;
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
        let mut out = Vec::with_capacity(hint.unwrap_or(0));
        flate2::read::GzDecoder::new(file).read_to_end(&mut out)?;
        out
    } else {
        let mut out = Vec::with_capacity(file.metadata().map(|m| m.len() as usize).unwrap_or(0));
        file.take(u64::MAX).read_to_end(&mut out)?;
        out
    };

    // Fast path: chunk-parallel parse of the traceEvents array.
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let mut trace: Option<TraceFile> = None;
    let mut scan_none = false;
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
        match parse_parallel(&mut bytes, &layout, &blocks, threads) {
            Ok((trace_events, metadata)) => {
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
    // Fallback: whole-buffer parse (unusual layout, non-object elements, …).
    // Note: must NOT use unwrap_or — its argument is evaluated eagerly and
    // would re-parse the comma-mutated buffer on the happy path.
    let mut trace = match trace {
        Some(t) => t,
        None => {
            if scan_none
                && memchr::memmem::find(&bytes, b"traceEvents").is_none()
            {
                return Err(format!(
                    "{}: not a Chrome trace (missing top-level \"traceEvents\" array)",
                    path.display()
                )
                .into());
            }
            serde_json::from_slice(&bytes).map_err(|e| {
                format!("{}: failed to parse trace: {}", path.display(), e)
            })?
        }
    };
    if debug {
        eprintln!("  [parse] trace ready: {} events", trace.trace_events.len());
    }

    // Extract page URL from TracingStartedInBrowser event
    {
        let meta = trace.metadata.get_or_insert(TraceMetadata {
            cpu_throttling: None,
            source: None,
            start_time: None,
            network_throttling: None,
            hardware_concurrency: None,
            host_dpr: None,
            page_url: None,
        });
        if meta.page_url.is_none() {
            for e in &trace.trace_events {
                if e.name == "TracingStartedInBrowser" {
                    if let Some(args) = e.args_value()
                        && let Some(frames) = args
                            .get("data")
                            .and_then(|d| d.get("frames"))
                            .and_then(|f| f.as_array())
                        {
                            for frame in frames {
                                if let Some(url) = frame.get("url").and_then(|u| u.as_str())
                                    && !url.is_empty() && url != "about:blank" {
                                        meta.page_url = Some(url.to_string());
                                        break;
                                    }
                            }
                        }
                    break;
                }
            }
        }
    }

    Ok(trace)
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
        let mut json = String::from(
            r#"{"traceEvents":[{"name":"trap","ph":"X","ts":1,"dur":1,"args":{"s":""#,
        );
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

        let path = std::env::temp_dir().join(format!("chperf-esc-split-{}.json", std::process::id()));
        std::fs::write(&path, json).unwrap();
        let trace = parse_trace(&path).expect("parse must succeed");
        std::fs::remove_file(&path).ok();
        // 1 trap + 300 tail events; the old walk saw 763/1063-style loss.
        assert_eq!(trace.trace_events.len(), 301);
        assert_eq!(trace.trace_events[0].name, "trap");
        assert_eq!(trace.trace_events[300].name, "tail299");
    }

    /// The fast tokenizer must capture async `id`s (integer) for s/f pairing,
    /// leave `has_id` false when the field is absent, and not bail the chunk
    /// on a string / negative id (those are simply left unpaired).
    #[test]
    fn fast_path_parses_async_ids_leniently() {
        let json = serde_json::json!({
            "traceEvents": [
                {"name": "AnimationFrame", "ph": "s", "ts": 1_000.0, "pid": 1, "tid": 1, "id": 42},
                {"name": "AnimationFrame", "ph": "f", "ts": 5_000.0, "pid": 1, "tid": 1, "id": 42},
                {"name": "AnimationFrame", "ph": "s", "ts": 2_000.0, "pid": 1, "tid": 1},
                {"name": "AnimationFrame", "ph": "s", "ts": 3_000.0, "pid": 1, "tid": 1, "id": "x7"},
                {"name": "AnimationFrame", "ph": "s", "ts": 4_000.0, "pid": 1, "tid": 1, "id": -3}
            ]
        });
        let path = std::env::temp_dir().join(format!("chperf-async-id-{}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        let trace = parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let evs = &trace.trace_events;
        assert_eq!(evs.len(), 5);
        assert_eq!(evs[0].id, 42);
        assert!(evs[0].has_id);
        assert_eq!(evs[1].id, 42);
        assert!(evs[1].has_id);
        // Absent / string / negative ids: present event, no id.
        for (k, e) in evs.iter().enumerate().skip(2) {
            assert!(!e.has_id, "event {} should have no id", k);
        }
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
            *m.entry(e.name).or_insert(0) += 1;
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
        let mut buf = bytes.clone();
        let (first, _) = chunk_work(&mut buf, &blocks, layout.arr_open, to).expect("chunk 1");
        assert_eq!(first.len(), 2, "first chunk owns tiny + giant");
        assert_eq!(first[0].name, "Tiny");
        assert_eq!(first[1].name, "BigEvent");
        let args_len = first[1].args.as_ref().map(|s| s.len()).unwrap_or(0);
        assert!(args_len > BLOCK, "giant args preserved, got {args_len}");
        let (second, _) = chunk_work(&mut buf, &blocks, to, layout.arr_close).expect("chunk 2");
        assert_eq!(second.len(), 1, "second chunk owns only the trailing tiny");
        assert_eq!(second[0].name, "Tiny");
        assert_eq!(second[0].ts, 3.0);
        let big_total = first.iter().chain(&second).filter(|e| e.name == "BigEvent").count();
        assert_eq!(big_total, 1, "giant event exactly once across adjacent ranges");
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
            let mut buf = bytes.clone();
            let (evs, _) = parse_parallel(&mut buf, &layout, &blocks, threads).expect("parallel");
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
}
