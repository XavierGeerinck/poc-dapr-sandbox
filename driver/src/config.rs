//! Component metadata, parsed exactly like Go `sandbox.ParseConfig`
//! (`upstream/dapr/pkg/components/sandbox/config.go`).
//!
//! daprd parses the same properties with the Go parser before it calls the
//! driver's `Init`, so this parser must accept everything the Go one accepts
//! and reject the same inputs.
//!
//! One key is the driver's own: `warmupCommand` (see
//! [`Config::warmup_command`]), off unless set. The Go parser ignores keys
//! it does not know, and daprd passes every metadata item to `Init`. Any
//! value parses.

use std::collections::HashMap;
use std::time::Duration;

/// How pool VMs are created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    /// Boot one VM, run `setup`, snapshot it, and restore pool VMs from it.
    Snapshot,
    /// Boot every pool VM from scratch (and run `setup` in each).
    None,
}

/// Parsed component metadata of a sandbox component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub image: String,
    pub cpus: u32,
    pub memory_mib: u32,
    pub setup: String,
    pub warm_pool_size: u32,
    /// Maximum acquired sandboxes; 0 means unlimited (as in the Go driver).
    pub max_sandboxes_per_host: u32,
    pub refill_concurrency: u32,
    /// Go accepts negative durations; they are clamped to zero here. daprd,
    /// not the driver, enforces the idle timeout.
    pub idle_timeout: Duration,
    pub template: Template,
    /// `warmupCommand`: the argv the pool runs once in each VM it refills
    /// the warm pool with, after spawning it and before handing it out; a
    /// VM whose warm-up fails is destroyed and replaced. Empty means no
    /// warm-up, and that is the default (ruling R22a): on microsandbox
    /// 0.7.11 the warm-up does not avoid the slow exec after ~25 ms of VM
    /// idle, and it costs about 30 ms per refill. See the pool's module docs
    /// (`pool`, "Warm-up") for what it does and does not save.
    ///
    /// The value is split on whitespace into argv, which maps onto
    /// `ExecSpec { command: argv[0], args: argv[1..] }` with no shell, no
    /// quoting and no expansion: `true` runs the image's `true` directly,
    /// without a shell. A value of only whitespace splits to nothing and
    /// also disables it. Work that needs a shell belongs in `setup`. The
    /// command must exist in the image and exit 0 within the pool's warm-up
    /// timeout (30 s by default); otherwise every refill fails and the pool
    /// never becomes ready.
    ///
    /// Absent: [`DEFAULT_WARMUP_COMMAND`] (disabled). Empty: disabled.
    pub warmup_command: Vec<String>,
}

/// The `warmupCommand` when the key is absent: empty, so no warm-up
/// (ruling R22a). Set it, for example to `true`, to enable one.
pub const DEFAULT_WARMUP_COMMAND: &str = "";

/// A metadata error. The messages match the Go ones.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("sandbox: metadata 'image' is required")]
    MissingImage,
    #[error("sandbox: invalid metadata '{name}' {value:?}: {reason}")]
    Invalid {
        name: &'static str,
        value: String,
        reason: String,
    },
    #[error("sandbox: invalid metadata 'template' {0:?}: must be 'snapshot' or 'none'")]
    InvalidTemplate(String),
}

