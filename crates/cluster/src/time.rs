//! Durations (`250ms`, `10s`, `5m`, `2h`, `1d`) and rates (`N/duration`) as
//! written in cluster files.

use std::fmt;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dur(pub Duration);

impl Dur {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| format!("duration {s:?} needs a unit"))?;
        let (n, unit) = s.split_at(split);
        let n: u64 = n.parse().map_err(|_| format!("bad duration {s:?}"))?;
        let d = match unit {
            "ms" => Duration::from_millis(n),
            "s" => Duration::from_secs(n),
            "m" => Duration::from_secs(n * 60),
            "h" => Duration::from_secs(n * 3600),
            "d" => Duration::from_secs(n * 86400),
            _ => return Err(format!("bad unit in duration {s:?} (ms, s, m, h, d)")),
        };
        if d.is_zero() {
            return Err(format!("duration {s:?} must be positive"));
        }
        Ok(Dur(d))
    }
}

impl fmt::Display for Dur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis();
        for (unit, per) in [("d", 86_400_000), ("h", 3_600_000), ("m", 60_000), ("s", 1000)] {
            if ms.is_multiple_of(per) {
                return write!(f, "{}{unit}", ms / per);
            }
        }
        write!(f, "{ms}ms")
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Dur::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Dur {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Duration".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "pattern": "^[0-9]+(ms|s|m|h|d)$"})
    }
}

/// At most `n` per `per`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub n: u32,
    pub per: Dur,
}

impl Rate {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (n, per) = s.split_once('/').ok_or_else(|| format!("rate {s:?} must look like N/duration"))?;
        let n: u32 = n.trim().parse().map_err(|_| format!("bad count in rate {s:?}"))?;
        if n == 0 {
            return Err(format!("rate {s:?} must allow at least 1"));
        }
        Ok(Rate { n, per: Dur::parse(per)? })
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.n, self.per)
    }
}

impl Serialize for Rate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Rate::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Rate {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Rate".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "pattern": "^[0-9]+/[0-9]+(ms|s|m|h|d)$"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(Dur::parse("250ms").unwrap().0, Duration::from_millis(250));
        assert_eq!(Dur::parse("10s").unwrap().0, Duration::from_secs(10));
        assert_eq!(Dur::parse("5m").unwrap().0, Duration::from_secs(300));
        assert_eq!(Dur::parse("1d").unwrap().to_string(), "1d");
        assert_eq!(Dur::parse("90s").unwrap().to_string(), "90s");
        assert_eq!(Dur::parse("120s").unwrap().to_string(), "2m");
        for bad in ["", "10", "s", "10w", "0s", "-1s", "1.5s"] {
            assert!(Dur::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rates() {
        let r = Rate::parse("1/10s").unwrap();
        assert_eq!((r.n, r.per.0), (1, Duration::from_secs(10)));
        assert_eq!(r.to_string(), "1/10s");
        for bad in ["10s", "0/1s", "x/1s", "1/"] {
            assert!(Rate::parse(bad).is_err(), "{bad}");
        }
    }
}
