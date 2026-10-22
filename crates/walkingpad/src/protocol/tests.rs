//! Fixtures from design §4.2, all re-checked by hand: CRC and every decoded
//! field.

use super::*;

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

fn status(s: &str) -> Status {
    match decode_reply(&hex(s)) {
        Ok(Reply::Status(st)) => st,
        other => panic!("expected a status frame, got {other:?}"),
    }
}

const MANUAL: ModeState = ModeState::Known(Mode::Manual);

#[test]
fn fixture_this_pad_idle() {
    let s = status("f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 a3 fd");
    assert_eq!(s.belt, BeltState::Idle);
    assert_eq!(s.mode, MANUAL);
    assert_eq!(s.speed, 0);
    assert_eq!((s.time_s, s.distance_10m, s.steps), (0, 0, 0));
    assert_eq!(s.remote_key(), RemoteKey::None);
    assert_eq!(s.fault(), None);
    assert!(!s.is_moving());
}

#[test]
fn fixture_nb_after_mode_switch() {
    let s = status("f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 01 00 81 00 25 fd");
    assert_eq!(s.belt, BeltState::Idle);
    assert_eq!(s.mode, MANUAL);
    assert_eq!(s.remote_key(), RemoteKey::ModeLongPress); // 0x81
    assert_eq!(s.byte14, 1);
}

#[test]
fn fixture_nb_after_start() {
    let s = status("f8 a2 09 00 01 00 00 00 00 00 00 00 00 00 01 00 03 00 b0 fd");
    assert_eq!(s.belt, BeltState::Countdown(9));
    assert_eq!(s.speed, 0);
    assert_eq!(s.mode, MANUAL);
    assert_eq!(s.remote_key(), RemoteKey::Stop); // byte 16 = 3
    assert!(!s.is_moving());
    assert!(s.is_active());
}

#[test]
fn fixture_nb_running() {
    let s = status("f8 a2 01 14 01 00 00 08 00 00 00 00 00 05 01 00 00 00 c6 fd");
    assert_eq!(s.belt, BeltState::Running);
    assert_eq!(s.speed, 20); // 2.0 km/h
    assert_eq!(s.time_s, 8);
    assert_eq!(s.distance_10m, 0);
    assert_eq!(s.steps, 5);
    assert!(s.is_moving());
}

#[test]
fn fixture_nb_after_speed_1_8() {
    let s = status("f8 a2 01 14 01 00 00 15 00 00 01 00 00 16 36 00 00 00 1a fd");
    assert_eq!(s.belt, BeltState::Running);
    assert_eq!(s.speed, 20); // still 2.0 while ramping down
    assert_eq!(s.byte14, 54); // 1.8 × 3 on the A1; unused (KS Fit ignores it)
    assert_eq!(s.time_s, 21);
    assert_eq!(s.distance_10m, 1); // 10 m
    assert_eq!(s.steps, 22);
}

#[test]
fn fixture_nb_standby_while_running() {
    let s = status("f8 a2 01 09 02 00 00 41 00 00 03 00 00 48 02 00 00 00 3c fd");
    assert_eq!(s.belt, BeltState::Running);
    assert_eq!(s.mode, ModeState::Known(Mode::Standby));
    assert_eq!(s.speed, 9); // decelerating at 0.9 km/h
    assert_eq!(s.time_s, 65);
    assert_eq!(s.distance_10m, 3);
    assert_eq!(s.steps, 72);
}

#[test]
fn fixture_ph4_readme() {
    let s = status("f8 a2 01 0f 01 00 0f d1 00 00 ab 00 12 ae 3c 00 00 00 3a fd");
    assert_eq!(s.belt, BeltState::Running);
    assert_eq!(s.speed, 15); // 1.5 km/h
    assert_eq!(s.time_s, 4049);
    assert_eq!(s.distance_10m, 171); // 1.71 km
    assert_eq!(s.steps, 4782);
}

#[test]
fn fixture_json_line_1() {
    let s = status("f8 a2 01 3c 01 00 02 2a 00 00 4f 00 03 d1 b4 00 00 00 e3 fd");
    assert_eq!(s.speed, 60); // 6.0 km/h
    assert_eq!(s.time_s, 554);
    assert_eq!(s.distance_10m, 79); // 790 m
    assert_eq!(s.steps, 977);
    assert_eq!(s.byte14, 180); // 6.0 × 3 on the A1; unused
}

