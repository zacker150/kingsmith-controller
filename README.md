<img src="assets/icon.svg" width="64" height="64" alt="">

# Kingsmith Controller

A controller for a KingSmith WalkingPad R1 Pro over Bluetooth LE.

Created in [T3 Code](https://t3.codes).

## Heart rate and walk history

The Android app can connect to the Pixel Watch 3's built-in Bluetooth
heart-rate sharing. No custom watch app is required. Standard Bluetooth
heart-rate monitors are supported too.

1. On the watch, open **Connected Fitness → Connect**, or **Settings →
   Connectivity → Share Heart Rate**. Enable **Extended pairing** if needed.
2. In the controller, tap **Search monitors**, then **Connect** beside the
   watch. Confirm heart-rate sharing on the watch if prompted.
3. Connect and start the WalkingPad as usual. Live BPM appears beside the
   pad stats. Nearby devices permission is shared with pad discovery; this
   feature does not request access to health records or watch sensors.

Walks record automatically from the first fresh status confirming movement
until the belt is confirmed stopped (a pause ends the current walk). Each
walk saves its observed time, distance, steps, and timestamped heart-rate
readings. The latest ten walks appear below the controls with average and
maximum BPM. Joining a moving pad records from that point onward.
Average and maximum BPM use recorded readings; gaps remain missing.

Hold a saved walk, right-click it, or use its **⋮** button to open a menu
with **Download TCX** and **Delete walk**. The menu also opens with Shift+F10
on the focused action button. Downloads go to **Downloads/Kingsmith Controller**
and include recorded totals and timestamped heart rate. TCX uses the Other
activity type for walking; GPS and calories are not recorded. Delete removes
the saved walk from local history; previously downloaded files remain available.

Android keeps active Bluetooth links and walk recording running with the
screen off using a connected-device foreground service and a partial CPU
wake lock. It releases these when both connections end. You can allow
notifications to see the ongoing connection notification and tap it to return.
Closing the app's task ends background operation.

Readings disappear after ten seconds without data, or when the monitor
reports missing skin contact. A dropped watch link retries automatically,
with increasing delays, for up to two minutes; leave Share Heart Rate enabled.
If recovery fails, enable sharing and search again. Disconnecting the monitor
cancels recovery. Missing readings cannot be backfilled over Bluetooth.
This does not interrupt the WalkingPad connection or its controls.

History stays in the app's data directory under `walks/`, one JSON file per
walk, with an active checkpoint every five seconds. No health data is uploaded
by this app. Loss of fresh pad status or app shutdown ends a partial walk;
on relaunch, the last checkpoint is recovered as partial. The last few seconds
may be missing after an abrupt shutdown. Uninstalling the app removes history.
The desktop app uses the same BLE support.

Local UI runs should use `npm run tauri -- dev --features mock`: this simulates
both the pad and a heart-rate monitor, with history isolated under `walks-mock/`.

## CI builds

The **Build apps** GitHub Actions workflow runs on pushes to `main` and `v1`,
pull requests into `main`, and manual dispatch. Download its artifacts from
the workflow run page (retained for 14 days):

- `kingsmith-android-universal`: one release APK containing ARM64, ARMv7,
  x86, and x86_64 native libraries (Android 12 or newer), built on
  a GitHub-hosted Ubuntu runner.
- `kingsmith-macos-arm64`: zipped Apple Silicon `.app`, built on a GitHub-hosted
  Mac. Unzip before opening; it is not Developer ID signed or notarized.

The APK uses the hosted runner's debug signing key, which is disposable and
changes between runs. It can be sideloaded, but cannot update an installation
signed by a different key, including local builds and earlier CI runs.
Uninstalling first removes app settings. These builds do not publish GitHub
Releases or install onto devices.

## Device

| | |
|---|---|
| Model | KingSmith WalkingPad R1 Pro |
| BLE name | `R1 Pro` |
| Firmware | `M26_V187.1.2` |
| BLE module | `WLT8266M` |

GATT layout:

- `0000fe00-…` vendor service
  - `0000fe01-…` read, notify: status messages from the pad
  - `0000fe02-…` write without response: commands to the pad
- `00010203-0405-0607-0809-0a0b0c0d1912` OTA service (leave alone)

## Protocol notes

The pad speaks the same protocol as the WalkingPad A1, as implemented in
[ph4r05/ph4-walkingpad](https://github.com/ph4r05/ph4-walkingpad).

- Commands are framed `f7 … <crc> fd`; replies are framed `f8 … <crc> fd`.
- Ask for status: `f7 a2 00 00 a2 fd`. Example idle reply:
  `f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 a3 fd`
  (state 0, manual mode, speed 0).
- Wait about 1 second after subscribing to `fe01` before sending the first
  command. A request sent right away gets no reply.
- Only one BLE client can be connected at a time. The KingSmith app can't
  connect while this controller is connected.
- On macOS, scan before connecting; connecting straight to an address doesn't
  work on macOS 12+.
- ph4-walkingpad's scanner only matches names containing "walkingpad", so it
  misses this pad unless you pass a custom `matcher`.
