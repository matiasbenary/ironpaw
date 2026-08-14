use std::str::FromStr;

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    Cron {
        expression: String,
    },
    At {
        #[serde(with = "datetime_serde")]
        #[schemars(with = "String")]
        datetime: chrono::DateTime<chrono::FixedOffset>,
    },
}

impl Schedule {
    pub fn cron(expression: &str) -> Result<Self, String> {
        let expression = match expression.split_whitespace().count() {
            5 => format!("0 {expression}"),
            _ => expression.to_string(),
        };
        cron::Schedule::from_str(&expression)
            .map_err(|err| format!("invalid cron expression: {err}"))?;
        Ok(Self::Cron { expression })
    }

    pub fn at(datetime: &str) -> Result<Self, String> {
        let datetime = parse_datetime(datetime)?;
        let schedule = Self::At { datetime };
        schedule.validate()?;
        Ok(schedule)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        match self {
            Self::Cron { expression } => Self::cron(expression).map(|_| ()),
            Self::At { datetime } => {
                const GRACE: chrono::Duration = chrono::Duration::minutes(5);
                if *datetime <= chrono::Local::now() - GRACE {
                    return Err(format!("'{datetime}' is in the past"));
                }
                Ok(())
            }
        }
    }
}

mod datetime_serde {
    use super::parse_datetime;

    pub fn serialize<S>(
        datetime: &chrono::DateTime<chrono::FixedOffset>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&datetime.to_rfc3339())
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<chrono::DateTime<chrono::FixedOffset>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        parse_datetime(&text).map_err(serde::de::Error::custom)
    }
}

fn parse_datetime(text: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
    if let Ok(datetime) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(datetime);
    }
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .map_err(|err| format!("invalid datetime '{text}' (use RFC 3339): {err}"))?;
    naive
        .and_local_timezone(chrono::Local)
        .single()
        .map(|datetime| datetime.fixed_offset())
        .ok_or_else(|| format!("ambiguous local datetime '{text}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_accepts_standard_forms_and_rejects_invalid_input() {
        assert!(Schedule::cron("0 0 9 * * * *").is_ok());
        assert!(Schedule::cron("0 9 * * 1").is_ok());
        assert!(Schedule::cron("*/5 * * * *").is_ok());
        assert!(Schedule::cron("not a cron").is_err());
    }

    #[test]
    fn at_accepts_future_rejects_past() {
        let future = (chrono::Local::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(Schedule::at(&future).is_ok());

        let past = (chrono::Local::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert!(Schedule::at(&past).is_err());

        assert!(Schedule::at("not a date").is_err());
        let local_future = (chrono::Local::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert!(Schedule::at(&local_future).is_ok());
    }
}
