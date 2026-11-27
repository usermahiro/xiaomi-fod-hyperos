//! Lock the panel to 120 fps while a FOD press is in flight.
//!
//! Flow:
//!   pointer_down -> request_lock() (async, non-blocking)
//!                -> wait_locked()  (bounded wait, right before HBM turns on)
//!   pointer_up   -> release()      (restore the user's refresh settings
//!                                   RESTORE_DELAY_MS after the finger left;
//!                                   a new press inside that window cancels it)
//!
//! Mechanism: Settings.System min_refresh_rate + peak_refresh_rate, which
//! DisplayModeDirector honours as a hard floor/ceiling. All binder/process
//! work happens on one worker thread so the HAL path never blocks on it.
use crate::{log_info, log_warn};
use parking_lot::Mutex;
use std::fs;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const TARGET_FPS: &str = "120.0";
const RESTORE_DELAY: Duration = Duration::from_millis(3_000);
/// Max time on_pointer_down waits for the lock to land before lighting HBM.
pub const LOCK_WAIT_MS: u64 = 60;

const CMD: &str = "/system/bin/cmd";
const SH: &str = "/system/bin/sh";
const KEY_MIN: &str = "min_refresh_rate";
const KEY_PEAK: &str = "peak_refresh_rate";
/// Survives a HAL crash/restart so we never leave the device pinned at 120.
const SAVED_FILE: &str = "/data/vendor_de/0/fpdata/refresh_orig";

enum Msg {
    Lock,
    Release,
}

#[derive(Clone)]
struct Orig {
    min: Option<String>,
    peak: Option<String>,
}

static TX: OnceLock<Mutex<Sender<Msg>>> = OnceLock::new();
static LOCKED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);

fn send(msg: Msg) {
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel();
        if let Err(e) = std::thread::Builder::new()
            .name("fod_refresh".to_string())
            .spawn(move || worker(rx))
        {
            log_warn!("refresh: failed to spawn worker: {e}");
        }
        Mutex::new(tx)
    });
    let _ = tx.lock().send(msg);
}

pub fn request_lock() {
    send(Msg::Lock);
}

pub fn release() {
    send(Msg::Release);
}

/// Wait (bounded) until the lock has been applied. Returns true if locked.
pub fn wait_locked(max_wait_ms: u64) -> bool {
    if LOCKED.load(Ordering::SeqCst) {
        return true;
    }
    if FAILED.load(Ordering::SeqCst) {
        return false;
    }
    let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
    while Instant::now() < deadline {
        if LOCKED.load(Ordering::SeqCst) {
            return true;
        }
        if FAILED.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    LOCKED.load(Ordering::SeqCst)
}

fn parse_val(s: &str) -> Option<String> {
    match s.trim() {
        "" | "null" => None,
        v => Some(v.to_string()),
    }
}

fn load_saved() -> Option<Orig> {
    let raw = fs::read_to_string(SAVED_FILE).ok()?;
    let mut it = raw.split_whitespace();
    Some(Orig {
        min: parse_val(it.next()?),
        peak: parse_val(it.next()?),
    })
}

fn store_saved(o: &Orig) {
    let line = format!(
        "{} {}\n",
        o.min.as_deref().unwrap_or("null"),
        o.peak.as_deref().unwrap_or("null")
    );
    let _ = fs::write(SAVED_FILE, line);
}

fn run_sh(script: &str) -> Option<String> {
    let out = Command::new(SH)
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn apply_lock(saved: &mut Option<Orig>) -> bool {
    let recovered = if saved.is_none() { load_saved() } else { None };
    let need_capture = saved.is_none() && recovered.is_none();
    if recovered.is_some() {
        *saved = recovered;
    }

    let puts = format!(
        "{c} settings put system {b} {t} & {c} settings put system {a} {t} & wait",
        c = CMD, a = KEY_MIN, b = KEY_PEAK, t = TARGET_FPS
    );
    let script = if need_capture {
        format!(
            "echo \"$({c} settings get system {a}) $({c} settings get system {b})\"; {p}",
            c = CMD, a = KEY_MIN, b = KEY_PEAK, p = puts
        )
    } else {
        puts
    };

    let Some(out) = run_sh(&script) else {
        log_warn!("refresh: cannot exec {} (SELinux?)", SH);
        return false;
    };

    if need_capture {
        let mut it = out.lines().next().unwrap_or("").split_whitespace();
        let orig = Orig {
            min: it.next().and_then(parse_val),
            peak: it.next().and_then(parse_val),
        };
        if it.next().is_none() && orig.min.is_none() && orig.peak.is_none() && out.trim().is_empty() {
            log_warn!("refresh: `cmd settings` returned nothing (SELinux denial on cmd/settings_service?)");
            return false;
        }
        store_saved(&orig);
        log_info!(
            "refresh: locked {} fps (orig min={:?} peak={:?})",
            TARGET_FPS, orig.min, orig.peak
        );
        *saved = Some(orig);
    } else {
        log_info!("refresh: locked {} fps (orig restored from file)", TARGET_FPS);
    }
    true
}

fn restore(o: &Orig) {
    let mut parts = Vec::new();
    for (key, val) in [(KEY_MIN, &o.min), (KEY_PEAK, &o.peak)] {
        parts.push(match val {
            Some(v) => format!("{CMD} settings put system {key} {v} &"),
            None => format!("{CMD} settings delete system {key} &"),
        });
    }
    parts.push("wait".to_string());
    let _ = run_sh(&parts.join(" "));
    let _ = fs::remove_file(SAVED_FILE);
    log_info!("refresh: restored min={:?} peak={:?}", o.min, o.peak);
}

fn worker(rx: Receiver<Msg>) {
    let mut saved: Option<Orig> = None;
    let mut deadline: Option<Instant> = None;

    // A previous instance may have died while locked.
    if let Some(o) = load_saved() {
        restore(&o);
    }

    loop {
        let msg = match deadline {
            Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            },
        };

        match msg {
            Some(Msg::Lock) => {
                deadline = None; // new press cancels the pending restore
                if !LOCKED.load(Ordering::SeqCst) {
                    if apply_lock(&mut saved) {
                        FAILED.store(false, Ordering::SeqCst);
                        LOCKED.store(true, Ordering::SeqCst);
                    } else {
                        FAILED.store(true, Ordering::SeqCst);
                    }
                }
            }
            Some(Msg::Release) => {
                if LOCKED.load(Ordering::SeqCst) {
                    deadline = Some(Instant::now() + RESTORE_DELAY);
                }
                FAILED.store(false, Ordering::SeqCst);
            }
            None => {
                deadline = None;
                if let Some(o) = saved.take() {
                    restore(&o);
                }
                LOCKED.store(false, Ordering::SeqCst);
            }
        }
    }
}