impl Config {
    /// Parses component metadata properties, applying defaults.
    pub fn from_properties(props: &HashMap<String, String>) -> Result<Config, ConfigError> {
        let get = |k: &str| props.get(k).map(String::as_str).unwrap_or("");
        let mut c = Config {
            image: get("image").to_string(),
            cpus: 1,
            memory_mib: 512,
            setup: get("setup").to_string(),
            warm_pool_size: 4,
            max_sandboxes_per_host: 32,
            refill_concurrency: 2,
            idle_timeout: Duration::from_secs(5 * 60),
            template: Template::Snapshot,
            warmup_command: props
                .get("warmupCommand")
                .map_or(DEFAULT_WARMUP_COMMAND, String::as_str)
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        };
        if c.image.is_empty() {
            return Err(ConfigError::MissingImage);
        }

        for (name, dst) in [
            ("cpus", &mut c.cpus),
            ("memoryMiB", &mut c.memory_mib),
            ("warmPoolSize", &mut c.warm_pool_size),
            ("maxSandboxesPerHost", &mut c.max_sandboxes_per_host),
            ("refillConcurrency", &mut c.refill_concurrency),
        ] {
            let v = get(name);
            if v.is_empty() {
                continue;
            }
            *dst = parse_go_uint32(v).map_err(|reason| ConfigError::Invalid {
                name,
                value: v.to_string(),
                reason,
            })?;
        }

        let v = get("idleTimeout");
        if !v.is_empty() {
            let ns = parse_go_duration(v).map_err(|reason| ConfigError::Invalid {
                name: "idleTimeout",
                value: v.to_string(),
                reason,
            })?;
            c.idle_timeout = Duration::from_nanos(ns.max(0).unsigned_abs());
        }

        match get("template") {
            "" | "snapshot" => {}
            "none" => c.template = Template::None,
            other => return Err(ConfigError::InvalidTemplate(other.to_string())),
        }

        Ok(c)
    }
}

/// Go `strconv.ParseUint(v, 10, 32)`: ASCII digits only (no sign, spaces or
/// underscores), at most `u32::MAX`. The error is Go's message.
fn parse_go_uint32(v: &str) -> Result<u32, String> {
    let err = |what: &str| format!("strconv.ParseUint: parsing {v:?}: {what}");
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err("invalid syntax"));
    }
    // All digits, so the only possible failure is overflow.
    v.parse::<u32>().map_err(|_| err("value out of range"))
}

/// Parses a duration with Go `time.ParseDuration` syntax and semantics:
/// `[-+]?([0-9]*(\.[0-9]*)?[a-z]+)+`, units ns, us, µs, μs, ms, s, m, h, and
/// the special case "0". Returns signed nanoseconds; the error is Go's
/// message. A line-by-line port of Go 1.26 `time.ParseDuration`.
pub fn parse_go_duration(orig: &str) -> Result<i64, String> {
    const LIMIT: u64 = 1 << 63;
    let invalid = || format!("time: invalid duration {orig:?}");

    let mut s = orig.as_bytes();
    let mut d: u64 = 0;
    let mut neg = false;

    // Consume [-+]?
    if let Some(&c) = s.first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        s = &s[1..];
    }
    // Special case: if all that is left is "0", this is zero.
    if s == b"0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }
    while !s.is_empty() {
        // The next character must be [0-9.]
        if !(s[0] == b'.' || s[0].is_ascii_digit()) {
            return Err(invalid());
        }
        // Consume [0-9]*
        let pl = s.len();
        let (mut v, rest) = leading_int(s).ok_or_else(invalid)?;
        s = rest;
        let pre = pl != s.len(); // whether we consumed anything before a period

        // Consume (\.[0-9]*)?
        let mut post = false;
        let (mut f, mut scale) = (0u64, 1f64);
        if s.first() == Some(&b'.') {
            s = &s[1..];
            let pl = s.len();
            (f, scale, s) = leading_fraction(s);
            post = pl != s.len();
        }
        if !pre && !post {
            // no digits (e.g. ".s" or "-.s")
            return Err(invalid());
        }

        // Consume unit.
        let i = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if i == 0 {
            return Err(format!("time: missing unit in duration {orig:?}"));
        }
        let (u, rest) = s.split_at(i);
        s = rest;
        let unit: u64 = match u {
            b"ns" => 1,
            b"us" => 1_000,
            b"\xc2\xb5s" => 1_000, // "µs", U+00B5 = micro symbol
            b"\xce\xbcs" => 1_000, // "μs", U+03BC = Greek letter mu
            b"ms" => 1_000_000,
            b"s" => 1_000_000_000,
            b"m" => 60 * 1_000_000_000,
            b"h" => 60 * 60 * 1_000_000_000,
            _ => {
                return Err(format!(
                    "time: unknown unit {:?} in duration {orig:?}",
                    String::from_utf8_lossy(u)
                ));
            }
        };
        if v > LIMIT / unit {
            // overflow
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            // float64 is needed to be nanosecond accurate for fractions of hours.
            // v >= 0 && (f*unit/scale) <= 3.6e+12 (ns/h, h is the largest unit)
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > LIMIT {
                // overflow
                return Err(invalid());
            }
        }
        d += v;
        if d > LIMIT {
            return Err(invalid());
        }
    }
    if neg {
        // d <= 1<<63, so -d fits in i64.
        return Ok((d as i64).wrapping_neg());
    }
    if d > LIMIT - 1 {
        return Err(invalid());
    }
    Ok(d as i64)
}

