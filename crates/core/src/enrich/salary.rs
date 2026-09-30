//! Salary normalization: any pay period to a year, any currency to USD.

/// Approximate USD value of one unit of each currency. A small static table that is good
/// enough for ranking pay across markets (NL search compares percentiles, not payslips).
/// Refresh it occasionally; last reviewed 2026-09.
const USD_PER_UNIT: &[(&str, f64)] = &[
    ("USD", 1.0),
    ("EUR", 1.08),
    ("GBP", 1.27),
    ("CHF", 1.12),
    ("CAD", 0.73),
    ("AUD", 0.66),
    ("NZD", 0.61),
    ("SGD", 0.74),
    ("HKD", 0.128),
    ("JPY", 0.0067),
    ("CNY", 0.14),
    ("KRW", 0.00072),
    ("INR", 0.012),
    ("PKR", 0.0036),
    ("BDT", 0.0083),
    ("PHP", 0.0175),
    ("IDR", 0.000062),
    ("MYR", 0.22),
    ("THB", 0.029),
    ("VND", 0.00004),
    ("AED", 0.272),
    ("SAR", 0.267),
    ("ILS", 0.27),
    ("TRY", 0.03),
    ("SEK", 0.095),
    ("NOK", 0.093),
    ("DKK", 0.145),
    ("PLN", 0.25),
    ("CZK", 0.043),
    ("HUF", 0.0027),
    ("RON", 0.22),
    ("UAH", 0.024),
    ("NGN", 0.00065),
    ("GHS", 0.065),
    ("KES", 0.0077),
    ("ZAR", 0.054),
    ("EGP", 0.02),
    ("MAD", 0.1),
    ("BRL", 0.18),
    ("MXN", 0.055),
    ("ARS", 0.001),
    ("CLP", 0.0011),
    ("COP", 0.00024),
    ("PEN", 0.27),
];

/// Annual figures outside this USD range are treated as bad data (typos, unit mix-ups).
const MIN_PLAUSIBLE_USD: f64 = 1_000.0;
const MAX_PLAUSIBLE_USD: f64 = 5_000_000.0;

/// Working periods per year for each pay period.
fn periods_per_year(period: &str) -> Option<f64> {
    return match period {
        "year" => Some(1.0),
        "month" => Some(12.0),
        "week" => Some(52.0),
        "day" => Some(260.0),
        "hour" => Some(2080.0),
        _ => None,
    };
}

/// Converts an amount per `period` to a year. Without a period, a small number is taken as
/// hourly pay and a large one as annual.
pub fn annualize(amount: f64, period: Option<&str>) -> Option<f64> {
    if !amount.is_finite() || amount <= 0.0 {
        return None;
    }
    let factor = match period {
        Some(p) => periods_per_year(p)?,
        None if amount < 1000.0 => 2080.0,
        None => 1.0,
    };
    return Some(amount * factor);
}

pub fn to_usd(amount: f64, currency: &str) -> Option<f64> {
    let code = currency.trim().to_uppercase();
    let rate = USD_PER_UNIT.iter().find(|(c, _)| *c == code)?.1;
    return Some(amount * rate);
}

/// The annual salary range and its USD midpoint.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Salary {
    pub annual_min: Option<f64>,
    pub annual_max: Option<f64>,
    pub usd_annual: Option<f64>,
}

pub fn normalize(
    min: Option<f64>,
    max: Option<f64>,
    currency: Option<&str>,
    period: Option<&str>,
) -> Salary {
    let annual_min = min.and_then(|v| annualize(v, period));
    let annual_max = max.and_then(|v| annualize(v, period));
    let mid = match (annual_min, annual_max) {
        (Some(a), Some(b)) => Some((a + b) / 2.0),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    let usd_annual = mid
        .zip(currency)
        .and_then(|(m, c)| to_usd(m, c))
        .filter(|usd| (MIN_PLAUSIBLE_USD..=MAX_PLAUSIBLE_USD).contains(usd));
    return Salary {
        annual_min,
        annual_max,
        usd_annual,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_become_years() {
        assert_eq!(annualize(5000.0, Some("month")), Some(60_000.0));
        assert_eq!(annualize(50.0, Some("hour")), Some(104_000.0));
        assert_eq!(annualize(90_000.0, Some("year")), Some(90_000.0));
        assert_eq!(annualize(40.0, None), Some(83_200.0), "small = hourly");
        assert_eq!(annualize(90_000.0, None), Some(90_000.0));
        assert_eq!(annualize(10.0, Some("fortnight")), None);
        assert_eq!(annualize(-1.0, Some("year")), None);
    }

    #[test]
    fn usd_uses_the_midpoint_and_the_currency() {
        let s = normalize(Some(100_000.0), Some(140_000.0), Some("eur"), Some("year"));
        assert_eq!(s.annual_min, Some(100_000.0));
        assert_eq!(s.usd_annual, Some(120_000.0 * 1.08));
        assert_eq!(
            normalize(Some(1.0), None, Some("XXX"), Some("year")).usd_annual,
            None,
            "unknown currency"
        );
        assert_eq!(
            normalize(Some(90_000.0), None, None, Some("year")).usd_annual,
            None,
            "no currency"
        );
    }

    #[test]
    fn implausible_amounts_have_no_usd_value() {
        // Naira quoted as if it were dollars-scale: 90 NGN a year is nonsense.
        assert_eq!(
            normalize(Some(90.0), None, Some("NGN"), Some("year")).usd_annual,
            None
        );
        assert_eq!(
            normalize(Some(9e9), None, Some("USD"), Some("year")).usd_annual,
            None
        );
    }
}
