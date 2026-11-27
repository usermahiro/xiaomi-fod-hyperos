#![allow(non_snake_case)]

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
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

/// FOD icon geometry, tuned on the Infinix X6886 panel. MIUI's
/// persist.vendor.sys.fp.fod.location.X_Y is the TOP-LEFT corner, not the
/// centre; keep the centre on the optical sensor reported by the stock ROM
/// (persist.vendor.fingerprint.optical.sensorlocation = 540::2172).
const FOD_CENTER_X: i64 = 540;
const FOD_CENTER_Y: i64 = 2172;
const FOD_ICON_SIZE: i64 = 220;

fn fod_top_left() -> (i64, i64) {
    (
        FOD_CENTER_X - FOD_ICON_SIZE / 2,
        FOD_CENTER_Y - FOD_ICON_SIZE / 2,
    )
}

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
    }
    HBM_ON.store(on, Ordering::SeqCst);
    tracing::info!("[HBM] {} ({}) node={}", if on { "ON" } else { "OFF" }, reason, hbm_read());
}

fn arm(reason: &str) {
    LAST_DOWN_MS.store(now_ms(), Ordering::SeqCst);
    tracing::debug!("[HBM] arm skipped ({reason}) - owned by fp_bridge");
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

/// The wrapper (fp_bridge) is the sole FOD-key injector; flip only for tests
/// without the wrapper running, otherwise the capture key is sent twice.
const INJECT_FOD_KEY: bool = false;

fn inject_fod_key(value: i32) {
    if !INJECT_FOD_KEY {
        return;
    }
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

/// fodbridge.cpp: MIUI never calls extCmd()
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

fn in_fod_area(x_raw: i64, y_raw: i64) -> bool {
    // mtk-tpd reports 0..17279 / 0..38399 = 16x the 1080x2400 panel.
    let dx = x_raw / 16 - FOD_CENTER_X;
    let dy = y_raw / 16 - FOD_CENTER_Y;
    let radius = FOD_ICON_SIZE / 2;
    (dx * dx + dy * dy) <= radius * radius
}

// ------------------------------------------------------------- FPS lock on FOD
//
// While a finger is on the sensor, force 120Hz so the capture is stable. Three
// seconds after the finger lifts, put every refresh-rate setting back to what
// it was before the press.

const FPS_FALLBACK_HZ: i32 = 120;
const FPS_RESTORE_DELAY: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
struct FpsSnapshot {
    peak: Option<String>,
    min: Option<String>,
    user_secure: Option<String>,
    user_system: Option<String>,
    miui: Option<String>,
    dfps: Option<String>,
}

static FPS_SNAPSHOT: Mutex<Option<FpsSnapshot>> = Mutex::new(None);
static FPS_GEN: AtomicU64 = AtomicU64::new(0);

fn prop_get_opt(key: &str) -> Option<String> {
    let out = Command::new("/system/bin/getprop").arg(key).output().ok()?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn settings_get(ns: &str, key: &str) -> Option<String> {
    let out = Command::new("/system/bin/settings")
        .args(["get", ns, key])
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() || value == "null" {
        None
    } else {
        Some(value)
    }
}

fn settings_put(ns: &str, key: &str, value: &str) {
    if let Err(e) = Command::new("/system/bin/settings")
        .args(["put", ns, key, value])
        .status()
    {
        tracing::warn!("[FPS] settings put {ns} {key}={value} failed: {e}");
    }
}

fn capture_fps() -> FpsSnapshot {
    FpsSnapshot {
        peak: settings_get("system", "peak_refresh_rate"),
        min: settings_get("system", "min_refresh_rate"),
        user_secure: settings_get("secure", "user_refresh_rate"),
        user_system: settings_get("system", "user_refresh_rate"),
        miui: settings_get("secure", "miui_refresh_rate"),
        dfps: prop_get_opt("persist.vendor.dfps.level"),
    }
}

fn parse_hz(value: Option<&str>) -> Option<i32> {
    let hz = value?.parse::<f64>().ok()?.round() as i32;
    if (60..=144).contains(&hz) {
        Some(hz)
    } else {
        None
    }
}

/// Which rate to pin while the sensor is pressed. Use the rate actually
/// selected by the user (120, 144, ...) so a 144Hz user does not get dropped
/// to 120Hz during the capture.
fn lock_hz(snap: &FpsSnapshot) -> i32 {
    parse_hz(snap.peak.as_deref())
        .or_else(|| parse_hz(snap.dfps.as_deref()))
        .or_else(|| parse_hz(snap.user_secure.as_deref()))
        .or_else(|| parse_hz(snap.user_system.as_deref()))
        .unwrap_or(FPS_FALLBACK_HZ)
}

fn fps_values(snap: &FpsSnapshot, lock: bool, hz: i32) -> (String, String) {
    let fallback = FPS_FALLBACK_HZ.to_string();
    let peak = if lock {
        hz.to_string()
    } else {
        snap.peak.clone().unwrap_or(fallback.clone())
    };
    let min = if lock {
        hz.to_string()
    } else {
        snap.min.clone().unwrap_or(fallback)
    };
    (peak, min)
}

fn write_fps(snap: &FpsSnapshot, lock: bool, hz: i32) {
    let (peak, min) = fps_values(snap, lock, hz);
    let (peak, min) = (peak.as_str(), min.as_str());
    settings_put("system", "peak_refresh_rate", peak);
    settings_put("system", "min_refresh_rate", min);
    let hz_str = hz.to_string();
    if lock {
        for (ns, key) in [
            ("secure", "user_refresh_rate"),
            ("system", "user_refresh_rate"),
            ("secure", "miui_refresh_rate"),
        ] {
            settings_put(ns, key, &hz_str);
        }
        let _ = Command::new("/system/bin/setprop")
            .args(["persist.vendor.dfps.level", &hz_str])
            .status();
    } else {
        if let Some(v) = &snap.user_secure {
            settings_put("secure", "user_refresh_rate", v);
        }
        if let Some(v) = &snap.user_system {
            settings_put("system", "user_refresh_rate", v);
        }
        if let Some(v) = &snap.miui {
            settings_put("secure", "miui_refresh_rate", v);
        }
        if let Some(v) = &snap.dfps {
            let _ = Command::new("/system/bin/setprop")
                .args(["persist.vendor.dfps.level", v])
                .status();
        }
    }
    tracing::info!(
        "[FPS] {} (peak={peak}, min={min}, hz={hz})",
        if lock { "lock" } else { "restore" }
    );
}

fn fps_lock() {
    if let Ok(mut guard) = FPS_SNAPSHOT.lock() {
        if guard.is_none() {
            *guard = Some(capture_fps());
        }
    }
    FPS_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(snap) = FPS_SNAPSHOT.lock().unwrap().as_ref() {
        write_fps(snap, true, lock_hz(snap));
    }
}

fn fps_release_after_delay() {
    let gen = FPS_GEN.load(Ordering::SeqCst);
    std::thread::spawn(move || {
        std::thread::sleep(FPS_RESTORE_DELAY);
        if gen != FPS_GEN.load(Ordering::SeqCst) {
            return;
        }
        if FINGER_DOWN.load(Ordering::SeqCst) {
            return;
        }
        let snap = FPS_SNAPSHOT.lock().unwrap().take();
        if let Some(snap) = snap {
            write_fps(&snap, false, 0);
        }
    });
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
                let mut p: Vec<&str> = line.split_whitespace().collect();
                if p.first().map_or(false, |t| t.ends_with(':')) {
                    p.remove(0);
                }
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
                            fps_release_after_delay();
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
                        // No session
                        if !SESSION_ACTIVE.load(Ordering::SeqCst) {
                            continue;
                        }
                        if !FINGER_DOWN.swap(true, Ordering::SeqCst) {
                            tracing::info!("[Input] FOD down x={x} y={y}");
                            inject_fod_key(1);
                            arm("finger-down");
                            fps_lock();
                        }
                    } else if FINGER_DOWN.swap(false, Ordering::SeqCst) {
                        inject_fod_key(0);
                        disarm("finger-up");
                        fps_release_after_delay();
                    }
                }
            }
        }
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(500));
    });
}

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