/// Consumes the leading `[0-9]*`; `None` on overflow past 1<<63.
fn leading_int(s: &[u8]) -> Option<(u64, &[u8])> {
    let mut x: u64 = 0;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        if x > (1 << 63) / 10 {
            return None;
        }
        x = x * 10 + u64::from(s[i] - b'0');
        if x > 1 << 63 {
            return None;
        }
        i += 1;
    }
    Some((x, &s[i..]))
}

/// Consumes the leading `[0-9]*` of a fraction. It never fails on overflow:
/// it just stops accumulating precision.
fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let mut x: u64 = 0;
    let mut scale = 1f64;
    let mut overflow = false;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        let c = s[i];
        i += 1;
        if overflow {
            continue;
        }
        if x > ((1u64 << 63) - 1) / 10 {
            // It's possible for overflow to give a positive number, so take care.
            overflow = true;
            continue;
        }
        let y = x * 10 + u64::from(c - b'0');
        if y > 1 << 63 {
            overflow = true;
            continue;
        }
        x = y;
        scale *= 10.0;
    }
    (x, scale, &s[i..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(kv: &[(&str, &str)]) -> HashMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults() {
        let c = Config::from_properties(&props(&[("image", "x")])).unwrap();
        assert_eq!(
            c,
            Config {
                image: "x".into(),
                cpus: 1,
                memory_mib: 512,
                setup: String::new(),
                warm_pool_size: 4,
                max_sandboxes_per_host: 32,
                refill_concurrency: 2,
                idle_timeout: Duration::from_secs(300),
                template: Template::Snapshot,
                warmup_command: Vec::new(),
            }
        );
    }

    #[test]
    fn warmup_command() {
        let argv = |v: &str| {
            Config::from_properties(&props(&[("image", "x"), ("warmupCommand", v)]))
                .unwrap()
                .warmup_command
        };
        // Absent: disabled (R22a).
        let c = Config::from_properties(&props(&[("image", "x")])).unwrap();
        assert!(c.warmup_command.is_empty());
        // Explicitly empty, or only whitespace: disabled.
        assert!(argv("").is_empty());
        assert!(argv(" \t\n ").is_empty());
        // Split on whitespace into argv; no shell, no quoting.
        assert_eq!(argv("true"), vec!["true"]);
        assert_eq!(argv("  echo  warm\tup \n"), vec!["echo", "warm", "up"]);
        assert_eq!(
            argv(r#"sh -c "true""#),
            vec!["sh", "-c", r#""true""#],
            "quotes are not interpreted"
        );
    }

    #[test]
    fn missing_image() {
        assert_eq!(
            Config::from_properties(&props(&[])),
            Err(ConfigError::MissingImage)
        );
        assert_eq!(
            Config::from_properties(&props(&[("image", "")])),
            Err(ConfigError::MissingImage)
        );
    }

    #[test]
    fn idle_timeout() {
        let c = Config::from_properties(&props(&[("image", "x"), ("idleTimeout", "90s")])).unwrap();
        assert_eq!(c.idle_timeout, Duration::from_secs(90));

        let err = Config::from_properties(&props(&[("image", "x"), ("idleTimeout", "soon")]))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"sandbox: invalid metadata 'idleTimeout' "soon": time: invalid duration "soon""#
        );

        // Go accepts negative durations; daprd parses first, so we must too.
        let c = Config::from_properties(&props(&[("image", "x"), ("idleTimeout", "-5m")])).unwrap();
        assert_eq!(c.idle_timeout, Duration::ZERO);

        // An empty value keeps the default, as in Go.
        let c = Config::from_properties(&props(&[("image", "x"), ("idleTimeout", "")])).unwrap();
        assert_eq!(c.idle_timeout, Duration::from_secs(300));
    }

    #[test]
    fn template() {
        let c = Config::from_properties(&props(&[("image", "x"), ("template", "none")])).unwrap();
        assert_eq!(c.template, Template::None);
        let c =
            Config::from_properties(&props(&[("image", "x"), ("template", "snapshot")])).unwrap();
        assert_eq!(c.template, Template::Snapshot);
        let c = Config::from_properties(&props(&[("image", "x"), ("template", "")])).unwrap();
        assert_eq!(c.template, Template::Snapshot);

        let err =
            Config::from_properties(&props(&[("image", "x"), ("template", "bogus")])).unwrap_err();
        assert_eq!(err, ConfigError::InvalidTemplate("bogus".into()));
        assert_eq!(
            err.to_string(),
            r#"sandbox: invalid metadata 'template' "bogus": must be 'snapshot' or 'none'"#
        );
        // Case-sensitive, as in Go.
        assert!(Config::from_properties(&props(&[("image", "x"), ("template", "None")])).is_err());
    }

    #[test]
    fn numeric_fields() {
        let c = Config::from_properties(&props(&[
            ("image", "x"),
            ("cpus", "2"),
            ("memoryMiB", "1024"),
            ("setup", "echo hi"),
            ("warmPoolSize", "0"),
            ("maxSandboxesPerHost", "8"),
            ("refillConcurrency", "3"),
        ]))
        .unwrap();
        assert_eq!(c.cpus, 2);
        assert_eq!(c.memory_mib, 1024);
        assert_eq!(c.setup, "echo hi");
        assert_eq!(c.warm_pool_size, 0);
        assert_eq!(c.max_sandboxes_per_host, 8);
        assert_eq!(c.refill_concurrency, 3);

        // An empty value keeps the default.
        let c = Config::from_properties(&props(&[("image", "x"), ("cpus", "")])).unwrap();
        assert_eq!(c.cpus, 1);

        // Leading zeros are fine; u32::MAX is the largest accepted value.
        let c = Config::from_properties(&props(&[("image", "x"), ("cpus", "007")])).unwrap();
        assert_eq!(c.cpus, 7);
        let c = Config::from_properties(&props(&[("image", "x"), ("cpus", "4294967295")])).unwrap();
        assert_eq!(c.cpus, u32::MAX);

        for k in [
            "cpus",
            "memoryMiB",
            "warmPoolSize",
            "maxSandboxesPerHost",
            "refillConcurrency",
        ] {
            // Go strconv.ParseUint rejects signs, spaces, non-digits and
            // values above 32 bits.
            for v in [
                "-1",
                "abc",
                "+1",
                " 1",
                "1 ",
                "1.0",
                "4294967296",
                "0x10",
                "1_000",
            ] {
                let err = Config::from_properties(&props(&[("image", "x"), (k, v)])).unwrap_err();
                let msg = err.to_string();
                assert!(msg.contains(k), "{k}={v}: {msg}");
                assert!(
                    msg.starts_with(&format!(
                        "sandbox: invalid metadata '{k}' {v:?}: strconv.ParseUint: parsing {v:?}: "
                    )),
                    "{k}={v}: {msg}"
                );
            }
        }
        let err =
            Config::from_properties(&props(&[("image", "x"), ("cpus", "4294967296")])).unwrap_err();
        assert!(err.to_string().ends_with("value out of range"), "{err}");
        let err = Config::from_properties(&props(&[("image", "x"), ("cpus", "abc")])).unwrap_err();
        assert!(err.to_string().ends_with("invalid syntax"), "{err}");
    }

    #[test]
    fn go_durations() {
        const NS: i64 = 1;
        const US: i64 = 1_000;
        const MS: i64 = 1_000_000;
        const S: i64 = 1_000_000_000;
        const M: i64 = 60 * S;
        const H: i64 = 60 * M;
        // Cases from Go's time/time_test.go parseDurationTests.
        let ok: &[(&str, i64)] = &[
            ("0", 0),
            ("5s", 5 * S),
            ("30s", 30 * S),
            ("1478s", 1478 * S),
            ("-5s", -5 * S),
            ("+5s", 5 * S),
            ("-0", 0),
            ("+0", 0),
            ("5.0s", 5 * S),
            ("5.6s", 5 * S + 600 * MS),
            ("5.s", 5 * S),
            (".5s", 500 * MS),
            ("1.0s", S),
            ("1.00s", S),
            ("1.004s", S + 4 * MS),
            ("1.0040s", S + 4 * MS),
            ("100.00100s", 100 * S + MS),
            ("10ns", 10 * NS),
            ("11us", 11 * US),
            ("12µs", 12 * US),
            ("12μs", 12 * US),
            ("13ms", 13 * MS),
            ("14s", 14 * S),
            ("15m", 15 * M),
            ("16h", 16 * H),
            ("3h30m", 3 * H + 30 * M),
            ("10.5s4m", 4 * M + 10 * S + 500 * MS),
            ("-2m3.4s", -(2 * M + 3 * S + 400 * MS)),
            (
                "1h2m3s4ms5us6ns",
                H + 2 * M + 3 * S + 4 * MS + 5 * US + 6 * NS,
            ),
            ("39h9m14.425s", 39 * H + 9 * M + 14 * S + 425 * MS),
            ("52763797000ns", 52763797000 * NS),
            ("0.3333333333333333333h", 20 * M),
            ("9007199254740993ns", (1 << 53) + 1),
            ("9223372036854775807ns", i64::MAX),
            ("9223372036854775.807us", i64::MAX),
            ("9223372036s854ms775us807ns", i64::MAX),
            ("-9223372036854775808ns", i64::MIN),
            ("-9223372036854775.808us", i64::MIN),
            ("-9223372036s854ms775us808ns", i64::MIN),
            ("-2562047h47m16.854775808s", i64::MIN),
            ("0.100000000000000000000h", 6 * M),
            ("0.830103483285477580700h", 49 * M + 48 * S + 372539827 * NS),
        ];
        for (s, want) in ok {
            assert_eq!(parse_go_duration(s), Ok(*want), "{s}");
        }

        let bad: &[(&str, &str)] = &[
            ("", r#"time: invalid duration """#),
            ("3", r#"time: missing unit in duration "3""#),
            ("-", r#"time: invalid duration "-""#),
            ("s", r#"time: invalid duration "s""#),
            (".", r#"time: invalid duration ".""#),
            ("-.", r#"time: invalid duration "-.""#),
            (".s", r#"time: invalid duration ".s""#),
            ("+.s", r#"time: invalid duration "+.s""#),
            ("1d", r#"time: unknown unit "d" in duration "1d""#),
            ("1s ", r#"time: unknown unit "s " in duration "1s ""#),
            (
                "9223372036854775808ns",
                r#"time: invalid duration "9223372036854775808ns""#,
            ),
            (
                "9223372036854775.808us",
                r#"time: invalid duration "9223372036854775.808us""#,
            ),
            (
                "2562047h47m16.854775808s",
                r#"time: invalid duration "2562047h47m16.854775808s""#,
            ),
            (
                "-9223372036854775809ns",
                r#"time: invalid duration "-9223372036854775809ns""#,
            ),
        ];
        for (s, want) in bad {
            assert_eq!(parse_go_duration(s), Err(want.to_string()), "{s:?}");
        }
    }
}
