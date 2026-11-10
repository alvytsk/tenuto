//! Jump-to-time grammar (M13 §1.2): `SS`, `MM:SS`, `HH:MM:SS`, an optional
//! fraction on the last part. Each part is one or two digits: the spec's
//! own example, `1:23:00`, has a one-digit hour, so two-digits-strict
//! would refuse the documented input. A malformed time stays in the box
//! with an error line rather than seeking somewhere unexpected.

use std::time::Duration;

use crate::application::view::PlayerView;
use crate::queue::QueueEntryId;

/// Parses `raw` against the strict grammar. A value past the track's
/// duration is clamped at the landing by the engine, so no range rule
/// exists here beyond the clock's own.
pub fn parse_hms(raw: &str) -> Result<Duration, &'static str> {
    const MALFORMED: &str = "Use SS, MM:SS or HH:MM:SS";
    let (body, fraction) = match raw.rsplit_once('.') {
        Some((body, fraction)) => (body, Some(fraction)),
        None => (raw, None),
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| !is_digits_of(p, 1, 2)) {
        return Err(MALFORMED);
    }
    if let Some(fraction) = fraction
        && (fraction.is_empty()
            || fraction.len() > 3
            || !fraction.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(MALFORMED);
    }
    let number = |part: &str| -> u64 {
        part.bytes()
            .fold(0u64, |acc, b| acc * 10 + u64::from(b - b'0'))
    };
    let Some(seconds) = parts.last() else {
        return Err(MALFORMED);
    };
    if number(seconds) >= 60 {
        return Err("Seconds must be 00-59");
    }
    let minutes = parts.len().checked_sub(2).map(|i| number(parts[i]));
    if minutes.is_some_and(|m| m >= 60) {
        return Err("Minutes must be 00-59");
    }
    let hours = parts.len().checked_sub(3).map(|i| number(parts[i]));
    let total = hours
        .unwrap_or(0)
        .checked_mul(3600)
        .and_then(|h| h.checked_add(minutes.unwrap_or(0).checked_mul(60)?))
        .and_then(|m| m.checked_add(number(seconds)))
        .ok_or("That time is too long")?;
    let nanos = fraction.map_or(0u32, |f| {
        let digits = f
            .bytes()
            .fold(0u32, |acc, b| acc * 10 + u32::from(b - b'0'));
        digits * 10u32.pow(3 - f.len() as u32) * 1_000_000
    });
    Ok(Duration::new(total, nanos))
}

fn is_digits_of(part: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit())
}

/// Jump-to-file matching (M13 §1.4): a case-insensitive substring over
/// "artist title", the same text the row shows, so what you filter is what
/// you see. An empty filter matches everything, in row order.
pub fn file_matches(view: &PlayerView, filter: &str) -> Vec<QueueEntryId> {
    let needle = filter.to_lowercase();
    if needle.is_empty() {
        return view.rows.iter().map(|row| row.id).collect();
    }
    view.rows
        .iter()
        .filter(|row| {
            let artist = row.subtitle.as_deref().unwrap_or("");
            format!("{artist} {}", row.title)
                .to_lowercase()
                .contains(&needle)
        })
        .map(|row| row.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_hms;
    use std::time::Duration;

    #[test]
    fn the_grammar_accepts_each_form() {
        // One-digit parts are valid: the spec's own example has one.
        assert_eq!(parse_hms("1:23:00"), Ok(Duration::from_secs(4980)));
        assert_eq!(parse_hms("1:2"), Ok(Duration::from_secs(62)));
        assert_eq!(parse_hms("1:2:3"), Ok(Duration::from_secs(3723)));
        assert_eq!(parse_hms("07"), Ok(Duration::from_secs(7)));
        assert_eq!(parse_hms("07.5"), Ok(Duration::new(7, 500_000_000)));
        assert_eq!(parse_hms("01:07"), Ok(Duration::from_secs(67)));
        assert_eq!(parse_hms("01:07.25"), Ok(Duration::new(67, 250_000_000)));
        assert_eq!(parse_hms("99:59:59"), Ok(Duration::from_secs(359_999)));
        assert_eq!(parse_hms("00:00.001"), Ok(Duration::new(0, 1_000_000)));
    }

    #[test]
    fn the_grammar_rejects_everything_else() {
        for raw in [
            "", "007", ":07", "07:", "1:07:", "0:07.", "07.25.5", "01:60", "01:23:60", "a:07",
            "+7", "-7", " 7", "07 ", "01.5:07", "07.1234",
        ] {
            assert!(parse_hms(raw).is_err(), "{raw:?} must not parse");
        }
        assert_eq!(parse_hms("1:07:60"), Err("Seconds must be 00-59"));
        assert_eq!(parse_hms("60:07"), Err("Minutes must be 00-59"));
        assert_eq!(parse_hms("1:60:07"), Err("Minutes must be 00-59"));
    }
}
