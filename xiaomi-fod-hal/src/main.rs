#![allow(non_snake_case)]

//! xiaomi_fod_hal - Xiaomi FOD HAL shim for HyperOS/MIUI ports on MTK.
//!
//! Services:
//!   vendor.xiaomi.hardware.fingerprintextension.IXiaomiFingerprint/default
//!   vendor.xiaomi.hw.touchfeature.ITouchFeature/default
//!
//! Policy (ported from usermahiro/Oplus-UDFPS-Bridge-Transsion):
//!   - HBM is only lit while a fingerprint session is active AND a finger is
//!     physically down on the FOD area.
//!   - Finger up / session end forces HBM off (with retries against re-latch).
//!   - FOD gesture nodes are enabled only for the duration of a session.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use rsbinder::Interface;

include!(concat!(env!("OUT_DIR"), "/fod.rs"));

use crate::vendor::xiaomi::hardware::fingerprintextension::IXiaomiFingerprint::{
    BnXiaomiFingerprint, IXiaomiFingerprint,
};
use crate::vendor::xiaomi::hw::touchfeature::ITouchFeature::{BnTouchFeature, ITouchFeature};
use crate::vendor::xiaomi::hw::touchfeature::ITouchFeatureCallback::ITouchFeatureCallback;

type BinderResult<T> = rsbinder::status::Result<T>;

const FP_SERVICE: &str =
    "vendor.xiaomi.hardware.fingerprintextension.IXiaomiFingerprint/default";
const TOUCH_SERVICE: &str = "vendor.xiaomi.hw.touchfeature.ITouchFeature/default";
const HBM_NODE: &str = "/sys/kernel/tran_display/lcm_hbm_state";
const TOUCH_NODE: &str = "/dev/input/event4";

static HBM_ON: AtomicBool = AtomicBool::new(false);
static FINGER_DOWN: AtomicBool = AtomicBool::new(false);
static SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);
static GESTURE_ON: AtomicBool = AtomicBool::new(false);
static LAST_DOWN_MS: AtomicU64 = AtomicU64::new(0);
static LAST_FOD_KEY_MS: AtomicU64 = AtomicU64::new(0);
static TOUCH_X: AtomicI64 = AtomicI64::new(-1);
static TOUCH_Y: AtomicI64 = AtomicI64::new(-1);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------------- HBM I/O

fn hbm_read() -> String {
    std::fs::read_to_string(HBM_NODE)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<missing>".into())
}

fn write_hbm(on: bool, reason: &str) {
    let mut file = match OpenOptions::new().write(true).truncate(true).open(HBM_NODE) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("[HBM] cannot open {HBM_NODE}: {e}");
            HBM_ON.store(false, Ordering::SeqCst);
            return;
        }
    };
    if file.write_all(if on { b"1\n" } else { b"0\n" }).is_err() {
        HBM_ON.store(false, Ordering::SeqCst);
        return;
    }
    if on {
        // Some panels need a second write right after the boost starts.
        std::thread::sleep(Duration::from_millis(30));
        let _ = file.write_all(b"1\n");
    } else {
        // Vendor HAL can re-latch HBM while finishing a capture.
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(10));
            let _ = file.write_all(b"0\n");
        }
    }
    HBM_ON.store(on, Ordering::SeqCst);
    tracing::info!("[HBM] {} ({}) node={}", if on { "ON" } else { "OFF" }, reason, hbm_read());
}

fn arm(reason: &str) {
    LAST_DOWN_MS.store(now_ms(), Ordering::SeqCst);
    if !HBM_ON.load(Ordering::SeqCst) {
        write_hbm(true, reason);
    }
}

fn disarm(reason: &str) {
    if HBM_ON.load(Ordering::SeqCst) {
        write_hbm(false, reason);
    }
}

fn spawn_idle_guard(hold_ms: u64) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        if HBM_ON.load(Ordering::SeqCst)
            && (!FINGER_DOWN.load(Ordering::SeqCst)
                || now_ms().saturating_sub(LAST_DOWN_MS.load(Ordering::SeqCst)) > hold_ms)
        {
            disarm("idle-guard");
        }
    });
}

// ---------------------------------------------------------- gesture / FOD key

fn write_node(path: &str, val: &[u8]) {
    if let Err(e) = std::fs::write(path, val) {
        tracing::warn!("[Gesture] write {path}: {e}");
    }
}

