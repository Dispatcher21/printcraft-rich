//! Date picture clauses (XFA 3.3 Part 2 "Picture Clause Specification", date symbols only):
//! show a canonical date (`YYYY-MM-DD`) the way the field's display picture says, and read what
//! the user typed back into canonical form.

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// The date pattern inside a picture such as `date{MM/DD/YYYY}` or `date(en_US){D MMM YYYY}`.
pub fn date_pattern(picture: &str) -> Option<String> {
    let p = picture.trim();
    let rest = p.strip_prefix("date")?;
    let open = rest.find('{')?;
    let close = rest.rfind('}')?;
    let body = rest.get(open + 1..close)?.trim();
    (!body.is_empty() && close > open).then(|| body.to_string())
}

#[derive(Debug, PartialEq)]
enum Tok {
    Year4,
    Year2,
    MonthName,
    MonthAbbr,
    Month2,
    Month1,
    Day2,
    Day1,
    Lit(String),
}

fn tokens(pattern: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let run = chars.get(i..).map(|r| r.iter().take_while(|x| **x == c).count()).unwrap_or(1);
        match c {
            'Y' => {
                out.push(if run >= 4 { Tok::Year4 } else { Tok::Year2 });
            }
            'M' => out.push(match run {
                1 => Tok::Month1,
                2 => Tok::Month2,
                3 => Tok::MonthAbbr,
                _ => Tok::MonthName,
            }),
            'D' => out.push(if run >= 2 { Tok::Day2 } else { Tok::Day1 }),
            // Weekdays, eras and Julian days need more than this reader knows.
            'E' | 'G' | 'J' | 'w' | 'W' => return None,
            '\'' => {
                let end = chars.get(i + 1..)?.iter().position(|x| *x == '\'')?;
                let lit: String = chars.get(i + 1..i + 1 + end)?.iter().collect();
                out.push(Tok::Lit(lit));
                i += end + 2;
                continue;
            }
            _ => out.push(Tok::Lit(std::iter::repeat_n(c, run).collect())),
        }
        i += run;
    }
    Some(out)
}

/// Parse a canonical date (`YYYY-MM-DD` or `YYYYMMDD`).
pub fn parse_canonical(s: &str) -> Option<(i32, u32, u32)> {
    let s = s.trim();
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    let ok_shape = (s.len() == 10 && s.as_bytes().get(4) == Some(&b'-') && s.as_bytes().get(7) == Some(&b'-')) || (s.len() == 8 && digits.len() == 8);
    if !ok_shape || digits.len() != 8 {
        return None;
    }
    let y = digits.get(0..4)?.parse().ok()?;
    let m = digits.get(4..6)?.parse().ok()?;
    let d = digits.get(6..8)?.parse().ok()?;
    valid(y, m, d).then_some((y, m, d))
}

fn valid(y: i32, m: u32, d: u32) -> bool {
    (1..=12).contains(&m) && (1..=31).contains(&d) && (1..=9999).contains(&y)
}

/// Format a canonical date for display (`None` if either side isn't understood).
pub fn format(canonical: &str, pattern: &str) -> Option<String> {
    let (y, m, d) = parse_canonical(canonical)?;
    let mut out = String::new();
    for t in tokens(pattern)? {
        match t {
            Tok::Year4 => out.push_str(&format!("{y:04}")),
            Tok::Year2 => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            Tok::MonthName => out.push_str(MONTHS.get(m as usize - 1)?),
            Tok::MonthAbbr => out.push_str(MONTHS.get(m as usize - 1)?.get(..3)?),
            Tok::Month2 => out.push_str(&format!("{m:02}")),
            Tok::Month1 => out.push_str(&m.to_string()),
            Tok::Day2 => out.push_str(&format!("{d:02}")),
            Tok::Day1 => out.push_str(&d.to_string()),
            Tok::Lit(l) => out.push_str(&l),
        }
    }
    Some(out)
}

/// Read a displayed date back to canonical `YYYY-MM-DD` (`None` if it doesn't match).
pub fn parse(display: &str, pattern: &str) -> Option<String> {
    let s: Vec<char> = display.trim().chars().collect();
    let mut i = 0usize;
    let (mut y, mut m, mut d) = (None, None, None);
    let num = |i: &mut usize, min: usize, max: usize| -> Option<u32> {
        let start = *i;
        while *i < s.len() && *i - start < max && s.get(*i).is_some_and(|c| c.is_ascii_digit()) {
            *i += 1;
        }
        if *i - start < min {
            return None;
        }
        s.get(start..*i)?.iter().collect::<String>().parse().ok()
    };
    for t in tokens(pattern)? {
        match t {
            Tok::Year4 => y = Some(num(&mut i, 4, 4)? as i32),
            Tok::Year2 => {
                let v = num(&mut i, 2, 2)? as i32;
                // XFA's century split: two-digit years below 30 are 20xx.
                y = Some(if v < 30 { 2000 + v } else { 1900 + v });
            }
            Tok::Month2 => m = Some(num(&mut i, 1, 2)?),
            Tok::Month1 => m = Some(num(&mut i, 1, 2)?),
            Tok::Day2 => d = Some(num(&mut i, 1, 2)?),
            Tok::Day1 => d = Some(num(&mut i, 1, 2)?),
            Tok::MonthName | Tok::MonthAbbr => {
                let rest: String = s.get(i..)?.iter().collect::<String>().to_ascii_lowercase();
                let (k, len) = MONTHS.iter().enumerate().find_map(|(k, name)| {
                    let full = name.to_ascii_lowercase();
                    if rest.starts_with(&full) {
                        Some((k, full.chars().count()))
                    } else if rest.starts_with(full.get(..3)?) {
                        Some((k, 3))
                    } else {
                        None
                    }
                })?;
                m = Some(k as u32 + 1);
                i += len;
            }
            Tok::Lit(l) => {
                for c in l.chars() {
                    // Separators are matched loosely: any one non-alphanumeric stands for another.
                    match s.get(i) {
                        Some(x) if *x == c || (!x.is_alphanumeric() && !c.is_alphanumeric()) => i += 1,
                        _ if c == ' ' => {}
                        _ => return None,
                    }
                }
            }
        }
    }
    if i != s.len() {
        return None;
    }
    let (y, m, d) = (y?, m?, d?);
    valid(y, m, d).then(|| format!("{y:04}-{m:02}-{d:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let p = date_pattern("date{MM/DD/YYYY}").unwrap_or_default();
        assert_eq!(format("2026-10-04", &p).as_deref(), Some("10/04/2026"));
        assert_eq!(parse("10/04/2026", &p).as_deref(), Some("2026-10-04"));
        assert_eq!(parse("10-4-2026", &p).as_deref(), Some("2026-10-04"));
        assert_eq!(parse("13/04/2026", &p), None);
        let p = date_pattern("date(en_US){DD MMM YYYY}").unwrap_or_default();
        assert_eq!(format("20261004", &p).as_deref(), Some("04 Oct 2026"));
        assert_eq!(parse("4 October 2026", &p).as_deref(), Some("2026-10-04"));
        let p = "YYYYMMDD";
        assert_eq!(parse("20261004", p).as_deref(), Some("2026-10-04"));
        assert_eq!(date_pattern("num{z9}"), None);
        assert_eq!(format("x", "EEE"), None);
        let _ = parse("", "'");
    }
}
