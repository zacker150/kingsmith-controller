<img src="assets/icon.svg" width="64" height="64" alt="">

# Kingsmith Controller

A controller for a KingSmith WalkingPad R1 Pro over Bluetooth LE.

Created in [T3 Code](https://t3.codes).

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
