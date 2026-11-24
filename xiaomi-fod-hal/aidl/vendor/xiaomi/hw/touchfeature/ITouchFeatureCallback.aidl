package vendor.xiaomi.hw.touchfeature;

interface ITouchFeatureCallback {
  int callback(int type, int mode, int status, in int[] intValues, in String[] strValues);
}
