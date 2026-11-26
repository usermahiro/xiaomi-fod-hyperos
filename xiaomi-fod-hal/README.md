# xiaomi_fod_hal

Shim HAL FOD Xiaomi HyperOS in Transsion (MT6789).

Masalahnya: vendor Transsion cuma sediakan `android.hardware.biometrics.fingerprint.IFingerprint`
(AIDL) untuk auth. Framework/SystemUI MIUI juga butuh dua service vendor ini:

- `vendor.xiaomi.hardware.fingerprintextension.IXiaomiFingerprint/default`
- `vendor.xiaomi.hw.touchfeature.ITouchFeature/default`

## Todo

1. Mendaftarkan kedua service AIDL di atas (nama + transaction code identik dengan
   implementasi Xiaomi asli, jadi tidak perlu patch framework/SystemUI).
2. Menerjemahkan `IXiaomiFingerprint.extCmd(cmd, param)` ke HBM panel:
   - `cmd=4` (FOD mode): `1`=enroll, `3`/`5`=auth → HBM ON; `0`/`2`/`4` → OFF
   - `cmd=7` (low-brightness assist): nonzero → ON, `0` → OFF
   - `cmd=10/2/1/3` = bookkeeping, cuma dicatat (jangan dianggap OFF)
3. Watchdog 15 detik: HBM otomatis dimatikan kalau tidak ada refresh (anti burn-in).
4. Set prop FOD MIUI kalau masih kosong:
   - `ro.hardware.fp.fod=1`, `ro.hardware.fp.fod.c=1`
   - `persist.vendor.sys.fp.fod.location.X_Y=370,2002`
   - `persist.vendor.sys.fp.fod.size.width_height=340,340`
   - `persist.vendor.fingerprint.sensor_type=optical`
   - `vendor.fingerprint.aidl.support=1`
5. `ITouchFeature.setModeValue(type, 10, value)` juga me-refresh HBM.

HAL fingerprint Transsion bawaan **tidak diganti** — ini hanya menambah sisi Xiaomi.

## Test runtime

```sh
adb push xiaomi_fod_hal /data/local/tmp/
adb shell 'kill -9 $(pidof xiaomi_fod_hal) 2>/dev/null'
adb shell 'nohup /data/local/tmp/xiaomi_fod_hal > /data/local/tmp/fod_hal.log 2>&1 < /dev/null &'
adb shell '/data/local/tmp/xiaomi_fod_hal --status'
```

## Referensi

- `TUSB-5834/flyme_fod` — `MiFodBridge.java` (semantik extCmd 4/7, touch mode 10)
- `nischay12089/fodbridge-source` — `fodbridge.cpp` (urutan arm/disarm, tx 1)
- `usermahiro/Oplus-UDFPS-Bridge-Transsion` — pola bridge + kontrol
  `/sys/kernel/tran_display/lcm_hbm_state`
