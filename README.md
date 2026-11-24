# Xiaomi FOD / UDFPS bridge for HyperOS (MIUI) ports on MTK Transsion

Under-display fingerprint support for **Infinix X6886 (MT6789, `nt37706a` panel)**
running a HyperOS/MIUI port via DSU. The stock Transsion fingerprint HAL works,
but the ported MIUI framework cannot talk to it: it expects Xiaomi vendor
extension services and a different HBM/touch sequence.

Two small components, no framework / SystemUI patching:

| Component | Service | Role |
|---|---|---|
| `xiaomi-fod-hal/` | `vendor.xiaomi.hardware.fingerprintextension.IXiaomiFingerprint/default` + `vendor.xiaomi.hw.touchfeature.ITouchFeature/default` | Provides the vendor services MIUI expects, tracks FOD sessions, gates HBM to real finger-down, drives FOD gesture nodes. |
| `gf-fingerprint-wrapper/` | `android.hardware.biometrics.fingerprint.IFingerprint/default` | AIDL v3 wrapper around `libgf_hal.so` / `gf_fingerprint.default.so`. Fixes the Goodix ABI (notify payload without `gid`, batch `remove`, 0x130 device struct) and patches the library so enrolment does not wait forever for a kernel uevent. Adapted from [`Oplus-UDFPS-Bridge-Transsion`](https://github.com/usermahiro/Oplus-UDFPS-Bridge-Transsion). |

Both daemons are independent and can be tested from `/data/local/tmp` on a
rooted device. `/vendor` is erofs (read-only), so a permanent install needs an
image rebuild — rc/VINTF snippets are included in each component.

---

## Why MIUI showed no enrolment / no icon

`FingerprintServiceStubImpl.isFingerprintDetected()` returns `false` when
`persist.vendor.sys.fp.vendor` is empty. MIUI then hides the enrolment entry and
SystemUI never starts fingerprint detection, so the Gxzw (FOD) icon is never
shown. The shim sets the props below before SystemUI reads them:

```
ro.hardware.fp.fod=1
ro.hardware.fp.fod.c=1                         # CFOD
ro.hardware.fp.fod.location=low
ro.vendor.localhbm.enable=1                    # LHBM path
persist.vendor.sys.fp.vendor=goodix            # <-- the critical one
persist.vendor.fingerprint.sensor_type=optical
vendor.fingerprint.aidl.support=1
persist.vendor.sys.fp.fod.location.X_Y=430,2068  # top-left of 220x220 icon
persist.vendor.sys.fp.fod.size.width_height=220,220
```

Geometry follows the panel: mtk-tpd reports 16x panel coordinates
(`0..17279` / `0..38399` for 1080x2400), so the watcher divides by 16 before
testing the FOD circle centred at `(540, 2178)`.

## HBM policy (the "stuck HBM" fix)

HBM is **not** turned on by `extCmd(4,3)` or `setModeValue(10,1)` — MIUI sends
those as session bookkeeping and they can outlive the actual touch. Instead:

* HBM turns on only when `SESSION_ACTIVE` **and** a real finger is down on the
  FOD area (EV_KEY `0xC3` / `BTN_TOUCH` / MT coordinates inside the circle).
* Finger-up writes `0` **once**. No OPlus-style retry loop — the node has no
  boolean read-back, so retries only spam logs.
* `extCmd(4,0|4)`, `setModeValue(10,0)` and session end force it off.
* A 500 ms idle guard clears a stale `1` when no finger is down and no fresh
  press came in within the hold window.
* One press = one capture attempt. No hold-re-arm / duplicate auth.
* Screen state is derived from DRM DPMS, not ColorOS custom cmd 1011 (MIUI never sends it). SOFOD works after the panel enters deep sleep.
* Re-issue one KEY 0xC3 DOWN after HBM is lit: Goodix only captures on that key, and in deep sleep the kernel FOD key fires before a session exists. Single refire, 350 ms hold — no repeated re-arm.
* `authenticate()` no longer clears HBM (MIUI may call it after the touch already lit the panel).
* Startup always clears HBM.

Node: `/sys/kernel/tran_display/lcm_hbm_state`. Reading it returns a hex
register value (e.g. `0xFA`), **not** `"1"`/`"0xOK"` — do not poll it as a
boolean.

## Enrolment fix (the wrapper)

`[GF_HAL][FingerprintCore] notifyErrorInfo err code : 5` came from libgf_hal
waiting on a netlink uevent that never arrives on this ROM. `patch_libgf_hal.py`
replaces `goodix::tran_uevent_handle()` with a fixed `usleep()` + `return 1`,
NOPs the `uiReadyStatus` gate in `gfHbmEventDetectingThread()`, and relaxes the
`Device::enable()` ioctl-init check. It verifies the original instructions
before writing and is idempotent.

The patched library keeps SONAME `libgf_hal.so`, so preloading it with
`RTLD_GLOBAL` makes `gf_fingerprint.default.so` (DT_NEEDED `libgf_hal.so`)
resolve to the patched copy without touching read-only `/odm`.

---

## Build

NDK r27c, Rust target `aarch64-linux-android`:

```bash
export ANDROID_NDK_HOME=/path/to/android-ndk-r27c
export CARGO_TARGET_DIR=/tmp/fod-build

(cd xiaomi-fod-hal && cargo build --target aarch64-linux-android --release)
(cd gf-fingerprint-wrapper && cargo build --target aarch64-linux-android --release)
```

## Runtime test (adb root)

```bash
# 1. patched Goodix library
./scripts/make-patched-libgf.sh

# 2. Xiaomi vendor-service shim
adb push xiaomi-fod-hal/target/aarch64-linux-android/release/xiaomi_fod_hal \
         /data/local/tmp/xiaomi_fod_hal
adb shell 'nohup /data/local/tmp/xiaomi_fod_hal > /data/local/tmp/fod_hal.log 2>&1 < /dev/null &'
adb shell /data/local/tmp/xiaomi_fod_hal --status

# 3. fingerprint HAL wrapper (stops the stock HAL first)
adb push gf-fingerprint-wrapper/target/aarch64-linux-android/release/\
android-hardware-biometrics-fingerprint-service-transsion /data/local/tmp/fp_bridge
adb shell setprop ctl.stop vendor.fingerprint-default
adb shell 'nohup /data/local/tmp/fp_bridge > /data/local/tmp/fp_bridge.out 2>&1 < /dev/null &'
adb shell 'service list | grep biometrics.fingerprint.IFingerprint'
```

Logs: `fod_hal.log` (file) and `adb logcat -s oplus_udfps_transsion` (wrapper).

To restore the stock HAL: `adb shell setprop ctl.start vendor.fingerprint-default`.

## Permanent install

Needs `/vendor` rebuilt/reflashed. Use each component's `.rc` and VINTF manifest:

* `xiaomi-fod-hal/init/xiaomi-fod-hal.rc`
* `gf-fingerprint-wrapper/android.hardware.biometrics.fingerprint-service.transsion.rc`
* `gf-fingerprint-wrapper/manifest_fingerprint_transsion.xml`

and disable the stock `fingerprint-default.rc` / `android.hardware.biometrics.fingerprint-service.rc`.
The patched `libgf_hal.so` must then live in `odm/lib64/` (or be patched at build
time by the wrapper's build script) — `/data/local/tmp` is only for testing.

## Known gaps

* FOD icon geometry / press animation on MIUI is driven by
  `MiuiGxzwUtils.caculateGxzwIconSize()` reading
  `persist.vendor.sys.fp.fod.location.X_Y` and `...size.width_height`; the
  press animation itself is SystemUI-side and still needs tuning on this port.
* Auto-brightness needs an ALS driver (`mn79911`) in the kernel — unrelated to FOD.

## Credits

`gf-fingerprint-wrapper/` is adapted from
[usermahiro/Oplus-UDFPS-Bridge-Transsion](https://github.com/usermahiro/Oplus-UDFPS-Bridge-Transsion)
under Apache-2.0.


## Screen-off state (verified)
`Reg(0xD0)` in `/sys/class/input/event4/device/device/fts_gesture_mode` is the
authoritative signal on this MTK/Transsion port (`0` = screen on, `1` = doze).
DRM DPMS stays "On" during doze, so it cannot be used. The wrapper derives
screen state from this node, so SOFOD responds immediately with no ColorOS
cmd 1011 and HBM never lights while the screen is on / while scrolling.

## Division of responsibility
- `fp_bridge` (AIDL IFingerprint wrapper) owns HBM and FOD key injection.
  One touch = one KEY 0xC3 capture, no OPlus hold-rearm / retry spam.
- `xiaomi_fod_hal` only exposes the MIUI vendor binder services
  (`IXiaomiFingerprint`, `ITouchFeature`). It does not drive HBM or inject keys;
  doing so caused HBM to light while scrolling with a stale session.

Verified: screen-off tap produces `Authentication SUCCEEDED`; screen-on tap
produces no HBM activity.

## FOD press animation (no SystemUI smali edits)
MIUI only starts the pressed-icon animation when the HAL reports a vendor
acquired event. The wrapper emits, from the bridge itself:

- `onAcquired(AcquiredInfo::VENDOR, 22)` on finger down
- `onAcquired(AcquiredInfo::VENDOR, 23)` on finger up

`FingerprintServiceStubImpl` maps these to `onAcquired(100,0)` / `(101,0)`,
which KeyguardUpdateMonitor forwards to `onFpsPointerDown()` /
`onFpsPointerUp()` -> `MiuiGxzwViewIcon.onTouchDown()` / `onTouchUp()`.
Sending only `AcquiredInfo::START` never triggered the animation.
No framework/SystemUI smali patching is required.