#[test]
fn fixture_nb_last_record() {
    let r = match decode_reply(&hex(
        "f8 a7 00 1c 2d 00 11 97 00 00 8c 00 00 05 00 00 b9 01 e3 fd",
    )) {
        Ok(Reply::Record(r)) => r,
        other => panic!("expected a record, got {other:?}"),
    };
    assert_eq!(r.time_s, 140);
    assert_eq!(r.distance_10m, 5); // 50 m
    assert_eq!(r.steps, 185);
    assert_eq!(r.head, [0x00, 0x1c, 0x2d, 0x00, 0x11, 0x97]);
    assert_eq!(r.unknown17, 1);
}

#[test]
fn status_encode_reproduces_every_fixture() {
    for f in [
        "f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 a3 fd",
        "f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 01 00 81 00 25 fd",
        "f8 a2 09 00 01 00 00 00 00 00 00 00 00 00 01 00 03 00 b0 fd",
        "f8 a2 01 14 01 00 00 08 00 00 00 00 00 05 01 00 00 00 c6 fd",
        "f8 a2 01 14 01 00 00 15 00 00 01 00 00 16 36 00 00 00 1a fd",
        "f8 a2 01 09 02 00 00 41 00 00 03 00 00 48 02 00 00 00 3c fd",
        "f8 a2 01 0f 01 00 0f d1 00 00 ab 00 12 ae 3c 00 00 00 3a fd",
        "f8 a2 01 3c 01 00 02 2a 00 00 4f 00 03 d1 b4 00 00 00 e3 fd",
    ] {
        assert_eq!(status(f).encode(), hex(f), "{f}");
    }
}

#[test]
fn belt_state_table() {
    let cases = [
        (0, BeltState::Idle),
        (1, BeltState::Running),
        (2, BeltState::Paused),
        (3, BeltState::Stopped),
        (4, BeltState::Fault),
        (5, BeltState::Sleep),
        (6, BeltState::Countdown(6)),
        (9, BeltState::Countdown(9)),
        (10, BeltState::Other(10)),
        (11, BeltState::LockOff(11)),
        (0xff, BeltState::LockOff(0xff)),
        (12, BeltState::Other(12)),
        (0x80, BeltState::Other(0x80)),
    ];
    let base = status("f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 a3 fd");
    for (byte, state) in cases {
        assert_eq!(BeltState::from_byte(byte), state);
        assert_eq!(state.byte(), byte);
        let s = Status {
            belt: state,
            ..base
        };
        assert_eq!(status_from(&s.encode()).belt, state, "{byte}");
    }
    for b in 0..=255u8 {
        assert_eq!(BeltState::from_byte(b).byte(), b);
    }
}

#[test]
fn mode_table() {
    assert_eq!(ModeState::from_byte(0), ModeState::Known(Mode::Auto));
    assert_eq!(ModeState::from_byte(1), ModeState::Known(Mode::Manual));
    assert_eq!(ModeState::from_byte(2), ModeState::Known(Mode::Standby));
    assert_eq!(ModeState::from_byte(3), ModeState::Newer);
    assert_eq!(ModeState::from_byte(9), ModeState::Unknown(9));
    for b in 0..=255u8 {
        assert_eq!(ModeState::from_byte(b).byte(), b);
    }
}

#[test]
fn running_test_counts_speed_when_the_state_byte_lags() {
    let base = status("f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 a3 fd");
    let lagging = Status {
        belt: BeltState::Idle,
        speed: 12,
        ..base
    };
    assert!(lagging.is_moving());
    let slowing = Status {
        belt: BeltState::Stopped,
        speed: 4,
        ..base
    };
    assert!(slowing.is_moving());
    let paused = Status {
        belt: BeltState::Paused,
        speed: 0,
        ..base
    };
    assert!(!paused.is_moving() && !paused.is_active());
}

