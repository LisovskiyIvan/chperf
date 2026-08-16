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
                let (v, a) = parse_num_u64(bytes, i)?;
                ev.tid = v;
                i = a;
            }
            b"pid" => {
                let (v, a) = parse_num_u64(bytes, i)?;
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
                    let (v, a) = parse_string(bytes, i)?;
                    ev.cat = Some(v.into());
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

/// Unescaped JSON string value (bytes between quotes). Escaped strings are
/// handled by the serde fallback. Returns (String, pos after closing quote).
fn parse_string(bytes: &[u8], p: usize) -> Result<(String, usize), ()> {
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
            let s = std::str::from_utf8(&bytes[p + 1..q]).map_err(|_| ())?.to_string();
            Ok((s, q + 1))
        }
        None => Err(()),
    }
}

/// Like `parse_string`, but interns the result instead of allocating a new
/// `String` per event (event names repeat heavily across a trace).
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
/// `parse_string`. Returns (byte, pos after closing quote).
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
/// block start. Depth is computed from raw brace/bracket counts (ignoring
/// strings — validated to match the exact state on real traces; strings in
/// Chrome traces almost never contain braces), in-string state from exact
/// (escape-aware) quote parity. Both are cheap SIMD-counted, so the full
/// element walk only runs inside the parse chunks.
struct Blocks {
    depth_at: Vec<i64>,
    in_string_at: Vec<bool>,
}

const BLOCK: usize = 64 * 1024;

