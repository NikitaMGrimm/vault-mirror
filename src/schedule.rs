//! Works out when the daemon runs next.

use std::str::FromStr;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;

use crate::config::Schedule;

/// A validated schedule, ready to be asked for run times.
pub enum Timetable {
    Cron { cron: Box<Cron>, zone: Tz },
    Every(chrono::Duration),
}

impl Timetable {
    pub fn new(schedule: &Schedule, timezone: Option<&str>) -> Result<Self> {
        match schedule {
            Schedule::Cron(expression) => {
                let zone = match timezone {
                    Some(name) => Tz::from_str(name)
                        .map_err(|_| anyhow!("TZ is not a known time zone: {name}"))?,
                    None => Tz::UTC,
                };
                let cron = Cron::from_str(expression)
                    .map_err(|error| anyhow!("invalid cron schedule {expression:?}: {error}"))?;
                Ok(Self::Cron {
                    cron: Box::new(cron),
                    zone,
                })
            }
            Schedule::Every(interval) => Ok(Self::Every(
                chrono::Duration::from_std(*interval)
                    .map_err(|_| anyhow!("the interval is too long"))?,
            )),
        }
    }

    /// The first run strictly after `after`.
    pub fn next(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
        match self {
            Self::Cron { cron, zone } => cron
                .find_next_occurrence(&after.with_timezone(zone), false)
                .map(|next| next.with_timezone(&Utc))
                .map_err(|error| anyhow!("the cron schedule never runs again: {error}")),
            Self::Every(interval) => Ok(after + *interval),
        }
    }

    /// Whether a run should happen as soon as the daemon starts. An interval
    /// starts counting from a first run; a cron schedule names its times.
    pub fn runs_at_start(&self) -> bool {
        matches!(self, Self::Every(_))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::TimeZone;

    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn cron_schedules_follow_the_configured_time_zone() {
        let schedule = Schedule::Cron("0 3 * * *".into());
        let utc_table = Timetable::new(&schedule, None).unwrap();
        assert_eq!(
            utc_table.next(utc(2026, 10, 14, 2, 0)).unwrap(),
            utc(2026, 10, 14, 3, 0)
        );
        assert_eq!(
            utc_table.next(utc(2026, 10, 14, 3, 0)).unwrap(),
            utc(2026, 10, 15, 3, 0)
        );

        // 03:00 in Berlin is 01:00 UTC in summer and 02:00 UTC in winter.
        let berlin = Timetable::new(&schedule, Some("Europe/Berlin")).unwrap();
        assert_eq!(
            berlin.next(utc(2026, 7, 1, 0, 0)).unwrap(),
            utc(2026, 7, 1, 1, 0)
        );
        assert_eq!(
            berlin.next(utc(2026, 12, 1, 0, 0)).unwrap(),
            utc(2026, 12, 1, 2, 0)
        );
        assert!(!berlin.runs_at_start());
    }

    #[test]
    fn intervals_count_from_the_previous_run() {
        let table = Timetable::new(&Schedule::Every(Duration::from_secs(6 * 3600)), None).unwrap();
        assert_eq!(
            table.next(utc(2026, 10, 14, 2, 30)).unwrap(),
            utc(2026, 10, 14, 8, 30)
        );
        assert!(table.runs_at_start());
    }

    #[test]
    fn invalid_schedules_are_rejected_at_startup() {
        let error = |schedule: &str, zone: Option<&str>| {
            Timetable::new(&Schedule::Cron(schedule.into()), zone)
                .err()
                .expect("must be rejected")
                .to_string()
        };
        assert!(error("every day", None).contains("invalid cron schedule"));
        assert!(error("0 3 * * *", Some("Mars/Olympus")).contains("not a known time zone"));
    }
}