#[test]
fn fault_codes() {
    let base = status("f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 a3 fd");
    assert_eq!(base.fault(), None);
    let e3 = Status {
        fault_code: 3,
        ..base
    };
    assert_eq!(e3.fault().as_deref(), Some("E03"));
    assert_eq!(status_from(&e3.encode()).fault_code, 3);
    assert_eq!(
        Status {
            fault_code: 15,
            ..base
        }
        .fault()
        .as_deref(),
        Some("E15")
    );
    // State 4 shows a fault even without a code.
    let state4 = Status {
        belt: BeltState::Fault,
        ..base
    };
    assert_eq!(state4.fault().as_deref(), Some("E00"));
}

#[test]
fn remote_keys() {
    assert_eq!(RemoteKey::from_byte(1), RemoteKey::Mode);
    assert_eq!(RemoteKey::from_byte(2), RemoteKey::Up);
    assert_eq!(RemoteKey::from_byte(3), RemoteKey::Stop);
    assert_eq!(RemoteKey::from_byte(4), RemoteKey::Down);
    assert_eq!(RemoteKey::from_byte(0x81), RemoteKey::ModeLongPress);
    assert_eq!(RemoteKey::from_byte(7), RemoteKey::Other(7));
}

fn status_from(bytes: &[u8]) -> Status {
    match decode_reply(bytes).unwrap() {
        Reply::Status(s) => s,
        Reply::Record(_) => panic!("record"),
    }
}

// --- negative cases --------------------------------------------------------

const IDLE: &str = "f8 a2 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 a3 fd";

#[test]
fn rejects_bad_crc() {
    let mut f = hex(IDLE);
    f[18] = 0xa4;
    assert_eq!(
        decode_reply(&f),
        Err(FrameError::BadCrc {
            expected: 0xa3,
            found: 0xa4
        })
    );
    // A flipped payload byte is caught by the CRC too.
    let mut f = hex(IDLE);
    f[3] = 0x3c;
    assert!(matches!(decode_reply(&f), Err(FrameError::BadCrc { .. })));
}

#[test]
fn rejects_missing_trailer() {
    let mut f = hex(IDLE);
    f[19] = 0x00;
    assert!(matches!(
        decode_reply(&f),
        Err(FrameError::BadTrailer(0x00))
    ));
    // Dropping the trailer altogether also fails (wrong length).
    let f = &hex(IDLE)[..19];
    assert!(matches!(
        decode_reply(f),
        Err(FrameError::WrongLength { found: 19, .. })
    ));
}

#[test]
fn rejects_wrong_length() {
    let mut long = hex(IDLE);
    long.insert(5, 0);
    assert!(matches!(
        decode_reply(&long),
        Err(FrameError::WrongLength {
            frame_type: 0xa2,
            expected: 20,
            found: 21
        })
    ));
    // A well-formed 6-byte frame with a valid CRC is still the wrong length.
    let short = frame(REPLY_HEADER, &[TYPE_STATUS, 0, 0]);
    assert!(matches!(
        decode_reply(&short),
        Err(FrameError::WrongLength { found: 6, .. })
    ));
    let short_record = frame(REPLY_HEADER, &[TYPE_RECORD, 0, 0]);
    assert!(matches!(
        decode_reply(&short_record),
        Err(FrameError::WrongLength {
            frame_type: 0xa7,
            ..
        })
    ));
}

#[test]
fn rejects_unknown_type() {
    let mut body = [0u8; 17];
    body[0] = 0xa5;
    let f = frame(REPLY_HEADER, &body);
    assert_eq!(f.len(), 20);
    assert_eq!(decode_reply(&f), Err(FrameError::UnknownType(0xa5)));
}

#[test]
fn rejects_empty_and_tiny() {
    assert_eq!(decode_reply(&[]), Err(FrameError::Empty));
    assert_eq!(decode_reply(&[0xf8]), Err(FrameError::TooShort(1)));
    assert_eq!(
        decode_reply(&[0xf8, 0xa2, 0xfd]),
        Err(FrameError::TooShort(3))
    );
    assert_eq!(decode_command(&[]), Err(FrameError::Empty));
}

#[test]
fn rejects_command_frames_as_replies_and_vice_versa() {
    let ask = Command::AskStatus.encode();
    assert_eq!(decode_reply(&ask), Err(FrameError::BadHeader(0xf7)));
    assert_eq!(decode_command(&hex(IDLE)), Err(FrameError::BadHeader(0xf8)));
}

// --- commands ------------------------------------------------------------

fn speed(units: i64) -> Speed {
    Speed::clamped(units).0
}