/// Geometry must win over a stale value from an older build, so always write.
fn prop_set_force(name: &str, value: &str) {
    let _ = Command::new("/system/bin/setprop").args([name, value]).status();
}

fn setup_props() {
    prop_set("ro.hardware.fp.fod", "1");
    prop_set("ro.hardware.fp.fod.c", "1");
    prop_set("ro.hardware.fp.fod.location", "low");
    prop_set("ro.vendor.localhbm.enable", "1");
    let (x, y) = fod_top_left();
    let size = FOD_ICON_SIZE.to_string();
    prop_set_force(
        "persist.vendor.sys.fp.fod.location.X_Y",
        &format!("{x},{y}"),
    );
    prop_set_force("persist.vendor.sys.fp.fod.size.width_height", &size);
    prop_set("persist.vendor.sys.fp.vendor", "goodix");
    prop_set("persist.vendor.fingerprint.sensor_type", "optical");
    prop_set("vendor.fingerprint.aidl.support", "1");
}

fn usage() -> ! {
    eprintln!(
        "xiaomi_fod_hal {}\n\
         usage:\n\
           xiaomi_fod_hal [--daemon] [--no-touch] [--timeout MS]\n\
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
    let mut no_touch = false;
    let mut hbm_on: Option<bool> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--daemon" => mode = "daemon",
            "--timeout" => {
                i += 1;
                hold_ms = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(5_000);
            }
            "--no-touch" => no_touch = true,
            "--hbm" => {
                i += 1;
                mode = "hbm";
                hbm_on = match args.get(i).map(String::as_str) {
                    Some("on") => Some(true),
                    Some("off") => Some(false),
                    _ => usage(),
                };
            }
            "--props" => mode = "props",
            "--status" => mode = "status",
            "-h" | "--help" => usage(),
            other => { eprintln!("unknown argument: {other}"); usage(); }
        }
        i += 1;
    }

    match mode {
        "hbm" => {
            write_hbm(hbm_on.unwrap_or(false), "cli");
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

    if !no_touch {
        let touch = BnTouchFeature::new_binder(TouchHal);
        rsbinder::hub::add_service(TOUCH_SERVICE, touch.as_binder())
            .map_err(|e| anyhow!("register {TOUCH_SERVICE}: {e}"))?;
        tracing::info!("[HAL] registered {TOUCH_SERVICE}");
    }

    rsbinder::ProcessState::join_thread_pool()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fod_icon_is_centered_on_the_sensor() {
        assert_eq!(fod_top_left(), (430, 2062));

        let cx = FOD_CENTER_X * 16;
        let cy = FOD_CENTER_Y * 16;
        assert!(in_fod_area(cx, cy));

        let edge = (FOD_ICON_SIZE / 2) * 16;
        assert!(in_fod_area(cx + edge, cy));
        assert!(!in_fod_area(cx + edge + 16, cy));
        assert!(!in_fod_area(cx + edge, cy + edge + 16));
    }

    #[test]
    fn fps_lock_uses_selected_rate_and_restore_uses_snapshot() {
        let snap = FpsSnapshot {
            peak: Some("144.0".into()),
            min: Some("60.0".into()),
            user_secure: Some("144".into()),
            user_system: Some("144".into()),
            miui: Some("144".into()),
            dfps: Some("144".into()),
        };
        assert_eq!(lock_hz(&snap), 144);
        assert_eq!(fps_values(&snap, true, 144), ("144".into(), "144".into()));
        assert_eq!(fps_values(&snap, false, 0), ("144.0".into(), "60.0".into()));

        let snap120 = FpsSnapshot {
            peak: Some("120.0".into()),
            ..snap.clone()
        };
        assert_eq!(lock_hz(&snap120), 120);
        assert_eq!(fps_values(&snap120, true, 120), ("120".into(), "120".into()));
        assert_eq!(FPS_RESTORE_DELAY, Duration::from_secs(3));
    }
}