fn set_gesture_mode(on: bool) {
    if GESTURE_ON.swap(on, Ordering::SeqCst) == on {
        return;
    }
    if on {
        write_node("/proc/fingerprint_status", b"1");
        write_node("/proc/gesture_function", b"fp1");
        write_node("/sys/class/input/event4/device/device/fts_gesture_mode", b"1");
        tracing::info!("[Gesture] FOD mode enabled");
    } else {
        write_node("/proc/fingerprint_status", b"0");
        write_node("/sys/class/input/event4/device/device/fts_gesture_mode", b"0");
        tracing::info!("[Gesture] FOD mode disabled");
    }
}

/// KEY 0xC3 is the trigger the Goodix HAL actually watches for capture.
#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

fn inject_fod_key(value: i32) {
    if now_ms().saturating_sub(LAST_FOD_KEY_MS.load(Ordering::SeqCst)) < 250 {
        return; // kernel already emitted it
    }
    let mut dev = match OpenOptions::new().write(true).open(TOUCH_NODE) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("[Input] cannot inject FOD key: {e}");
            return;
        }
    };
    let ev = [
        InputEvent { time: libc::timeval { tv_sec: 0, tv_usec: 0 }, type_: 0x01, code: 0xc3, value },
        InputEvent { time: libc::timeval { tv_sec: 0, tv_usec: 0 }, type_: 0x00, code: 0x00, value: 0 },
    ];
    let n = std::mem::size_of::<InputEvent>() * ev.len();
    let r = unsafe { libc::write(dev.as_raw_fd(), ev.as_ptr() as *const libc::c_void, n) };
    if r < 0 {
        tracing::warn!("[Input] FOD key write failed");
    } else {
        tracing::info!("[Input] injected FOD key {}", value);
    }
    let _ = dev.flush();
}

// ------------------------------------------------------------------- session

fn set_session(active: bool, reason: &str) {
    if SESSION_ACTIVE.swap(active, Ordering::SeqCst) == active {
        return;
    }
    tracing::info!("[Session] {} ({})", if active { "START" } else { "END" }, reason);
    set_gesture_mode(active);
    if !active {
        FINGER_DOWN.store(false, Ordering::SeqCst);
        disarm("session-end");
    }
}

/// Follow framework fingerprint session boundaries from logcat, same idea as
/// fodbridge.cpp: MIUI never calls extCmd() on this port, so the HAL's own log
/// is the only reliable session signal.
fn spawn_session_watcher() {
    std::thread::spawn(move || loop {
        let child = Command::new("/system/bin/logcat")
            .args([
                "-T", "1", "-v", "brief",
                "FodFingerprintServiceStubImpl:I",
                "BiometricScheduler:I",
                "android.hardware.biometrics.fingerprint-service:I",
                "FingerprintManager:I",
                "*:S",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[Session] cannot start logcat: {e}");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let start = line.contains("CMD_APP_ENROLL")
                    || line.contains("CMD_APP_AUTHEN")
                    || line.contains("CMD_KEYGUARD_DETECT")
                    || (line.contains("[Started]")
                        && (line.contains("FingerprintEnrollClient")
                            || line.contains("FingerprintAuthenticationClient")));
                let end = line.contains("onError")
                    || line.contains("[Finished / destroy]")
                    || line.contains("Client finished")
                    || line.contains("CMD_APP_CANCEL")
                    || line.contains("CMD_KEYGUARD_CANCEL_DETECT")
                    || line.contains("onAuthenticated");
                if start {
                    set_session(true, "logcat-start");
                } else if end {
                    set_session(false, "logcat-end");
                }
            }
        }
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(500));
    });
}

// -------------------------------------------------------------- touch watcher

fn in_fod_area(x_raw: i64, y_raw: i64) -> bool {
    // mtk-tpd reports 0..17279 / 0..38399 = 16x the 1080x2400 panel.
    let dx = x_raw / 16 - 540;
    let dy = y_raw / 16 - 2178;
    (dx * dx + dy * dy) <= 140 * 140
}