#[test]
fn command_bytes_match_design_table() {
    let cases: [(Command, &str); 10] = [
        (Command::AskStatus, "f7 a2 00 00 a2 fd"),
        (Command::SetSpeed(speed(25)), "f7 a2 01 19 bc fd"),
        (Command::SpeedZero, "f7 a2 01 00 a3 fd"),
        (Command::SetMode(Mode::Auto), "f7 a2 02 00 a4 fd"),
        (Command::SetMode(Mode::Manual), "f7 a2 02 01 a5 fd"),
        (Command::SetMode(Mode::Standby), "f7 a2 02 02 a6 fd"),
        (Command::StartPause, "f7 a2 04 01 a7 fd"),
        (Command::AskRecord(RecordQuery::Latest), "f7 a7 aa ff 50 fd"),
        (Command::AskRecord(RecordQuery::Next), "f7 a7 aa 00 51 fd"),
        (
            Command::SetPreference(Preference::new(Preference::MAX_SPEED, 60).unwrap()),
            "f7 a6 03 00 00 00 3c e5 fd",
        ),
    ];
    for (cmd, bytes) in cases {
        assert_eq!(cmd.encode(), hex(bytes), "{cmd:?}");
    }
}

#[test]
fn every_command_round_trips() {
    let mut cmds = vec![
        Command::AskStatus,
        Command::SpeedZero,
        Command::StartPause,
        Command::AskRecord(RecordQuery::Latest),
        Command::AskRecord(RecordQuery::Next),
        Command::SetPreference(Preference::new(9, 0x00ab_cdef).unwrap()),
    ];
    cmds.extend([Mode::Auto, Mode::Manual, Mode::Standby].map(Command::SetMode));
    for u in MIN_SPEED_UNITS..=MAX_SPEED_UNITS {
        cmds.push(Command::SetSpeed(speed(i64::from(u))));
    }
    for cmd in cmds {
        let bytes = cmd.encode();
        assert_eq!(bytes[0], COMMAND_HEADER);
        assert_eq!(*bytes.last().unwrap(), TRAILER);
        assert_eq!(decode_command(&bytes), Ok(cmd.wire()), "{cmd:?}");
    }
}

#[test]
fn unmodelled_command_frames_decode_as_other() {
    let f = frame(COMMAND_HEADER, &[TYPE_STATUS, 0x03, 0x07]); // ph4's cmd_162_3_7
    assert_eq!(
        decode_command(&f),
        Ok(WireCommand::Other(vec![0xa2, 0x03, 0x07]))
    );
}

#[test]
fn preference_value_is_24_bits() {
    assert!(Preference::new(1, 0x00ff_ffff).is_some());
    assert!(Preference::new(1, 0x0100_0000).is_none());
}

#[test]
fn only_motion_commands_can_move_the_belt() {
    assert!(Command::StartPause.can_move_belt());
    assert!(Command::SetSpeed(speed(20)).can_move_belt());
    assert!(!Command::SpeedZero.can_move_belt());
    assert!(!Command::SetMode(Mode::Standby).can_move_belt());
    assert!(!Command::AskStatus.can_move_belt());
}

// --- clamping ------------------------------------------------------------

#[test]
fn clamp_to_the_walking_range() {
    assert_eq!(Speed::clamped(25), (Speed(25), Clamp::None));
    assert_eq!(Speed::clamped(5), (Speed(5), Clamp::None));
    assert_eq!(Speed::clamped(60), (Speed(60), Clamp::None));
    assert_eq!(Speed::clamped(61), (Speed(60), Clamp::CappedAtMax));
    assert_eq!(Speed::clamped(255), (Speed(60), Clamp::CappedAtMax));
    assert_eq!(Speed::clamped(i64::MAX), (Speed(60), Clamp::CappedAtMax));
    assert_eq!(Speed::clamped(4), (Speed(5), Clamp::RaisedToMin));
    for req in -300..=300 {
        let s = Speed::clamped(req).0.units();
        assert!((5..=60).contains(&s), "{req} → {s}");
    }
}

#[test]
fn clamp_zero_and_negative_become_the_minimum() {
    for req in [0, -1, -300, i64::MIN] {
        let (s, c) = Speed::clamped(req);
        assert_eq!(s.units(), MIN_SPEED_UNITS);
        assert_eq!(c, Clamp::RaisedToMin);
    }
}

