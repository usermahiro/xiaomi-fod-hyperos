use crate::{log_info, log_warn};
use std::ffi::CString;
use std::time::Duration;

#[cfg(target_os = "android")]
extern "C" {
    fn __system_property_get(name: *const libc::c_char, value: *mut libc::c_char) -> libc::c_int;
    fn __system_property_set(name: *const libc::c_char, value: *const libc::c_char) -> libc::c_int;
}

#[cfg(not(target_os = "android"))]
unsafe fn __system_property_get(_name: *const libc::c_char, _value: *mut libc::c_char) -> libc::c_int {
    0
}

#[cfg(not(target_os = "android"))]
unsafe fn __system_property_set(_name: *const libc::c_char, _value: *const libc::c_char) -> libc::c_int {
    0
}

const ENFORCED: &[(&str, &str)] = &[
    ("persist.vendor.fingerprint.sensor_type", "optical"),
    ("vendor.fingerprint.aidl.support", "1"),
];

const DEFAULTS: &[(&str, &str)] = &[
    ("persist.vendor.fingerprint.optical.iconsize", "200"),
    ("persist.vendor.fingerprint.optical.iconlocation", "228"),
    ("persist.vendor.fingerprint.optical.sensorlocation", "540::2172"),
    ("persist.vendor.fingerprint.optical.iconnumber", "5"),
];

const GUARD_INTERVAL: Duration = Duration::from_secs(5);

pub fn get(key: &str) -> String {
    let Ok(c_key) = CString::new(key) else {
        return String::new();
    };
    let mut value = [0u8; 92];
    let len = unsafe {
        __system_property_get(
            c_key.as_ptr(),
            value.as_mut_ptr() as *mut libc::c_char,
        )
    };
    if len <= 0 {
        return String::new();
    }
    let len = (len as usize).min(value.len() - 1);
    String::from_utf8_lossy(&value[..len]).into_owned()
}

fn set(key: &str, value: &str) -> bool {
    let (Ok(c_key), Ok(c_value)) = (CString::new(key), CString::new(value)) else {
        return false;
    };
    unsafe { __system_property_set(c_key.as_ptr(), c_value.as_ptr()) == 0 }
}

pub fn enforce() {
    for (key, want) in ENFORCED {
        if get(key) == *want {
            continue;
        }
        if set(key, want) {
            log_info!("props: {} = {}", key, want);
        } else {
            log_warn!("props: cannot write {} = {} (SELinux?)", key, want);
        }
    }

    for (key, want) in DEFAULTS {
        if !get(key).is_empty() {
            continue;
        }
        if set(key, want) {
            log_info!("props: {} empty, set to {}", key, want);
        } else {
            log_warn!("props: cannot fill {} = {} (SELinux?)", key, want);
        }
    }
}

pub fn spawn_guard() {
    std::thread::spawn(|| loop {
        std::thread::sleep(GUARD_INTERVAL);
        enforce();
    });
}