fn spawn_input_watcher() {
    std::thread::spawn(move || loop {
        let child = Command::new("/system/bin/getevent")
            .args(["-q", TOUCH_NODE])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[Input] cannot start getevent: {e}");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let p: Vec<&str> = line.split_whitespace().collect();
                if p.len() < 3 {
                    continue;
                }
                let ty = u32::from_str_radix(p[0], 16).unwrap_or(0);
                let code = u32::from_str_radix(p[1], 16).unwrap_or(0);
                let val = u32::from_str_radix(p[2], 16).unwrap_or(0) as i32 as i64;
                if ty == 0x03 {
                    match code {
                        0x35 => TOUCH_X.store(val, Ordering::SeqCst),
                        0x36 => TOUCH_Y.store(val, Ordering::SeqCst),
                        0x39 if val < 0 && FINGER_DOWN.swap(false, Ordering::SeqCst) => {
                            tracing::info!("[Input] finger lifted");
                            inject_fod_key(0);
                            disarm("finger-up");
                        }
                        _ => {}
                    }
                } else if ty == 0x01 && (code == 0xc3 || code == 0x14a || code == 0x152) {
                    if code == 0xc3 {
                        LAST_FOD_KEY_MS.store(now_ms(), Ordering::SeqCst);
                    }
                    let x = TOUCH_X.load(Ordering::SeqCst);
                    let y = TOUCH_Y.load(Ordering::SeqCst);
                    let hit = code == 0xc3 || code == 0x152 || in_fod_area(x, y);
                    if !hit {
                        continue;
                    }
                    if val != 0 {
                        // No session -> never light the panel (anti stuck-HBM).
                        if !SESSION_ACTIVE.load(Ordering::SeqCst) {
                            continue;
                        }
                        if !FINGER_DOWN.swap(true, Ordering::SeqCst) {
                            tracing::info!("[Input] FOD down x={x} y={y}");
                            inject_fod_key(1);
                            arm("finger-down");
                        }
                    } else if FINGER_DOWN.swap(false, Ordering::SeqCst) {
                        inject_fod_key(0);
                        disarm("finger-up");
                    }
                }
            }
        }
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(500));
    });
}

// -------------------------------------------------------------- AIDL service

struct FpExtHal;
impl Interface for FpExtHal {}

impl IXiaomiFingerprint for FpExtHal {
    fn extCmd(&self, cmd: i32, param1: i32) -> BinderResult<i32> {
        tracing::info!("[FP] extCmd({}, {})", cmd, param1);
        match cmd {
            4 => {
                if param1 == 1 || param1 == 3 || param1 == 5 {
                    set_session(true, &format!("extCmd-4-{param1}"));
                } else {
                    set_session(false, &format!("extCmd-4-{param1}"));
                }
            }
            7 => {
                if param1 == 0 {
                    disarm("fod-assist-off");
                }
            }
            _ => {}
        }
        Ok(0)
    }
}

struct TouchHal;
impl Interface for TouchHal {}

impl ITouchFeature for TouchHal {
    fn getModeCurValue(&self, _type_: i32, _mode: i32) -> BinderResult<i32> { Ok(0) }
    fn getModeDefaultValue(&self, _type_: i32, _mode: i32) -> BinderResult<i32> { Ok(0) }
    fn getModeMaxValue(&self, _type_: i32, _mode: i32) -> BinderResult<i32> { Ok(1) }
    fn getModeMinValue(&self, _type_: i32, _mode: i32) -> BinderResult<i32> { Ok(0) }
    fn getModeValue(&self, _type_: i32, _mode: i32) -> BinderResult<Vec<i32>> { Ok(vec![0]) }
    fn getTouchEvent(&self) -> BinderResult<String> { Ok(String::new()) }
    fn modeReset(&self, _type_: i32, _mode: i32) -> BinderResult<i32> { Ok(0) }
    fn setModeLongValue(&self, _type_: i32, _mode: i32, _len: i32, _values: &[i32]) -> BinderResult<i32> { Ok(0) }
    fn setModeValue(&self, type_: i32, mode: i32, value: i32) -> BinderResult<i32> {
        tracing::info!("[Touch] setModeValue(type={type_}, mode={mode}, value={value})");
        if type_ == 0 && mode == 10 {
            if value != 0 {
                set_session(true, "touch-monitor-on");
            } else {
                set_session(false, "touch-monitor-off");
            }
        }
        Ok(0)
    }
    fn getModeCurValueString(&self, _type_: i32, _mode: i32) -> BinderResult<String> { Ok(String::new()) }
    fn getModeWhiteList(&self, _type_: i32, _modes: &[i32]) -> BinderResult<String> { Ok(String::new()) }
    fn registerCallback(
        &self, _type_: i32, _callback: &rsbinder::Strong<dyn ITouchFeatureCallback>, _name: &str,
    ) -> BinderResult<i32> { Ok(0) }
    fn unregisterCallback(
        &self, _type_: i32, _callback: &rsbinder::Strong<dyn ITouchFeatureCallback>, _name: &str,
    ) -> BinderResult<i32> { Ok(0) }
    fn setModePackageName(&self, _type_: i32, _mode: i32, _packageName: &str) -> BinderResult<String> { Ok(String::new()) }
    fn setTouchMode(&self, touchId: i32, mode: i32, value: i32) -> BinderResult<()> {
        tracing::info!("[Touch] setTouchMode(id={touchId}, mode={mode}, value={value})");
        Ok(())
    }
}