// --- conversions ---------------------------------------------------------

#[test]
fn mph_walking_range_maps_to_pad_units_5_to_60() {
    let m = DisplayUnits::Miles;
    assert_eq!(display_tenths_to_pad(3, m), 5); // 0.3 mph = 0.48 km/h
    assert_eq!(display_tenths_to_pad(37, m), 60); // 3.7 mph = 5.95 km/h
    let mut prev = 0;
    for t in 3..=37 {
        let u = display_tenths_to_pad(t, m);
        assert!(u > prev, "0.{t} mph must map to a distinct unit");
        assert!((5..=60).contains(&u));
        prev = u;
        // and back to the same 0.1 mph, so the display never drifts
        assert_eq!(pad_to_display_tenths(u as u8, m), t, "{t} → {u}");
        let shown = pad_to_display_speed(u as u8, m);
        assert!((shown - f64::from(t) / 10.0).abs() < 0.05, "{t}: {shown}");
    }
}

#[test]
fn mph_round_trip_over_the_whole_byte_range() {
    for display in [DisplayUnits::Miles, DisplayUnits::Km] {
        for u in 0..=255u8 {
            let t = pad_to_display_tenths(u, display);
            let back = display_tenths_to_pad(t, display);
            // A display tenth is at most 1.61 pad units wide.
            assert!(back.abs_diff(u32::from(u)) <= 1, "{display:?} {u}");
        }
        // tenths → units → tenths is exact (pad units are the finer unit)
        for t in 0..=150 {
            let u = display_tenths_to_pad(t, display);
            assert!(u <= 255);
            assert_eq!(
                pad_to_display_tenths(u as u8, display),
                t,
                "{display:?} {t}"
            );
        }
    }
}

#[test]
fn km_display_is_identity() {
    for u in 0..=255u8 {
        assert_eq!(pad_to_display_tenths(u, DisplayUnits::Km), u32::from(u));
        assert_eq!(
            display_tenths_to_pad(u32::from(u), DisplayUnits::Km),
            u32::from(u)
        );
    }
}

#[test]
fn mph_display_values() {
    // 2.0 km/h = 1.24 mph; 6.0 km/h = 3.73 mph (× 0.6213712)
    let s = pad_to_display_speed(20, DisplayUnits::Miles);
    assert!((s - 2.0 * 0.621_371_2).abs() < 1e-6);
    let s = pad_to_display_speed(60, DisplayUnits::Miles);
    assert!((s - 6.0 * 0.621_371_2).abs() < 1e-6);
    // 171 × 10 m = 1.71 km = 1.0625 mi
    assert!((distance_to_display(171, DisplayUnits::Miles) - 1.0625).abs() < 0.001);
    assert!((distance_to_display(171, DisplayUnits::Km) - 1.71).abs() < 1e-9);
}

// --- MAC -----------------------------------------------------------------

#[test]
fn mac_from_this_pads_manufacturer_data() {
    assert_eq!(
        mac_from_manufacturer_data(&[0x57, 0x4c, 0x4d, 0x0f, 0x62, 0x75]).as_deref(),
        Some("57:4C:4D:0F:62:75")
    );
    assert_eq!(mac_from_manufacturer_data(&[0; 6]), None);
    assert_eq!(mac_from_manufacturer_data(&[0xff; 6]), None);
    assert_eq!(mac_from_manufacturer_data(&[1, 2, 3, 4, 5]), None);
    assert_eq!(mac_from_manufacturer_data(&[1, 2, 3, 4, 5, 6, 7]), None);
    assert_eq!(
        normalize_mac("57:4c:4d:0f:62:75").as_deref(),
        Some("57:4C:4D:0F:62:75")
    );
    assert_eq!(
        normalize_mac("57-4C-4D-0F-62-75").as_deref(),
        Some("57:4C:4D:0F:62:75")
    );
    assert_eq!(normalize_mac("F076ECBD-4FC5-B48F-A341-D7101DBCFBB8"), None);
    assert_eq!(normalize_mac("57:4C:4D:0F:62"), None);
}

#[test]
fn checksum_wraps() {
    assert_eq!(checksum(&[0xff, 0x02]), 0x01);
    assert_eq!(checksum(&[]), 0);
}
