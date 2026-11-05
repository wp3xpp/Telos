use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDurationError {
    message: &'static str,
}

impl fmt::Display for ParseDurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ParseDurationError {}

/// Parse an integer duration expressed in seconds, or with an `s`, `m`, or `h` suffix.
pub fn parse_duration_seconds(input: &str) -> Result<u64, ParseDurationError> {
    let (number, multiplier) = match input.as_bytes().last() {
        Some(b's') => (&input[..input.len() - 1], 1),
        Some(b'm') => (&input[..input.len() - 1], 60),
        Some(b'h') => (&input[..input.len() - 1], 60 * 60),
        Some(b'0'..=b'9') => (input, 1),
        Some(_) => {
            return Err(ParseDurationError {
                message: "duration must be an integer with an optional s, m, or h suffix",
            });
        }
        None => {
            return Err(ParseDurationError {
                message: "duration must not be empty",
            });
        }
    };

    let value = number.parse::<u64>().map_err(|_| ParseDurationError {
        message: "duration must be a non-negative integer",
    })?;

    value.checked_mul(multiplier).ok_or(ParseDurationError {
        message: "duration is too large",
    })
}

/// Format a duration in seconds using compact `h`, `m`, and `s` components.
pub fn format_duration_seconds(seconds: u64) -> String {
    if seconds == 0 {
        return "0s".to_string();
    }

    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    let mut formatted = String::new();

    if hours != 0 {
        formatted.push_str(&format!("{hours}h"));
    }
    if minutes != 0 {
        formatted.push_str(&format!("{minutes}m"));
    }
    if seconds != 0 {
        formatted.push_str(&format!("{seconds}s"));
    }

    formatted
}

#[cfg(test)]
mod tests {
    use super::{format_duration_seconds, parse_duration_seconds};

    #[test]
    fn formats_compact_duration_components() {
        assert_eq!(format_duration_seconds(0), "0s");
        assert_eq!(format_duration_seconds(5), "5s");
        assert_eq!(format_duration_seconds(300), "5m");
        assert_eq!(format_duration_seconds(3600), "1h");
        assert_eq!(format_duration_seconds(3661), "1h1m1s");
    }

    #[test]
    fn parses_seconds_and_supported_suffixes() {
        assert_eq!(parse_duration_seconds("5"), Ok(5));
        assert_eq!(parse_duration_seconds("5s"), Ok(5));
        assert_eq!(parse_duration_seconds("5m"), Ok(300));
        assert_eq!(parse_duration_seconds("1h"), Ok(3600));
    }

    #[test]
    fn rejects_invalid_input() {
        for input in ["", "s", "5d", "-5", "five"] {
            assert!(parse_duration_seconds(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn rejects_overflow() {
        assert!(parse_duration_seconds("18446744073709551615h").is_err());
    }
}