fn build_blocks(bytes: &[u8]) -> Blocks {
    let nb = bytes.len().div_ceil(BLOCK);
    let threads = std::thread::available_parallelism()
        .map(|t| t.get())
        .unwrap_or(1)
        .max(1);
    let chunk_count = if threads >= 4 && nb >= 64 { threads } else { 1 };

    // Per-block counts are independent — count in parallel.
    let mut opens = vec![0i64; nb];
    let mut closes = vec![0i64; nb];
    let mut quote_parity = vec![false; nb];
    if chunk_count > 1 {
        let ptr_o: usize = opens.as_mut_ptr() as usize;
        let ptr_c: usize = closes.as_mut_ptr() as usize;
        let ptr_q: usize = quote_parity.as_mut_ptr() as usize;
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunk_count);
            for t in 0..chunk_count {
                let lo = nb * t / chunk_count;
                let hi = nb * (t + 1) / chunk_count;
                let bstart = lo * BLOCK;
                let bend = (hi * BLOCK).min(bytes.len());
                let bytes = &bytes[bstart..bend];
                handles.push(s.spawn(move || {
                    let ptr_o = ptr_o as *mut i64;
                    let ptr_c = ptr_c as *mut i64;
                    let ptr_q = ptr_q as *mut bool;
                    // Exact unescaped-quote parity per block: a quote is
                    // escaped iff preceded by an odd run of backslashes.
                    let mut prev_trail = 0usize; // backslashes ending previous block
                    for (k, chunk) in bytes.chunks(BLOCK).enumerate() {
                        let bi = lo + k;
                        unsafe {
                            *ptr_o.add(bi) = memchr::memchr2_iter(b'{', b'[', chunk).count() as i64;
                            *ptr_c.add(bi) = memchr::memchr2_iter(b'}', b']', chunk).count() as i64;
                        }
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
                        unsafe { *ptr_q.add(bi) = nq % 2 == 1; }
                        let mut t2 = 0usize;
                        let mut kk = chunk.len();
                        while kk > 0 && chunk[kk - 1] == b'\\' {
                            t2 += 1;
                            kk -= 1;
                        }
                        prev_trail = t2;
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });
    } else {
        for (k, chunk) in bytes.chunks(BLOCK).enumerate() {
            opens[k] = memchr::memchr2_iter(b'{', b'[', chunk).count() as i64;
            closes[k] = memchr::memchr2_iter(b'}', b']', chunk).count() as i64;
            let mut nq = 0usize;
            for q in memchr::memchr_iter(b'"', chunk) {
                let mut run = 0usize;
                let mut kk = q;
                while kk > 0 && chunk[kk - 1] == b'\\' {
                    run += 1;
                    kk -= 1;
                }
                if run.is_multiple_of(2) {
                    nq += 1;
                }
            }
            quote_parity[k] = nq % 2 == 1;
        }
    }

    // Sequential prefix sums: depth and in-string state at each block start.
    let mut depth_at = Vec::with_capacity(nb + 1);
    let mut in_string_at = Vec::with_capacity(nb + 1);
    let mut d = 0i64;
    let mut s = false;
    depth_at.push(0);
    in_string_at.push(false);
    for k in 0..nb {
        d += opens[k] - closes[k];
        s ^= quote_parity[k];
        depth_at.push(d);
        in_string_at.push(s);
    }
    Blocks {
        depth_at,
        in_string_at,
    }
}

/// Exact (string-aware) depth and in-string state at byte `pos`, by walking
/// only the (≤64KB) block prefix. `d`/`s` are the block-start state.
fn state_at(bytes: &[u8], blocks: &Blocks, pos: usize) -> (i64, bool) {
    let block = pos / BLOCK;
    let mut d = blocks.depth_at[block];
    let mut s = blocks.in_string_at[block];
    walk_range(&bytes[block * BLOCK..pos], &mut d, &mut s);
    (d, s)
}

/// String-aware walk over a byte range, updating depth and in-string state.
fn walk_range(r: &[u8], d: &mut i64, s: &mut bool) {
    let mut in_str = *s;
    let mut esc = false;
    for &b in r {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
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
        let mut i = bs;
        while i < be {
            let b = bytes[i];
            if s {
                if b == b'"' {
                    let mut run = 0usize;
                    let mut kk = i;
                    while kk > bs && bytes[kk - 1] == b'\\' {
                        run += 1;
                        kk -= 1;
                    }
                    if run.is_multiple_of(2) {
                        s = false;
                    }
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
        let mut end = None;
        for (i, &b) in bytes[bs..].iter().enumerate() {
            let idx = bs + i;
            if s {
                if b == b'"' {
                    let mut run = 0usize;
                    let mut k = idx;
                    while k > bs && bytes[k - 1] == b'\\' {
                        run += 1;
                        k -= 1;
                    }
                    if run.is_multiple_of(2) {
                        s = false;
                    }
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
fn parse_parallel(bytes: &mut [u8], layout: &Layout, blocks: &Blocks) -> Result<(Vec<TraceEvent>, Option<TraceMetadata>), Box<dyn std::error::Error>> {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let threads = parse_thread_count(&(layout.arr_close - layout.arr_open));
    let span = layout.arr_close - layout.arr_open;

    let t_scope = std::time::Instant::now();

    // Per-thread: (events, comma positions for restore).
    let results: Vec<ChunkResult> = if threads <= 1 {
        let r = chunk_work(bytes, blocks, layout.arr_open, layout.arr_close);
        vec![r]
    } else {
        std::thread::scope(|s| {
            let ptr: usize = bytes.as_ptr() as usize;
            let mut handles = Vec::with_capacity(threads);
            for t in 0..threads {
                let from = layout.arr_open + span * t / threads;
                let to = if t + 1 == threads {
                    layout.arr_close
                } else {
                    layout.arr_open + span * (t + 1) / threads
                };
                handles.push(s.spawn(move || {
                    let t0 = std::time::Instant::now();
                    let bytes = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, layout.arr_close) };
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
                    r
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("chunk thread panicked".into()))).collect()
        })
    };

    // Unwrap chunk results; on any failure restore commas and propagate.
    let mut parsed: Vec<Vec<TraceEvent>> = Vec::with_capacity(threads);
    let mut replaced: Vec<usize> = Vec::new();
    for r in results {
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

    // Walk to the end of the chunk (and finish the element containing `to`).
    let end = if to + BLOCK > bytes.len() {
        bytes.len()
    } else {
        to + BLOCK
    };
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
                if run.is_multiple_of(2) {
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
            if std::env::var("CHPERF_DEBUG").is_ok() {
                eprintln!("  [dbg] slice [{s}..{e}): {}", String::from_utf8_lossy(&bytes[s..e]));
            }
            format!("{}", err)
        })?;
    Ok((events, replaced))
}


pub fn parse_trace(path: &Path) -> Result<TraceFile, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    // Decompress (if .gz) and parse from an in-memory slice. This is far
    // faster than streaming serde_json through the gzip decoder: zlib-rs
    // inflates at ~1.5GB/s, and `from_slice` skips reader indirection.
    let mut bytes = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        let mut out = Vec::new();
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
        let t_par = std::time::Instant::now();
        if debug {
            eprintln!(
                "  [parse] threads: {}",
                parse_thread_count(&(layout.arr_close - layout.arr_open))
            );
        }
        match parse_parallel(&mut bytes, &layout, &blocks) {
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
    } else if debug {
        eprintln!("  [parse] scan returned None, falling back");
    }
    // Fallback: whole-buffer parse (unusual layout, non-object elements, …).
    // Note: must NOT use unwrap_or — its argument is evaluated eagerly and
    // would re-parse the comma-mutated buffer on the happy path.
    let mut trace = match trace {
        Some(t) => t,
        None => serde_json::from_slice(&bytes)?,
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
