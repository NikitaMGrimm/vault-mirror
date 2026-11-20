//! Works out when the daemon runs next.

use std::str::FromStr;

use anyhow::{Result, anyhow};
use croner::Cron;
use jiff::{Timestamp, tz::TimeZone};

/// A validated cron schedule, ready to be asked for run times.
pub struct Timetable {
    cron: Cron,
    zone: TimeZone,
}

impl Timetable {
    pub fn new(expression: &str, timezone: Option<&str>) -> Result<Self> {
        let zone = match timezone {
            Some(name) => {
                TimeZone::get(name).map_err(|_| anyhow!("TZ is not a known time zone"))?
            }
            None => TimeZone::UTC,
        };
        let cron = Cron::from_str(expression)
            .map_err(|_| anyhow!("SCHEDULE is an invalid cron schedule"))?;
        Ok(Self { cron, zone })
    }

    /// The first run strictly after `after`.
    pub fn next(&self, after: Timestamp) -> Result<Timestamp> {
        self.cron
            .find_next_occurrence(&after.to_zoned(self.zone.clone()), false)
            .map(|next| next.timestamp())
            .map_err(|error| anyhow!("the cron schedule never runs again: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> Timestamp {
        jiff::civil::date(y, mo, d)
            .at(h, mi, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    #[test]
    fn cron_schedules_follow_the_configured_time_zone() {
        let utc_table = Timetable::new("0 3 * * *", None).unwrap();
        assert_eq!(
            utc_table.next(utc(2026, 10, 14, 2, 0)).unwrap(),
            utc(2026, 10, 14, 3, 0)
        );
        assert_eq!(
            utc_table.next(utc(2026, 10, 14, 3, 0)).unwrap(),
            utc(2026, 10, 15, 3, 0)
        );

        // 03:00 in Berlin is 01:00 UTC in summer and 02:00 UTC in winter.
        let berlin = Timetable::new("0 3 * * *", Some("Europe/Berlin")).unwrap();
        assert_eq!(
            berlin.next(utc(2026, 7, 1, 0, 0)).unwrap(),
            utc(2026, 7, 1, 1, 0)
        );
        assert_eq!(
            berlin.next(utc(2026, 12, 1, 0, 0)).unwrap(),
            utc(2026, 12, 1, 2, 0)
        );
    }

    #[test]
    fn invalid_schedules_are_rejected_at_startup() {
        let error = |schedule: &str, zone: Option<&str>| {
            Timetable::new(schedule, zone)
                .err()
                .expect("must be rejected")
                .to_string()
        };
        assert!(error("every day", None).contains("invalid cron schedule"));
        assert!(error("0 3 * * *", Some("Mars/Olympus")).contains("not a known time zone"));
    }
}
