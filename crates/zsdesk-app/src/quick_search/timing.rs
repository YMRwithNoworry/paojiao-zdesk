//! Optional compositor timing for diagnostics. Query strings and file paths are never recorded.
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::Instant,
};

#[derive(Default)]
struct Clock {
    marker: std::path::PathBuf,
    stopped: bool,
    wake: Option<Instant>,
    input: Option<(Instant, &'static str, usize)>,
    ready: Option<(u64, usize, Instant)>,
    prior_rows: usize,
    blank_paints: u32,
    hidden_transitions: u32,
    samples: VecDeque<Value>,
}
static CLOCK: OnceLock<Option<Mutex<Clock>>> = OnceLock::new();
fn clock() -> Option<&'static Mutex<Clock>> {
    CLOCK
        .get_or_init(|| {
            let marker = std::path::PathBuf::from(std::env::var_os("ZSDESK_QUICK_SEARCH_TIMING")?);
            (marker.is_absolute() && marker.is_file()).then(|| {
                Mutex::new(Clock {
                    marker,
                    ..Clock::default()
                })
            })
        })
        .as_ref()
}
pub fn wake() {
    if let Some(c) = clock() {
        let mut c = c.lock().unwrap_or_else(|e| e.into_inner());
        c.wake = Some(Instant::now());
        c.input = None;
        c.ready = None;
    }
}
pub fn action(kind: &'static str, filter: usize, prior_rows: usize) {
    if let Some(c) = clock() {
        let mut c = c.lock().unwrap_or_else(|e| e.into_inner());
        if c.stopped {
            return;
        }
        if !c.marker.is_file() {
            c.stopped = true;
            c.input = None;
            c.ready = None;
            return;
        }
        c.input = Some((Instant::now(), kind, filter));
        c.prior_rows = prior_rows;
        c.blank_paints = 0;
        c.hidden_transitions = 0;
        c.ready = None;
    }
}
pub fn ready(generation: u64, rows: usize) {
    if let Some(c) = clock() {
        let mut c = c.lock().unwrap_or_else(|e| e.into_inner());
        if c.input.is_some() && c.ready.is_none() {
            c.ready = Some((generation, rows, Instant::now()));
        }
    }
}
pub fn intermediate(hidden: bool, rows: impl FnOnce() -> usize) {
    if let Some(c) = clock() {
        let mut c = c.lock().unwrap_or_else(|e| e.into_inner());
        if c.input.is_some() && c.ready.is_none() && c.prior_rows > 0 {
            if hidden {
                c.hidden_transitions += 1;
            } else if rows() == 0 {
                c.blank_paints += 1;
            }
        }
    }
}
pub fn painted(empty: bool) {
    let Some(c) = clock() else {
        return;
    };
    let sample = {
        let mut c = c.lock().unwrap_or_else(|e| e.into_inner());
        if !c.ready.is_some_and(|(_, rows, _)| (rows == 0) == empty) {
            return;
        }
        let Some((generation, rows, ready)) = c.ready.take() else {
            return;
        };
        let Some((input, kind, filter)) = c.input.take() else {
            return;
        };
        (
            generation,
            rows,
            ready,
            input,
            kind,
            filter,
            c.wake,
            c.blank_paints,
            c.hidden_transitions,
        )
    };
    let (generation, rows, ready, input, kind, filter, wake, blank, hides) = sample;
    let painted = Instant::now();
    let composed = unsafe { windows::Win32::Graphics::Dwm::DwmFlush() }.is_ok();
    let now = Instant::now();
    let mut c = c.lock().unwrap_or_else(|e| e.into_inner());
    if c.samples.len() >= 48 {
        c.samples.pop_front();
    }
    c.samples.push_back(json!({"empty_list_paints_while_waiting":blank,"list_hides_while_waiting":hides,"action":kind,"filter":filter,"generation":generation,"rows":rows,
        "ready_ms":ready.duration_since(input).as_millis(),"paint_ms":painted.duration_since(input).as_millis(),
        "composition_ms":composed.then(||now.duration_since(input).as_millis()),
        "wake_to_input_ms":wake.map(|w|input.saturating_duration_since(w).as_millis()),
        "wake_to_composition_ms":wake.filter(|_|composed).map(|w|now.duration_since(w).as_millis())}));
}
pub fn snapshot() -> Value {
    clock().map_or(Value::Null, |c| {
        json!(c.lock().unwrap_or_else(|e| e.into_inner()).samples)
    })
}
