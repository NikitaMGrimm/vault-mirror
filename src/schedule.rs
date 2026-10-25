//! Works out when the daemon runs next.

use std::str::FromStr;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;

/// A validated cron schedule, ready to be asked for run times.
pub struct Timetable {
    cron: Cron,
    zone: Tz,
}

impl Timetable {
    pub fn new(expression: &str, timezone: Option<&str>) -> Result<Self> {
        let zone = match timezone {
            Some(name) => Tz::from_str(name).map_err(|_| anyhow!("TZ is not a known time zone"))?,
            None => Tz::UTC,
        };
        let cron = Cron::from_str(expression)
            .map_err(|_| anyhow!("SCHEDULE is an invalid cron schedule"))?;
        Ok(Self { cron, zone })
    }

    /// The first run strictly after `after`.
    pub fn next(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
        self.cron
            .find_next_occurrence(&after.with_timezone(&self.zone), false)
            .map(|next| next.with_timezone(&Utc))
            .map_err(|error| anyhow!("the cron schedule never runs again: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
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
