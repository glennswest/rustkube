//! Kubernetes resource quantities — `100m`, `1Gi`, `1.5G`, `1e9`.
//!
//! One implementation, because there were three: `scheduler::filter`,
//! `scheduler::score` and `scheduler::preemption` each carried their own
//! `parse_memory_bytes`, and they had already drifted in what they accepted.
//! A parser that silently returns 0 for a quantity it does not recognise
//! decides scheduling and volume binding — a PVC asking for `1.5Gi` matching
//! a 0-byte volume is a bind that should not have happened — so the fallback
//! matters as much as the happy path.

/// Parse a quantity to bytes.
///
/// Accepts the binary suffixes (`Ki Mi Gi Ti Pi Ei`), the decimal ones
/// (`n u m k K M G T P E`), plain numbers, fractions (`1.5Gi`) and the
/// exponent form (`1e9`). An unparseable quantity is 0 — the caller reads
/// that as "asks for nothing", which fails closed for capacity comparisons.
pub fn parse_bytes(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    // Binary (power-of-two) suffixes first: "Mi" ends in 'i', so it must be
    // tested before the decimal "M".
    const BINARY: &[(&str, f64)] = &[
        ("Ki", 1024.0),
        ("Mi", 1_048_576.0),
        ("Gi", 1_073_741_824.0),
        ("Ti", 1_099_511_627_776.0),
        ("Pi", 1_125_899_906_842_624.0),
        ("Ei", 1_152_921_504_606_846_976.0),
    ];
    for (suffix, mult) in BINARY {
        if let Some(num) = s.strip_suffix(suffix) {
            return scale(num, *mult);
        }
    }
    const DECIMAL: &[(&str, f64)] = &[
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("K", 1e3), // not canonical, but written by hand often enough
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ];
    for (suffix, mult) in DECIMAL {
        // `1e9` ends in no suffix but contains 'E'/'e'; only strip a suffix
        // when what is left still parses as a number.
        if let Some(num) = s.strip_suffix(suffix) {
            if num.parse::<f64>().is_ok() {
                return scale(num, *mult);
            }
        }
    }
    s.parse::<f64>().map(|v| v.max(0.0) as u64).unwrap_or(0)
}

fn scale(num: &str, mult: f64) -> u64 {
    match num.parse::<f64>() {
        Ok(v) if v > 0.0 => (v * mult) as u64,
        _ => 0,
    }
}

/// Parse a CPU quantity to millicores.
pub fn parse_cpu_millis(s: &str) -> u64 {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix('m') {
        stripped.parse::<f64>().map(|v| v.max(0.0) as u64).unwrap_or(0)
    } else {
        s.parse::<f64>().map(|v| (v.max(0.0) * 1000.0) as u64).unwrap_or(0)
    }
}

/// Render bytes back as a quantity, preferring the largest binary suffix that
/// divides exactly — `1073741824` becomes `1Gi`, not `1024Mi` or a raw count.
/// Status fields are read by people.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("Ei", 1 << 60),
        ("Pi", 1 << 50),
        ("Ti", 1 << 40),
        ("Gi", 1 << 30),
        ("Mi", 1 << 20),
        ("Ki", 1 << 10),
    ];
    for (suffix, size) in UNITS {
        if bytes >= *size && bytes % size == 0 {
            return format!("{}{suffix}", bytes / size);
        }
    }
    bytes.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_suffixes() {
        assert_eq!(parse_bytes("1Ki"), 1024);
        assert_eq!(parse_bytes("1Mi"), 1024 * 1024);
        assert_eq!(parse_bytes("1Gi"), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes("2Ti"), 2 * 1024u64.pow(4));
    }

    #[test]
    fn decimal_suffixes_are_not_binary_ones() {
        assert_eq!(parse_bytes("1G"), 1_000_000_000);
        assert_eq!(parse_bytes("1k"), 1000);
        assert_ne!(parse_bytes("1G"), parse_bytes("1Gi"));
    }

    #[test]
    fn fractions_and_exponents() {
        assert_eq!(parse_bytes("1.5Gi"), 1_610_612_736);
        assert_eq!(parse_bytes("1e9"), 1_000_000_000);
        assert_eq!(parse_bytes("500"), 500);
    }

    #[test]
    fn nonsense_is_zero_not_a_panic() {
        assert_eq!(parse_bytes(""), 0);
        assert_eq!(parse_bytes("plenty"), 0);
        assert_eq!(parse_bytes("-5Gi"), 0);
    }

    #[test]
    fn cpu() {
        assert_eq!(parse_cpu_millis("100m"), 100);
        assert_eq!(parse_cpu_millis("1"), 1000);
        assert_eq!(parse_cpu_millis("0.5"), 500);
    }

    #[test]
    fn round_trips_to_the_largest_exact_suffix() {
        assert_eq!(format_bytes(1 << 30), "1Gi");
        assert_eq!(format_bytes(5 * (1 << 20)), "5Mi");
        assert_eq!(format_bytes(1000), "1000");
        assert_eq!(parse_bytes(&format_bytes(3 * (1 << 30))), 3 * (1 << 30));
    }
}
