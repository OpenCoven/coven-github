//! Restricted cron schedule parsing for branch gardener runs.

use serde::{Deserialize, Serialize};
use std::fmt;

const ACCEPTED_FORM: &str =
    "accepted form: '<minute 0-59> <hour 0-23> * * *' (for example '0 4 * * *')";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub minute: u8,
    pub hour: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleError {
    message: String,
}

impl ScheduleError {
    pub fn message(&self) -> &str {
        &self.message
    }

    fn accepted_form() -> Self {
        Self {
            message: ACCEPTED_FORM.to_string(),
        }
    }
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ScheduleError {}

pub fn parse_schedule(input: &str) -> Result<Schedule, ScheduleError> {
    let fields: Vec<_> = input.split_whitespace().collect();
    if fields.len() != 5 || fields[2] != "*" || fields[3] != "*" || fields[4] != "*" {
        return Err(ScheduleError::accepted_form());
    }

    let minute = parse_field(fields[0], 59)?;
    let hour = parse_field(fields[1], 23)?;

    Ok(Schedule { minute, hour })
}

fn parse_field(field: &str, max: u8) -> Result<u8, ScheduleError> {
    let value = field
        .parse::<u8>()
        .map_err(|_| ScheduleError::accepted_form())?;
    if value <= max {
        Ok(value)
    } else {
        Err(ScheduleError::accepted_form())
    }
}

impl Schedule {
    pub fn slot_id(&self, unix_minutes: i64) -> Option<String> {
        let minute_of_day = unix_minutes.rem_euclid(1440);
        let scheduled_minute = i64::from(self.hour) * 60 + i64::from(self.minute);
        if minute_of_day == scheduled_minute {
            let unix_day = unix_minutes.div_euclid(1440);
            Some(format!("{unix_day}-{:02}{:02}", self.hour, self.minute))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_accepted_five_field_schedule() {
        assert_eq!(
            parse_schedule("0 4 * * *").unwrap(),
            Schedule { minute: 0, hour: 4 }
        );
    }

    #[test]
    fn rejects_unsupported_schedule_forms_with_human_message() {
        for input in [
            "0 4 * * * extra",
            "60 4 * * *",
            "0 24 * * *",
            "*/5 4 * * *",
            "0 4 * * 1",
        ] {
            let error = parse_schedule(input).unwrap_err();
            assert!(
                error.to_string().contains("accepted form"),
                "{input}: {error}"
            );
            assert!(error.message().contains("0-59"));
        }
    }

    #[test]
    fn slot_id_fires_only_on_scheduled_minute() {
        let schedule = Schedule { minute: 0, hour: 4 };
        let day_two_four_am = 2 * 1440 + 4 * 60;

        assert_eq!(
            schedule.slot_id(day_two_four_am),
            Some("2-0400".to_string())
        );
        assert_eq!(schedule.slot_id(day_two_four_am - 1), None);
        assert_eq!(schedule.slot_id(day_two_four_am + 1), None);
    }

    #[test]
    fn slot_ids_are_stable_within_minute_and_differ_across_days() {
        let schedule = Schedule {
            minute: 30,
            hour: 23,
        };
        let day_one = 1440 + 23 * 60 + 30;
        let day_two = 2 * 1440 + 23 * 60 + 30;

        assert_eq!(schedule.slot_id(day_one), schedule.slot_id(day_one));
        assert_ne!(schedule.slot_id(day_one), schedule.slot_id(day_two));
    }
}
