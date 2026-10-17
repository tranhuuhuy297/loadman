//! Dynamic variables (`{{$guid}}`, `{{$randomInt}}`, ...).
//!
//! Names follow the publicly documented Postman dynamic variable list. Only the
//! commonly used subset is implemented; unknown `$names` stay unresolved.

use std::fmt::Write;

const FIRST: &[&str] = &[
    "Ada",
    "Alan",
    "Grace",
    "Linus",
    "Margaret",
    "Dennis",
    "Barbara",
    "Ken",
    "Frances",
    "Edsger",
    "Radia",
    "Donald",
    "Hedy",
    "Tim",
    "Katherine",
    "Guido",
    "Anita",
    "Bjarne",
    "Lynn",
    "Yukihiro",
];
const LAST: &[&str] = &[
    "Lovelace",
    "Turing",
    "Hopper",
    "Torvalds",
    "Hamilton",
    "Ritchie",
    "Liskov",
    "Thompson",
    "Allen",
    "Dijkstra",
    "Perlman",
    "Knuth",
    "Lamarr",
    "Berners",
    "Johnson",
    "Rossum",
    "Borg",
    "Stroustrup",
    "Conway",
    "Matsumoto",
];
const WORDS: &[&str] = &[
    "alpha", "bravo", "cobalt", "delta", "ember", "falcon", "garnet", "harbor", "indigo", "juniper", "kernel", "lumen",
    "matrix", "nimbus", "orbit", "prism", "quartz", "raven", "sierra", "tundra",
];
const CITIES: &[&str] =
    &["Hanoi", "Tokyo", "Berlin", "Lisbon", "Austin", "Nairobi", "Toronto", "Seoul", "Paris", "Lima"];
const COUNTRIES: &[&str] =
    &["Vietnam", "Japan", "Germany", "Portugal", "United States", "Kenya", "Canada", "Korea", "France", "Peru"];
const COLORS: &[&str] = &["red", "green", "blue", "cyan", "magenta", "yellow", "black", "white", "orange", "purple"];
const DOMAINS: &[&str] = &["example.com", "example.org", "example.net", "test.dev"];

fn pick(list: &[&'static str]) -> &'static str {
    list[fastrand::usize(..list.len())]
}

pub fn uuid_v4() -> String {
    let mut b: [u8; 16] = fastrand::u128(..).to_le_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{byte:02x}");
    }
    s
}

fn alnum(n: usize) -> String {
    (0..n).map(|_| fastrand::alphanumeric().to_ascii_lowercase()).collect()
}

/// Generate the value for a dynamic variable name without its leading `$`.
pub fn generate(name: &str) -> Option<String> {
    let now = chrono::Utc::now();
    Some(match name {
        "guid" | "randomUUID" => uuid_v4(),
        "timestamp" => now.timestamp().to_string(),
        "isoTimestamp" => now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "randomInt" => fastrand::u32(0..=1000).to_string(),
        "randomBoolean" => fastrand::bool().to_string(),
        "randomAlphaNumeric" => fastrand::alphanumeric().to_ascii_lowercase().to_string(),
        "randomFirstName" => pick(FIRST).to_string(),
        "randomLastName" => pick(LAST).to_string(),
        "randomFullName" => format!("{} {}", pick(FIRST), pick(LAST)),
        "randomUserName" => format!("{}.{}{}", pick(FIRST), pick(LAST), fastrand::u16(1..999)).to_lowercase(),
        "randomEmail" | "randomExampleEmail" => format!(
            "{}.{}{}@{}",
            pick(FIRST).to_lowercase(),
            pick(LAST).to_lowercase(),
            fastrand::u16(1..999),
            pick(DOMAINS)
        ),
        "randomPassword" => alnum(15),
        "randomPhoneNumber" => {
            format!("{:03}-{:03}-{:04}", fastrand::u16(200..999), fastrand::u16(100..999), fastrand::u16(0..9999))
        }
        "randomIP" => {
            format!("{}.{}.{}.{}", fastrand::u8(1..=254), fastrand::u8(..), fastrand::u8(..), fastrand::u8(1..=254))
        }
        "randomColor" => pick(COLORS).to_string(),
        "randomHexColor" => format!("#{:06x}", fastrand::u32(..0x0100_0000)),
        "randomWord" | "randomNoun" | "randomAdjective" => pick(WORDS).to_string(),
        "randomWords" => (0..3).map(|_| pick(WORDS)).collect::<Vec<_>>().join(" "),
        "randomCity" => pick(CITIES).to_string(),
        "randomCountry" => pick(COUNTRIES).to_string(),
        "randomPrice" => format!("{}.{:02}", fastrand::u32(1..1000), fastrand::u8(0..100)),
        "randomDateFuture" => (now + chrono::Duration::days(fastrand::i64(1..365))).to_rfc2822(),
        "randomDatePast" => (now - chrono::Duration::days(fastrand::i64(1..365))).to_rfc2822(),
        "randomDateRecent" => (now - chrono::Duration::hours(fastrand::i64(1..72))).to_rfc2822(),
        "randomLoremWord" => pick(WORDS).to_string(),
        "randomUrl" => format!("https://{}.{}", pick(WORDS), pick(DOMAINS)),
        "randomDomainName" => format!("{}.{}", pick(WORDS), pick(DOMAINS)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn known_and_unknown() {
        assert!(generate("randomEmail").unwrap().contains('@'));
        assert!(generate("randomInt").unwrap().parse::<u32>().unwrap() <= 1000);
        assert!(generate("notAThing").is_none());
    }
}
