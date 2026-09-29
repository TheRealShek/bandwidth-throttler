use std::error::Error;
use std::fmt;

/// The reason a decimal megabits-per-second rate cannot be used.
#[derive(Debug)]
pub enum ParseRateError {
    InvalidNumber(String),
    OutOfRange(String),
}

impl fmt::Display for ParseRateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNumber(value) => write!(formatter, "Invalid rate: {value}"),
            Self::OutOfRange(value) => {
                write!(
                    formatter,
                    "Rate must be a positive finite number of Mbps: {value}"
                )
            }
        }
    }
}

impl Error for ParseRateError {}

/// Converts decimal megabits per second to bytes per second.
pub fn parse_rate(text: &str) -> Result<u64, ParseRateError> {
    let mbps = text
        .parse::<f64>()
        .map_err(|_| ParseRateError::InvalidNumber(text.to_owned()))?;
    let bytes_per_second = mbps * 125_000.0;
    if !bytes_per_second.is_finite() || !(1.0..u64::MAX as f64).contains(&bytes_per_second) {
        return Err(ParseRateError::OutOfRange(text.to_owned()));
    }
    Ok(bytes_per_second.round() as u64)
}
