# blec Android backend 0.18.0

Android sources copied from `tauri-plugin-blec` 0.18.0 on crates.io,
upstream commit [d1b38b43e22ccaf8b10ef6fde963537551974dee](https://github.com/MnlPhlp/tauri-plugin-blec/tree/d1b38b43e22ccaf8b10ef6fde963537551974dee/crates/blec/android/lib).
Used under the included upstream MIT license. No generated build outputs
or Rust sources are vendored here.

The app's Gradle settings redirect only the `:tauri-plugin-blec` Android
module to this directory. The Cargo plugin stays pinned to 0.18.0 so the
JNI bridge remains compatible. All other platforms use the unmodified crate.

## Local change

`Peripheral.kt` requests the standard LE 1M PHY for a peripheral whose
discovered GATT database contains the Heart Rate Service (`0x180D`). This
provides a compatibility workaround for repeated Pixel Watch 3/OnePlus link
timeouts after negotiation to the 2M PHY. It does not change WalkingPad
connections. Android's preference is advisory; PHY callbacks report what
the controller actually negotiated. At most two further preference
requests are made if the controller switches to another PHY, avoiding a
retry loop if the device rejects the preference.

On-device testing confirmed `tx=1 rx=1` and continued heart-rate notifications
through screen-off/background periods without the earlier recurring timeouts.
This is a device-specific interoperability observation, not a guarantee for
every watch/phone combination.

When upgrading blec, refresh these sources and reapply/retest the focused
change. Check JNI signatures, the Android manifest, and consumer R8 rules.
