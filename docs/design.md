# Kingsmith Controller: design

Status: draft for review, 2026-10-21, revised after the first round of
answers. Nothing is implemented yet.

A small desktop and Android app that connects to a KingSmith WalkingPad R1 Pro
over Bluetooth LE and controls it: start, stop, speed, mode and live stats.
Device facts are in the [README](../README.md). Protocol references:

- [ph4r05/ph4-walkingpad](https://github.com/ph4r05/ph4-walkingpad) `pad.py`
  (MIT, A1 model).
- The R1 Pro driver in
  [qdomyos-zwift](https://github.com/cagnulein/qdomyos-zwift/tree/master/src/devices/kingsmithr1protreadmill)
  (GPL-3, so used for facts only, never code).

## 1. Decision summary

| | |
|---|---|
| Stack | **Tauri 2**, Rust core, plain TypeScript UI, BLE through **`tauri-plugin-blec`** (btleplug on desktop, its own Kotlin backend on Android) |
| Targets | v1: macOS desktop app and Android (OnePlus 13, Android 16). Windows: nice to have, same code. Linux: out of scope. |
| Builds | Personal, unsigned: built locally on macOS, sideloaded on Android. No notarization, no store. |
| Units | Miles (mph, mi) in the UI. The pad speaks 0.1 km/h, so the app converts. |
| Shape | One window, one screen. The pad's status is the source of truth; the UI never assumes a command worked until a status frame confirms it. |
| First step | A gating spike on macOS and Android (§2.3). If it fails, fall back to Flutter. |

## 2. Stack

### 2.1 Options compared

Size and memory figures are typical for each framework's minimal release
build. They were not measured for this app; the spike in §2.3 measures ours.

| | Tauri 2 + tauri-plugin-blec | Flutter + flutter_blue_plus | KMP + Compose + Kable | Web app + Web Bluetooth |
|---|---|---|---|---|
| BLE on macOS | btleplug (CoreBluetooth), mature | Mature | Kable JVM wraps btleplug through FFI; marked experimental when added in 0.39 (now 0.45) | Chrome only |
| BLE on Android | Plugin's own Kotlin backend, one maintainer, v0.18 | Mature | Mature | Chrome only |
| Windows | btleplug (WinRT) | Third-party 0.0.x package only | Same as macOS (JVM) | Chrome/Edge |
| Install size | ~5–10 MB desktop (uses the system WebView); ~5–10 MB APK (arm64) | ~35–45 MB macOS app; ~6–8 MB APK | ~3–6 MB APK; ~80–150 MB desktop (bundled JRE) | Nothing to install |
| Startup / memory | Fast / low–medium | Fast / medium | Fast on Android; JVM start of 1–2 s and high memory on desktop | Browser-dependent |
| Shared code | ~100% (Rust core + TS UI) | ~100% Dart | ~100% Kotlin | 100% |
| Build toolchain | Rust, Node, Android SDK/NDK | Flutter SDK, Android SDK | JDK, Gradle, Android SDK | Node |
| Verdict | **Chosen** | Fallback | Too heavy on desktop | Ruled out: you want a desktop app |

### 2.2 Why Tauri

- **Lightest real desktop app.** It has the smallest install, starts fast, and
  has few runtime pieces because it reuses the OS WebView.
- **One Rust core.** Protocol, session and safety logic run unchanged on
  macOS, Android and Windows and are tested with `cargo test`. Safety logic
  sits in Rust rather than in the WebView, so it keeps running when the
  WebView is throttled (window hidden, phone screen off).
- **btleplug on macOS** uses CoreBluetooth, the same OS API bleak used against
  this pad. On Windows it uses WinRT, so Windows costs packaging and testing
  only.
- **The plugin fits our needs.** `blec` 0.18 has:
  - scan filters by service UUID;
  - write-without-response and notifications on Android;
  - connection events;
  - a desktop **mock backend** that simulates devices and dropped links.

The risk is the Android half of `blec`: one maintainer, pre-1.0 (last pushed
2026-10-14), and docs.rs failed to build recent releases. Our BLE needs are
tiny (scan, connect, one notify, one write-without-response, disconnect
events). If the plugin falls short, the fix inside Tauri is a ~300-line Kotlin
Tauri plugin of our own. Only if Tauri on Android itself fails do we move to
Flutter. For a personal build, flutter_blue_plus's licence is free, so it
becomes the Flutter BLE choice, and `universal_ble` if Windows matters.

### 2.3 Gating spike (first implementation step)

This is a throwaway Tauri app with no UI beyond a log. On the Mac and on the
OnePlus 13 it must:

1. Scan with a `fe00` service filter and find `R1 Pro`.
2. Connect, subscribe to `fe01`, send `f7 a2 00 00 a2 fd` and decode the reply.
3. Disconnect, and do steps 1–2 again.
4. Report the release app/APK size and cold-start time.

On Android it also logs the pad's MAC address, to confirm §5.2. It only reads
status. Pass: everything works on both platforms, and both artifacts are
≤ 15 MB.

## 3. Platforms and builds

- **macOS (v1).** Tauri 2 needs 10.15+; your Mac runs 15.7.
  - `tauri build` produces an ad-hoc-signed `.app`. Built locally, it isn't
    quarantined, so Gatekeeper doesn't block it.
  - macOS may ask for Bluetooth permission again after a rebuild, because the
    ad-hoc signature changes. Signing with a free Apple Development
    certificate avoids that; it's optional.
- **Android (v1).** OnePlus 13 on Android 16 (API 36). minSdk 31, targetSdk 36.
  - Sideload with `adb install`, debug-signed.
  - OxygenOS kills background apps aggressively. v1 assumes the app is in the
    foreground while you walk (see §7, "Leaving or closing the app").
- **Windows (nice to have).** Same code; build it when convenient. An unsigned
  installer triggers a SmartScreen warning ("More info → Run anyway").
- **Linux:** out of scope.

## 4. Architecture

```
ui/  (TypeScript, Vite, no framework)
  renders AppState, sends intents: connect, start, stop, setSpeed, setMode
      │  Tauri commands ↓   events ↑ (state snapshot after every change)
src-tauri/  (app shell)
  BlecTransport: implements Transport with tauri-plugin-blec
  settings file: remembered pad, units
      │
crates/walkingpad/  (no Tauri dependency)
  protocol   pure functions: CRC, frame encode/decode, Command → bytes,
             bytes → Status / Record, km/h ↔ mph. No I/O, no dependencies.
  session    async state machine over a Transport trait: handshake, poll loop,
             command queue, Stop handling, clamping, reconnect policy
  transport  trait Transport { scan, connect, subscribe, write_no_response,
             disconnect, events }
```

Rules:

- **Only `protocol` knows bytes.** Only `session` decides when to send. The UI
  never talks to BLE.
- **The protocol is typed so bad frames can't be built.** `Speed` is stored
  in pad units (0.1 km/h) and can only be built through a clamping
  constructor. `Command` is an enum. Unknown belt states decode to
  `Unknown(u8)` instead of failing.
- **Decoding is strict.** It checks the `f8` header, the `fd` trailer, the
  length (20 bytes for `a2` and `a7`) and the CRC. Frames that fail are
  counted and logged, never applied. One notification is one frame (MTU 23).

### 4.1 Protocol tables

Frames are `f7 <type> <payload> <crc> fd` (to the pad) and
`f8 <type> <payload> <crc> fd` (from the pad), where
`crc = sum(bytes[1..len-2]) mod 256`. Multi-byte numbers are 3-byte big-endian.

Commands. Bytes are computed with `pad.py`'s `fix_crc`.

| Command | Bytes | Notes |
|---|---|---|
| Ask status | `f7 a2 00 00 a2 fd` | v1. The only command sent to the pad so far. |
| Set speed 2.5 km/h (0.1 km/h units) | `f7 a2 01 19 bc fd` | v1 |
| Speed 0 | `f7 a2 01 00 a3 fd` | ph4's stop. Not yet verified to stop the R1 Pro (§7). |
| Mode: auto / manual / standby | `f7 a2 02 00 a4 fd` / `… 01 a5 fd` / `… 02 a6 fd` | v1. Manual also clears the lock (state 5). |
| Start / pause | `f7 a2 04 01 a7 fd` | v1. **A toggle on R1 Pro-family pads** (qdomyos-zwift): pauses a running belt and starts a stopped one. |
| Last record (latest / next) | `f7 a7 aa ff 50 fd` / `f7 a7 aa 00 51 fd` | Phase 2 |
| Preference `key` = `val` | `f7 a6 <key> 00 <val:3> <crc> fd`, e.g. max speed 6.0 → `f7 a6 03 00 00 00 3c e5 fd` | Phase 2 |

qdomyos-zwift also sends an init sequence (`f7 a5 …`, `f7 a6 00 …`,
`f7 b1 …`, `f7 b3 …`) whose meaning is unknown. Our pad answers status
without it, so we don't send it.

Status frame (`f8 a2`, 20 bytes):

| Bytes | Meaning |
|---|---|
| 2 | Belt state: 0 idle, 1 running, 9 start countdown, 5 locked/standby. qdomyos-zwift: the pad locks after ~15 min idle; switching to manual unlocks it. |
| 3 | Current speed, 0.1 km/h |
| 4 | Mode: 0 auto, 1 manual, 2 standby |
| 5–7 | Time, s |
| 8–10 | Distance, 10 m |
| 11–13 | Steps |
| 14 | Target speed. A1 data fits ×3 (54 after setting 1.8); qdomyos-zwift reads it as ×10 on the R1 Pro. Verify; unused in v1. |
| 15, 17 | Unknown. Candidates for a handrail flag (open question 1). |
| 16 | Last remote button |
| 18 | CRC |
| 19 | `fd` |

Last-record frame (`f8 a7`, 20 bytes): time in bytes 8–10, distance in 11–13,
steps in 14–16.

**Miles conversion.** 1 mph = 16.09344 pad units.

- **The pad's units are unknown, and that's a safety issue.** The pad's own
  display is set to miles. ph4 and qdomyos-zwift both assume the speed byte
  is 0.1 km/h, but neither says what happens in miles mode.
  - If the pad switches to 0.1 mph, then "60" means 6.0 mph (9.7 km/h), not
    6.0 km/h, and the clamp would be wrong by a factor of 1.6.
  - So the protocol layer takes a `PadUnits` setting (`Km` or `Miles`), and
    the clamp is defined in km/h and converted to pad units.
  - Until test 6 sets `PadUnits`, the app sends no speed command above 10
    pad units. That's ≤ 1.0 mph either way.
  - The rest of this section assumes km/h pad units. If test 6 shows mph
    pad units, the UI uses them directly and the clamp becomes 37 units.
- **Speed.** Speed buttons step in 0.1 mph, converted to the nearest pad unit.
  At this resolution, every 0.1 mph step maps to a distinct pad unit and
  converts back to the same mph, so the display never drifts.
- **v1 speed range:** 0.3–3.7 mph, which is pad units 5–60
  (0.5–6.0 km/h).
- **Distance:** shown in miles to 2 decimals; the pad resolves about
  0.006 mi.

Don't port these `pad.py` bugs:

- `send_cmd` sleeps `min(elapsed, spacing)` instead of `spacing - elapsed`.
- The notify handler stores `None` as the last record.
- `__str__` prints the mode as the button.
- ph4's README puts the CRC at byte 17; it is at byte 18.

### 4.2 Test fixtures (no device needed)

All CRCs verified, and decoded with `pad.py` on 2026-10-21. "nb" is
`belt_control.ipynb`, "json" is ph4's README stats file. ph4's frames come
from an A1, which uses the same protocol.

| Source | Frame | Expected |
|---|---|---|
| This pad (README) | `f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 a3 fd` | state 0, manual, speed 0 |
| nb, after mode switch | `f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 01 00 81 00 25 fd` | state 0, manual, button 0x81 |
| nb, after start | `f8 a2 09 00 01 00 00 00 00 00 00 00 00 00 01 00 03 00 b0 fd` | state 9 (countdown) |
| nb, running | `f8 a2 01 14 01 00 00 08 00 00 00 00 00 05 01 00 00 00 c6 fd` | running 2.0 km/h, 8 s, 5 steps |
| nb, after speed 1.8 | `f8 a2 01 14 01 00 00 15 00 00 01 00 00 16 36 00 00 00 1a fd` | speed 2.0, byte 14 = 54, 21 s, 10 m, 22 steps |
| nb, standby while running | `f8 a2 01 09 02 00 00 41 00 00 03 00 00 48 02 00 00 00 3c fd` | mode 2, decelerating at 0.9 |
| ph4 README | `f8 a2 01 0f 01 00 0f d1 00 00 ab 00 12 ae 3c 00 00 00 3a fd` | 1.5 km/h, 4049 s, 1.71 km, 4782 steps |
| json line 1 | `f8 a2 01 3c 01 00 02 2a 00 00 4f 00 03 d1 b4 00 00 00 e3 fd` | 6.0 km/h, 554 s, 790 m, 977 steps |
| nb, last record | `f8 a7 00 1c 2d 00 11 97 00 00 8c 00 00 05 00 00 b9 01 e3 fd` | 140 s, 50 m, 185 steps |

Also test:

- Bad CRC, a missing `fd`, a wrong length, an unknown type and an empty frame
  are all rejected.
- Every command round-trips.
- mph ↔ pad-unit conversion over the whole range.

`session` is tested against a scripted fake pad. The fake answers status
requests, applies speed with a ramp, models the start/pause toggle and the
lock state, and can drop the link. The tests cover:

- The readiness retry.
- Command spacing.
- Stop handling, including that **Stop never sends the toggle when the belt is
  stopped or slowing**.
- Start being sent at most once per press.
- Queued commands being dropped on disconnect.
- No motion command being sent after a reconnect.
- Clamping.
- Stale-link detection.

## 5. Finding, pairing and connecting

### 5.1 How "pairing" works

Neither ph4 nor qdomyos-zwift describes pairing; both simply scan and connect.
What we observed fits this model:

- **No Bluetooth pairing or bonding.** The connection is open: macOS never
  asked for a PIN or confirmation, and reads and writes worked unencrypted.
  Nothing is stored on the pad or the OS, so there's nothing to "un-pair".
- **"Pairing mode" is just an advertising window.** Pressing **M twice**
  makes the pad advertise and accept a connection. It stopped advertising
  roughly 10 minutes after our last disconnect, and a few minutes more after
  the M press. Which event starts the timer wasn't measured; you chose to
  skip it, since the design doesn't depend on it.
- **While a client is connected,** the pad doesn't advertise, since it takes
  one client at a time. Inside the window it was found again straight after
  each disconnect. Whether it re-advertises after a disconnect once the
  window has expired is unknown.
- **A remembered address doesn't bypass the window** (tested 18:55, pad not
  advertising):
  - CoreBluetooth returned the pad from its remembered identifier
    (`retrievePeripheralsWithIdentifiers`) without any scan.
  - But `connectPeripheral` stayed pending for 45 s and never completed.
  - This is how Bluetooth LE works: a central can only connect to a device
    that is advertising. Android's connect-by-MAC has the same limit.
  - So M×2 is needed for every new connection once the window has closed.
- **The lock is a separate thing.** After ~15 min idle, the pad locks itself
  (state 5); switching to manual unlocks it. This concerns starting the belt,
  not connecting.

Measured on 2026-10-21 (read-only, three runs):

- **Advertisement.** Name `R1 Pro`, service `fe00` in the advertisement,
  manufacturer data `0x0060: 57 4c 4d 0f 62 75`. A `fe00` filter finds it in
  0.1–0.5 s.
- **Connection.** Connect takes 0.3 s and MTU is 23.
- **Readiness.** The first status reply came 0.12 s after subscribing, to the
  first request. This contradicts the README's ~1 s, so the session retries
  rather than sleeping a fixed time.
- **Polling.** The pad sends no frames unless asked. It answered 4 of 4 polls
  at 750 ms.

### 5.2 Discovery and remembering

- **Scan.** Use the `fe00` service filter (native on macOS and Android). List
  every match, preselecting `R1 Pro`. We don't filter on name; ph4's
  "walkingpad" filter is what missed this pad.
- **The pad's identity is probably its MAC address.** The manufacturer data
  `57 4c 4d 0f 62 75` looks like a MAC: pads in ph4's issues have MACs
  `57:4C:4D:20:04:BD` and `57:4C:4E:11:79:0C`. The spike confirms this on
  Android, where the MAC is visible.
  - If confirmed, we remember the pad by MAC: read from manufacturer data on
    macOS (which hides MACs), and from the device address on Android.
  - That gives the same ID on the Mac and the phone.
  - Fallback: the CoreBluetooth UUID on macOS (specific to that Mac).
- **Scan before connecting,** on every platform.
  - The README's macOS rule is really a bleak limitation: CoreBluetooth itself
    can connect from a remembered identifier.
  - But btleplug only hands out peripherals it has scanned.
  - Scanning costs under a second once the pad advertises, so we lose
    nothing.
- **Connect screen.** The pad usually isn't advertising when you open the
  app.
  - The screen says "Press M twice on the pad" and keeps scanning (with the
    `fe00` filter) while it's open.
  - It connects to the remembered pad the moment it appears. There's no
    scan timeout and no *Scan again* button.
  - Scanning stops when the screen closes or the app goes to the
    background.

### 5.3 Connection handshake and steady state

1. Connect, then check that `fe01` has notify and `fe02` has
   write-without-response. If not, disconnect with "not a compatible pad".
2. Subscribe to `fe01`.
3. Send *ask status* every 300 ms until the first valid status arrives. Time
   out after 3 s with an error.
4. **Ready.** Poll every 750 ms.
5. **Single writer.** One task owns `fe02` and spaces writes at least 700 ms
   apart (ph4 uses 0.69 s). A queued command takes the next slot instead of a
   poll. Repeated *set speed* commands collapse to the latest one.
6. **Stale link.** Three missed replies in a row grey out the stats and show
   "link stale". *Stop* stays enabled.
7. **Session length.** The connection is held for the whole session, idle or
   not, because reconnecting needs M×2 once the window has closed. The app
   disconnects only on an explicit *Disconnect* or when it closes.
   - Only one client fits, so the Mac app and the phone app can't both be
     connected.
   - *Disconnect* is how you hand the pad from one to the other (then press
     M twice).

### 5.4 Reconnecting

- **On an unexpected disconnect:**
  - Drop every queued command; they are never replayed.
  - Scan and reconnect with backoff (1, 2, 4 … 15 s) for 1 minute. This only
    works if the pad is still advertising.
  - After that, show the connect screen with "Connection lost. Press M twice
    to reconnect." It keeps scanning, as in §5.2.
  - After reconnecting, run the handshake and take state from the pad.
  - **Never send start, speed or mode automatically after a reconnect.**
- **On app launch:** if the remembered pad is found, connect to it. Never
  start the belt.

### 5.5 Platform specifics

- **macOS.**
  - The bundle needs `NSBluetoothAlwaysUsageDescription`.
  - The first scan triggers the permission prompt.
  - If permission is denied, show a link to System Settings → Privacy &
    Security → Bluetooth.
- **Android 16.**
  - Request `BLUETOOTH_SCAN` and `BLUETOOTH_CONNECT` at runtime before the
    first scan.
  - You're fine with location permission, so we accept the plugin's manifest
    as is (it also declares location) and don't override it.
  - If the Bluetooth adapter is off, ask to turn it on.

## 6. Features

**v1:**

- **Connection:** M×2 guidance, scan, connect, remember, reconnect,
  disconnect, and a connection status indicator.
- **Belt control:**
  - Start, including auto-unlock from state 5.
  - Stop.
  - Set speed (±0.1 and ±0.5 mph buttons, 3 editable presets).
  - Switch mode (manual / auto / standby).
- **Live stats:** speed (mph), time, distance (mi), steps, belt state, and
  the start countdown.
- **Units:** miles by default, with a km setting (it's only a conversion).
- **Desktop:** a compact window (~360×560) with keyboard shortcuts.

**Phase 2:**

- **Running mode / handrail,** raising the cap from 6.0 to 10 km/h (3.7 → 6.2
  mph). It depends on finding a handrail flag in the status frame (open
  question 1), or else on a manual "handrail up" switch.
- **Preferences:** max speed, start speed, intelligent start, sensitivity,
  display, child lock, units, target. Each is verified on the device first.
- **Reading the pad's settings,** using QWalkingPad's documented query (GPL,
  so used as documentation only).
- **History / last record.** With the KingSmith app out of the picture,
  there's no sync to protect.
- **Session logging** to local JSONL/CSV, and **calorie estimates** from a
  profile, using ph4's formulas.
- **Android foreground service** with a notification *Stop* action, so the
  app survives OxygenOS background killing. Plus a desktop menu-bar
  mini-controller.

**Later:** Windows packaging, Health Connect / Apple Health export.

## 7. Safety

The app moves a belt someone is standing on, so these are requirements, not
polish.

- **Stop is always reachable.**
  - A large red *Stop* sits in the same place on every screen and in every
    connection state, including connecting, stale and reconnecting.
  - It is never disabled while a link exists.
  - Desktop: `Esc` and `Space` both mean Stop, and never Start.
  - Nothing covers it.
- **Stop never uses the toggle blindly.** On R1 Pro-family pads,
  `f7 a2 04 01` toggles start/pause. qdomyos-zwift users saw a "stop" restart
  a stopped belt, and a retried stop restart a belt that was still slowing
  down (it took 21 s to stop).
  - Stop pre-empts the queue and sends an idempotent command: speed 0, or
    standby if speed 0 turns out not to stop the R1 Pro. Which one is decided
    by supervised test 1.
  - It is re-sent each slot until confirmed. Confirmed means status shows the
    speed falling or the belt leaving the running state.
  - The toggle is used for Stop only if test 1 shows nothing else works. Even
    then: only when the belt is running at steady speed, at most once per
    press, and never retried automatically.
  - If Stop isn't confirmed within 5 s, the app shows "Stop not confirmed —
    use the remote or the pad's button."
- **Start is sent at most once per press.**
  - It's only offered when the belt is idle or locked. From the locked state
    (5) the app switches to manual first and waits for confirmation.
  - The app shows the countdown (state 9).
  - If the belt isn't counting down or running within 5 s, the app says
    "Start not confirmed" and waits for another press. It never retries by
    itself, because a retry could toggle the belt off or on unexpectedly.
- **Speeds are clamped in the protocol layer.**
  - v1 allows 0.5–6.0 km/h (0.3–3.7 mph), the walking range with the handrail
    down.
  - Requests below the minimum become the minimum. Zero is only reachable via
    Stop.
  - The 10 km/h running range waits for Phase 2.
- **No surprise motion.**
  - Auto mode can only be selected while the belt is stopped. Standby can be
    selected any time.
  - Nothing that moves the belt is ever sent automatically: not on launch, not
    on reconnect, and not from a replayed queue.
- **Disconnect or crash.**
  - Working assumption: **the belt keeps running at its current speed**,
    because nothing tells the pad to stop. Supervised test 4 checks this.
  - The first-run screen and the "connection lost" banner say: "If the app
    loses the pad, the belt keeps going — use the remote or step off."
- **Leaving or closing the app.**
  - **Desktop.** Closing the window (or ⌘Q) ends the app and drops the
    Bluetooth link, even if the belt is moving. **The belt keeps running**:
    there's no prompt and no automatic stop (your choice). Use the remote
    afterwards.
  - **Android.**
    - Switching apps or locking the screen leaves the app running and
      connected for as long as OxygenOS allows.
    - Swiping it away from recents, or the OS killing it, gives the app no
      reliable chance to act. That's the disconnect case above.
    - The Phase 2 foreground service makes this much rarer.
- **Device tests need your authorization.** Before each test session I'll ask
  in the thread, listing the exact commands.
  - Motion tests run with you present.
  - Any speed command is ≤ 10 pad units, which is ≤ 1.0 mph whatever the
    pad's units turn out to be.
  - Test 6 runs first.

  | # | Test | Sends | Why |
  |---|---|---|---|
  | 6 | Units: start, set 10 pad units, read the console | start, speed | Does miles mode change BLE units? Sets `PadUnits`. |
  | 1 | Stop: speed 0, then standby | start, speed 0, mode | Picks the idempotent Stop |
  | 2 | Start toggle: send start/pause once while running | start | Confirms the toggle on this pad |
  | 3 | Lock/unlock: wait for state 5, switch to manual | mode | Unlock path |
  | 4 | Kill the app mid-run; turn Bluetooth off mid-run | start, speed | Disconnect behaviour |
  | 5 | Commands closer together than 700 ms | speed | Spacing rule |
  | 7 | Read-only: raise/lower the handrail while polling | status only | Handrail flag (open question 1) |

## 8. Open questions

1. **Handrail:** no reference documents a handrail flag. Bytes 15 and 17 of
   the status frame are unexplained and are the candidates. Test 7 was
   started but stopped before the pad connected. For v1 we assume the
   handrail is down (walking range) and revisit it in Phase 2.
2. **Pad units:** the pad's display shows miles. Test 6 decides whether the
   BLE speed byte follows it (§4.1). It needs your go-ahead, since it moves
   the belt at ≤ 1.0 mph.

Settled: running mode / handrail is Phase 2; v1 caps at 6.0 km/h (3.7 mph).
