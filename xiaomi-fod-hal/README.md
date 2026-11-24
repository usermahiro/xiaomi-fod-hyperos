# xiaomi_fod_hal

Shim HAL FOD Xiaomi untuk port HyperOS/MIUI di perangkat Transsion (MT6789).

Masalahnya: vendor Transsion cuma sediakan `android.hardware.biometrics.fingerprint.IFingerprint`
(AIDL) untuk auth. Framework/SystemUI MIUI juga butuh dua service vendor ini:

- `vendor.xiaomi.hardware.fingerprintextension.IXiaomiFingerprint/default`
- `vendor.xiaomi.hw.touchfeature.ITouchFeature/default`

Tanpa keduanya, FOD (lingkaran terang di area sensor) tidak pernah menyala dan
prop geometri FOD tidak ada.

## Yang dilakukan

1. Mendaftarkan kedua service AIDL di atas (nama + transaction code identik dengan
   implementasi Xiaomi asli, jadi tidak perlu patch framework/SystemUI).
2. Menerjemahkan `IXiaomiFingerprint.extCmd(cmd, param)` ke HBM panel:
   - `cmd=4` (FOD mode): `1`=enroll, `3`/`5`=auth → HBM ON; `0`/`2`/`4` → OFF
   - `cmd=7` (low-brightness assist): nonzero → ON, `0` → OFF
   - `cmd=10/2/1/3` = bookkeeping, cuma dicatat (jangan dianggap OFF)
3. Watchdog 15 detik: HBM otomatis dimatikan kalau tidak ada refresh (anti burn-in).
4. Set prop FOD MIUI kalau masih kosong:
   - `ro.hardware.fp.fod=1`, `ro.hardware.fp.fod.c=1`
   - `persist.vendor.sys.fp.fod.location.X_Y=540,2178`
   - `persist.vendor.sys.fp.fod.size.width_height=220,220`
   - `persist.vendor.fingerprint.sensor_type=optical`
   - `vendor.fingerprint.aidl.support=1`
5. `ITouchFeature.setModeValue(type, 10, value)` juga me-refresh HBM.

HAL fingerprint Transsion bawaan **tidak diganti** — ini hanya menambah sisi Xiaomi.

## Pemakaian (runtime, /data/local/tmp)

```sh
adb push xiaomi_fod_hal /data/local/tmp/
adb shell 'kill -9 $(pidof xiaomi_fod_hal) 2>/dev/null'
adb shell 'nohup /data/local/tmp/xiaomi_fod_hal > /data/local/tmp/fod_hal.log 2>&1 < /dev/null &'
adb shell '/data/local/tmp/xiaomi_fod_hal --status'
```

Uji HBM langsung:

```sh
adb shell '/data/local/tmp/xiaomi_fod_hal --hbm on'
adb shell 'cat /sys/kernel/tran_display/lcm_hbm_state'
adb shell '/data/local/tmp/xiaomi_fod_hal --hbm off'
```

Uji binder service:

```sh
adb shell 'service call vendor.xiaomi.hardware.fingerprintextension.IXiaomiFingerprint/default 1 i32 4 i32 3'
adb shell 'cat /data/local/tmp/fod_hal.log'
```

## Opsi

| opsi | fungsi | default |
|---|---|---|
| `--daemon` | mode normal (default) | - |
| `--no-touch` | jangan daftarkan ITouchFeature | - |
| `--timeout MS` | watchdog HBM | 15000 |
| `--hbm on\|off` | tulis langsung sysfs, lalu exit | - |
| `--props` | set prop FOD saja, lalu exit | - |
| `--status` | lihat state HBM + prop | - |

## Catatan

- `/sys/kernel/tran_display/lcm_hbm_state` bacaannya bukan `0/1`, tapi nilai register
  heksa (mis. `0xFA`). Yang ditulis tetap `1` / `0`.
- Kalau SystemUI sudah cache prop FOD saat boot, restart SystemUI:
  `am force-stop com.android.systemui` (atau reboot) setelah prop diset.
- Untuk permanen: taruh binary di `/vendor/bin/hw/xiaomi_fod_hal`, rc di
  `/vendor/etc/init/`, manifest VINTF untuk dua interface tersebut, lalu rebuild
  image vendor (vendor di perangkat ini erofs read-only).

## Referensi

- `TUSB-5834/flyme_fod` — `MiFodBridge.java` (semantik extCmd 4/7, touch mode 10)
- `nischay12089/fodbridge-source` — `fodbridge.cpp` (urutan arm/disarm, tx 1)
- `usermahiro/Oplus-UDFPS-Bridge-Transsion` — pola bridge + kontrol
  `/sys/kernel/tran_display/lcm_hbm_state`
