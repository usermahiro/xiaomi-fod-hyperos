use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use crate::{log_info, log_warn};

const TRAN_LCM_HBM_STATE: &str = "/sys/kernel/tran_display/lcm_hbm_state";
const OPLUS_DISPLAY_DIR: &str = "/data/addon/oplus_display";
const OPLUS_DISPLAY_HBM: &str = "/data/addon/oplus_display/hbm";
const OPLUS_DISPLAY_FPPRESS: &str = "/data/addon/oplus_display/notify_fppress";

const HBM_SAFETY_TIMEOUT_MS: u64 = 3_000;
struct HbmController {
    is_enabled: bool,
}

static GLOBAL_HBM: Mutex<Option<HbmController>> = Mutex::new(None);
static HBM_GENERATION: AtomicU64 = AtomicU64::new(0);

fn read_tran() -> Option<i32> {
    let raw = fs::read_to_string(TRAN_LCM_HBM_STATE).ok()?;
    let t = raw.trim();
    match t {
        "0" => Some(0),
        "1" => Some(1),
        _ => None,
    }
}

impl HbmController {
    fn new() -> Self {
        Self { is_enabled: false }
    }

    fn write_node(&self, enabled: bool) {
        let val = if enabled { b"1\n" } else { b"0\n" };
        if let Err(e) = fs::write(TRAN_LCM_HBM_STATE, val) {
            log_warn!("Failed to write to {}: {}", TRAN_LCM_HBM_STATE, e);
            return;
        }
        if enabled {
            std::thread::sleep(Duration::from_millis(30));
            let _ = fs::write(TRAN_LCM_HBM_STATE, val);
        }
    }

    fn write_addon(&self, enabled: bool) {
        let _ = fs::create_dir_all(OPLUS_DISPLAY_DIR);
        let value = if enabled { b"1\n" } else { b"0\n" };
        if enabled {
            let _ = fs::write(OPLUS_DISPLAY_FPPRESS, value);
            let _ = fs::write(OPLUS_DISPLAY_HBM, value);
        } else {
            let _ = fs::write(OPLUS_DISPLAY_HBM, value);
            let _ = fs::write(OPLUS_DISPLAY_FPPRESS, value);
        }
    }

    fn write(&mut self, enabled: bool) {
        let generation = HBM_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        if enabled {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(HBM_SAFETY_TIMEOUT_MS));
                force_off_if_generation(generation);
            });
        }

        self.write_node(enabled);

        self.write_addon(enabled);

        let changed = self.is_enabled != enabled;
        self.is_enabled = enabled;
        if changed {
            if enabled {
                log_info!("Hardware LCM HBM display enabled");
            } else {
                log_info!("Hardware LCM HBM display disabled");
            }
        }
    }
}

fn force_off_if_generation(expected: u64) {
    let mut lock = GLOBAL_HBM.lock();
    if HBM_GENERATION.load(Ordering::SeqCst) != expected {
        return;
    }
    HBM_GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Some(ctrl) = lock.as_mut() {
        log_warn!(
            "HBM safety watchdog: forced off after {}ms without pointer-up",
            HBM_SAFETY_TIMEOUT_MS
        );
        ctrl.write(false);
    }
}

pub fn is_enabled() -> bool {
    GLOBAL_HBM.lock().as_ref().map_or(false, |c| c.is_enabled)
}

pub fn set_hbm(enabled: bool) -> bool {
    set_hbm_sync(enabled, 0)
}

pub fn set_hbm_sync(enabled: bool, max_wait_ms: u64) -> bool {
    {
        let mut lock = GLOBAL_HBM.lock();
        if lock.is_none() {
            *lock = Some(HbmController::new());
        }
        match lock.as_mut() {
            Some(ctrl) => ctrl.write(enabled),
            None => return false,
        }
    }

    if !enabled || max_wait_ms == 0 {
        return true;
    }

    let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
    loop {
        match read_tran() {
            Some(1) => return true,
            None => return true,
            _ => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