// -------------------------------------------------------------------- props

fn prop_get(name: &str) -> String {
    Command::new("/system/bin/getprop")
        .arg(name)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn prop_set(name: &str, value: &str) {
    if !prop_get(name).is_empty() {
        return;
    }
    let _ = Command::new("/system/bin/setprop").args([name, value]).status();
}

fn setup_props() {
    prop_set("ro.hardware.fp.fod", "1");
    prop_set("ro.hardware.fp.fod.c", "1");
    prop_set("ro.hardware.fp.fod.location", "low");
    prop_set("ro.vendor.localhbm.enable", "1");
    prop_set("persist.vendor.sys.fp.fod.location.X_Y", "540,2178");
    prop_set("persist.vendor.sys.fp.fod.size.width_height", "220,220");
    prop_set("persist.vendor.sys.fp.vendor", "goodix");
    prop_set("persist.vendor.fingerprint.sensor_type", "optical");
    prop_set("vendor.fingerprint.aidl.support", "1");
}

// --------------------------------------------------------------------- main

fn usage() -> ! {
    eprintln!(
        "xiaomi_fod_hal {}\n\
         usage:\n\
           xiaomi_fod_hal [--daemon] [--timeout MS]\n\
           xiaomi_fod_hal --hbm on|off\n\
           xiaomi_fod_hal --props | --status",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

fn main() -> Result<(), anyhow::Error> {
    let _ = tracing_log::LogTracer::init();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .try_init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode = "daemon";
    let mut hold_ms: u64 = 5_000;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--daemon" => mode = "daemon",
            "--timeout" => {
                i += 1;
                hold_ms = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(5_000);
            }
            "--hbm" => { i += 1; mode = "hbm"; }
            "--props" => mode = "props",
            "--status" => mode = "status",
            "-h" | "--help" => usage(),
            other => { eprintln!("unknown argument: {other}"); usage(); }
        }
        i += 1;
    }

    match mode {
        "hbm" => {
            write_hbm(args.iter().any(|a| a == "on"), "cli");
            return Ok(());
        }
        "props" => { setup_props(); return Ok(()); }
        "status" => {
            println!("HBM node : {} = {}", HBM_NODE, hbm_read());
            println!("HBM      : {}", HBM_ON.load(Ordering::SeqCst));
            println!("session  : {}", SESSION_ACTIVE.load(Ordering::SeqCst));
            println!("finger   : {}", FINGER_DOWN.load(Ordering::SeqCst));
            for p in [
                "ro.hardware.fp.fod", "ro.hardware.fp.fod.c", "ro.hardware.fp.fod.location",
                "ro.vendor.localhbm.enable", "persist.vendor.sys.fp.vendor",
                "persist.vendor.sys.fp.fod.location.X_Y",
            ] {
                println!("{p} = {}", prop_get(p));
            }
            return Ok(());
        }
        _ => {}
    }

    setup_props();
    write_hbm(false, "startup");
    spawn_idle_guard(hold_ms);
    spawn_session_watcher();
    spawn_input_watcher();

    rsbinder::ProcessState::init_default()
        .map_err(|e| anyhow!("ProcessState::init_default: {e}"))?;
    rsbinder::ProcessState::start_thread_pool();

    let fp = BnXiaomiFingerprint::new_binder(FpExtHal);
    rsbinder::hub::add_service(FP_SERVICE, fp.as_binder())
        .map_err(|e| anyhow!("register {FP_SERVICE}: {e}"))?;
    tracing::info!("[HAL] registered {FP_SERVICE}");

    let touch = BnTouchFeature::new_binder(TouchHal);
    rsbinder::hub::add_service(TOUCH_SERVICE, touch.as_binder())
        .map_err(|e| anyhow!("register {TOUCH_SERVICE}: {e}"))?;
    tracing::info!("[HAL] registered {TOUCH_SERVICE}");

    rsbinder::ProcessState::join_thread_pool()?;
    Ok(())
}
