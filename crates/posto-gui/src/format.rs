//! Pure display helpers: avatars, dates, sizes, address labels.

use std::fmt::Display;

#[cfg(test)]
use chrono::FixedOffset;
use chrono::{DateTime, Local, TimeZone, Utc};
use posto_core::store::Address;

/// Avatar letters: first letters of the first two words, or the first
/// letter of an email's local part. `"?"` when there is nothing to use.
pub fn initials(name_or_email: &str) -> String {
    let s = name_or_email.trim();
    let first = |w: &str| w.chars().find(|c| c.is_alphanumeric());

    let letters: String = if s.contains('@') && !s.contains(char::is_whitespace) {
        s.split('@').next().and_then(first).into_iter().collect()
    } else {
        s.split_whitespace().take(2).filter_map(first).collect()
    };

    if letters.is_empty() {
        "?".to_string()
    } else {
        letters.to_uppercase()
    }
}

/// Stable hue in `0..360` for an avatar colour (FNV-1a hash of `seed`).
pub fn avatar_hue(seed: &str) -> u16 {
    let hash = seed.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });

    (hash % 360) as u16
}

/// List-row date: `HH:MM` today, a weekday name within six days, else `D/M/YYYY`.
/// Unparsable input is returned as is.
pub fn short_date(rfc3339: &str, now: DateTime<Utc>) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.to_string();
    };
    let when = parsed.with_timezone(&Local);
    let days = now
        .with_timezone(&Local)
        .date_naive()
        .signed_duration_since(when.date_naive())
        .num_days();

    match days {
        0 => when.format("%H:%M").to_string(),
        1..=6 => when.format("%A").to_string(),
        _ => when.format("%-d/%-m/%Y").to_string(),
    }
}

/// Reader date like `29 Aug 2026 at 17.00` in the local time zone.
///
/// The offset is the one in force at the email's own time, so winter and
/// summer mail both match the list dates.
pub fn long_date(rfc3339: &str) -> String {
    long_date_with(rfc3339, &Local)
}

/// [`long_date`] in a fixed offset, for testing.
#[cfg(test)]
pub fn long_date_in(rfc3339: &str, offset: FixedOffset) -> String {
    long_date_with(rfc3339, &offset)
}

/// [`long_date`] in the time zone `tz`.
fn long_date_with<Tz>(rfc3339: &str, tz: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(parsed) => parsed
            .with_timezone(tz)
            .format("%-d %b %Y at %-H.%M")
            .to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

/// `s` cut to `n` characters, the last being `…` when anything was removed.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }

    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();

    out.push('…');
    out
}

/// Byte count as `34 kB` / `1.2 MB`.
pub fn size(bytes: i64) -> String {
    let b = bytes.max(0) as f64;

    if b < 1000.0 {
        format!("{bytes} B")
    } else if b < 1_000_000.0 {
        format!("{:.0} kB", b / 1000.0)
    } else {
        format!("{:.1} MB", b / 1_000_000.0)
    }
}

/// Display name of an address: its name, else the email.
pub fn who(a: &Address) -> &str {
    match a.name.as_deref() {
        Some(name) if !name.trim().is_empty() => name,
        _ => &a.email,
    }
}

/// `Name <email>` for editing in a header field.
pub fn address_text(a: &Address) -> String {
    match a.name.as_deref() {
        Some(name) if !name.trim().is_empty() => format!("{name} <{}>", a.email),
        _ => a.email.clone(),
    }
}

/// Comma-separated [`address_text`] list.
pub fn addresses_text(list: &[Address]) -> String {
    list.iter().map(address_text).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn initials_cases() {
        assert_eq!(initials("Building Management"), "BM");
        assert_eq!(initials("Linear"), "L");
        assert_eq!(initials("ho@x.io"), "H");
        assert_eq!(initials(""), "?");
    }

    #[test]
    fn short_date_buckets() {
        let now = Utc::now();
        let local = |d: DateTime<Utc>, fmt: &str| d.with_timezone(&Local).format(fmt).to_string();
        let three = now - Duration::days(3);
        let thirty = now - Duration::days(30);

        assert_eq!(short_date(&now.to_rfc3339(), now), local(now, "%H:%M"));
        assert_eq!(short_date(&three.to_rfc3339(), now), local(three, "%A"));
        assert_eq!(
            short_date(&thirty.to_rfc3339(), now),
            local(thirty, "%-d/%-m/%Y")
        );
        assert_eq!(short_date("junk", now), "junk");
    }

    #[test]
    fn long_date_format() {
        let plus_two = FixedOffset::east_opt(2 * 3600).unwrap();

        assert_eq!(
            long_date_in("2026-08-29T15:00:00Z", plus_two),
            "29 Aug 2026 at 17.00"
        );
    }

    /// A zone with daylight saving: UTC+1 from April to October, else UTC.
    #[derive(Clone, Copy)]
    struct SummerTime;

    #[derive(Clone, Copy, Debug)]
    struct SummerOffset(FixedOffset);

    impl std::fmt::Display for SummerOffset {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }

    impl chrono::Offset for SummerOffset {
        fn fix(&self) -> FixedOffset {
            self.0
        }
    }

    impl TimeZone for SummerTime {
        type Offset = SummerOffset;

        fn from_offset(_: &SummerOffset) -> Self {
            SummerTime
        }

        fn offset_from_local_date(
            &self,
            _: &chrono::NaiveDate,
        ) -> chrono::LocalResult<SummerOffset> {
            unreachable!("only UTC to local conversion is used")
        }

        fn offset_from_local_datetime(
            &self,
            _: &chrono::NaiveDateTime,
        ) -> chrono::LocalResult<SummerOffset> {
            unreachable!("only UTC to local conversion is used")
        }

        fn offset_from_utc_date(&self, d: &chrono::NaiveDate) -> SummerOffset {
            self.offset_from_utc_datetime(&d.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, dt: &chrono::NaiveDateTime) -> SummerOffset {
            use chrono::Datelike;

            let hours = if (4..=10).contains(&dt.month()) { 1 } else { 0 };

            SummerOffset(FixedOffset::east_opt(hours * 3600).unwrap())
        }
    }

    #[test]
    fn long_date_uses_offset_in_force_at_that_time() {
        assert_eq!(
            long_date_with("2026-01-15T15:00:00Z", &SummerTime),
            "15 Jan 2026 at 15.00"
        );
        assert_eq!(
            long_date_with("2026-07-15T15:00:00Z", &SummerTime),
            "15 Jul 2026 at 16.00"
        );
    }

    #[test]
    fn avatar_hue_stable_and_in_range() {
        assert_eq!(avatar_hue("Linear"), avatar_hue("Linear"));
        assert_ne!(avatar_hue("Linear"), avatar_hue("Mailhost"));

        for seed in ["", "a", "Birgitte Holm", "ho@x.io", "ÆØÅ"] {
            assert!(avatar_hue(seed) < 360);
        }
    }
}
