//! TCX v2 from recorded totals and timestamped HR; no fabricated GPS or speed.

use std::{collections::BTreeMap, fmt::Write};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::walks::Walk;

fn timestamp(ms: u64) -> Result<String, String> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|_| "Walk timestamp is out of range".to_string())?
        .format(&Rfc3339)
        .map_err(|_| "Walk timestamp is out of range".to_string())
}

pub fn export(walk: &Walk) -> Result<String, String> {
    let end = walk.ended_ms.ok_or("This walk is still recording")?;
    if end < walk.started_ms {
        return Err("Walk ends before it starts".into());
    }
    let start = timestamp(walk.started_ms)?;
    // Merge equal timestamps, including readings exactly at the boundaries.
    let mut points = BTreeMap::from([(walk.started_ms, None), (end, None)]);
    for reading in &walk.heart_rate {
        if (walk.started_ms..=end).contains(&reading.at_ms) && (1..=255).contains(&reading.bpm) {
            points.insert(reading.at_ms, Some(reading.bpm));
        }
    }
    let readings: Vec<u16> = points.values().filter_map(|value| *value).collect();
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <TrainingCenterDatabase xmlns=\"http://www.garmin.com/xmlschemas/TrainingCenterDatabase/v2\">\n\
         <Activities><Activity Sport=\"Other\"><Id>{start}</Id>\n\
         <Lap StartTime=\"{start}\"><TotalTimeSeconds>{}</TotalTimeSeconds>\n\
         <DistanceMeters>{}</DistanceMeters><Calories>0</Calories>\n",
        walk.time_s, walk.distance_m
    );
    if let Some(max) = readings.iter().max() {
        let average =
            readings.iter().map(|bpm| u64::from(*bpm)).sum::<u64>() / readings.len() as u64;
        writeln!(xml, "<AverageHeartRateBpm><Value>{average}</Value></AverageHeartRateBpm><MaximumHeartRateBpm><Value>{max}</Value></MaximumHeartRateBpm>").unwrap();
    }
    xml.push_str("<Intensity>Active</Intensity><TriggerMethod>Manual</TriggerMethod><Track>\n");
    for (at, bpm) in points {
        write!(xml, "<Trackpoint><Time>{}</Time>", timestamp(at)?).unwrap();
        if at == end {
            write!(xml, "<DistanceMeters>{}</DistanceMeters>", walk.distance_m).unwrap();
        } else if at == walk.started_ms {
            xml.push_str("<DistanceMeters>0</DistanceMeters>");
        }
        if let Some(bpm) = bpm {
            write!(xml, "<HeartRateBpm><Value>{bpm}</Value></HeartRateBpm>").unwrap();
        }
        xml.push_str("</Trackpoint>\n");
    }
    write!(
        xml,
        "</Track></Lap><Notes>WalkingPad walk. Steps: {}. Calories were not recorded.",
        walk.steps
    )
    .unwrap();
    if walk.interrupted {
        xml.push_str(" Partial walk: pad status was lost or the app closed.");
    }
    xml.push_str("</Notes></Activity></Activities></TrainingCenterDatabase>\n");
    Ok(xml)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heart_rate::Sample;

    fn walk() -> Walk {
        Walk {
            id: "1000-1".into(),
            started_ms: 1000,
            ended_ms: Some(3000),
            interrupted: true,
            time_s: 2,
            distance_m: 5,
            steps: 8,
            heart_rate: vec![],
        }
    }

    #[test]
    fn exports_totals_without_inventing_heart_rate_or_gps() {
        let xml = export(&walk()).unwrap();
        assert!(xml.contains("1970-01-01T00:00:01Z"));
        assert!(xml.contains("<TotalTimeSeconds>2</TotalTimeSeconds>"));
        assert!(xml.contains("<DistanceMeters>5</DistanceMeters>"));
        assert!(xml.contains("Steps: 8."));
        assert!(xml.contains("Partial walk:"));
        assert!(!xml.contains("HeartRate"));
        assert!(!xml.contains("Position"));
        assert_eq!(xml.matches("<Trackpoint>").count(), 2);
    }

    #[test]
    fn sorts_and_merges_real_readings_and_omits_invalid_samples() {
        let mut w = walk();
        w.heart_rate = [
            (3000, 130),
            (2001, 100),
            (1000, 80),
            (2001, 110),
            (999, 90),
            (3001, 90),
            (2500, 256),
            (2400, 0),
        ]
        .into_iter()
        .map(|(at_ms, bpm)| Sample {
            at_ms,
            bpm,
            monitor: "<&watch>".into(),
        })
        .collect();
        let xml = export(&w).unwrap();
        assert_eq!(xml, include_str!("../tests/fixtures/walk.tcx"));
        assert_eq!(xml.matches("<Trackpoint>").count(), 3);
        assert!(xml.contains("00:00:02.001Z"));
        assert!(xml.contains("<AverageHeartRateBpm><Value>106</Value>"));
        assert!(xml.contains("<MaximumHeartRateBpm><Value>130</Value>"));
        assert!(!xml.contains("<Value>100</Value>"));
        assert!(!xml.contains("watch"));
    }

    #[test]
    fn rejects_unfinished_reversed_and_unrepresentable_times() {
        let mut w = walk();
        w.ended_ms = None;
        assert!(export(&w).is_err());
        w.ended_ms = Some(0);
        assert!(export(&w).is_err());
        w.started_ms = u64::MAX;
        w.ended_ms = Some(u64::MAX);
        assert!(export(&w).is_err());
    }
}
