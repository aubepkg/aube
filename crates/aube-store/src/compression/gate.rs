//! The install-time gate: decide whether a given file should be OS-compressed.
//!
//! A `Gate` is a glob AND/OR a size predicate. The PM helper calls
//! `gate.matches(name, len)` after it knows the addon's name + decoded length and
//! only hands the bytes to `compress_bytes` when both predicates pass. Both halves
//! are optional; a `Gate::default()` matches the fleet default `**/*.node` with no
//! size floor.
//!
//! The size predicate parses a human string (`">"`/`">="` + a number with an
//! optional unit) so a manifest can carry `compress = ">= 1MB"` verbatim. Units are
//! case-insensitive and cover both the decimal (`KB`/`MB`/`GB` = 1000ⁿ) and binary
//! (`KiB`/`MiB`/`GiB` = 1024ⁿ) families; a bare number is bytes.

/// A `>`/`>=` comparison against a byte threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizePredicate {
    /// `len > threshold`.
    GreaterThan(u64),
    /// `len >= threshold`.
    AtLeast(u64),
}

impl SizePredicate {
    /// True when `len` satisfies the comparison.
    pub fn matches(&self, len: u64) -> bool {
        match *self {
            SizePredicate::GreaterThan(t) => len > t,
            SizePredicate::AtLeast(t) => len >= t,
        }
    }

    /// Parse `"> 1MB"` / `">=1024"` / `"> 4 KiB"` into a predicate. Whitespace is
    /// optional everywhere; the operator must be `>` or `>=`; the unit (if any) is
    /// case-insensitive.
    pub fn parse(spec: &str) -> Result<SizePredicate, GateParseError> {
        let trimmed = spec.trim();
        let (at_least, rest) = if let Some(rest) = trimmed.strip_prefix(">=") {
            (true, rest)
        } else if let Some(rest) = trimmed.strip_prefix('>') {
            (false, rest)
        } else {
            return Err(GateParseError::Operator);
        };
        let bytes = parse_size(rest.trim())?;
        Ok(if at_least {
            SizePredicate::AtLeast(bytes)
        } else {
            SizePredicate::GreaterThan(bytes)
        })
    }
}

/// Parse a size literal (`"1MB"`, `"4 KiB"`, `"512"`) into a byte count. A bare
/// number is bytes; a unit suffix scales it. Decimal units are powers of 1000,
/// binary units (the `i` forms) powers of 1024.
fn parse_size(spec: &str) -> Result<u64, GateParseError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(GateParseError::Number);
    }
    // Split the leading digit run from the trailing unit.
    let split = spec
        .find(|c: char| !c.is_ascii_digit() && c != '_')
        .unwrap_or(spec.len());
    let (digits, unit) = spec.split_at(split);
    let number: u64 = digits
        .replace('_', "")
        .parse()
        .map_err(|_| GateParseError::Number)?;
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        _ => return Err(GateParseError::Unit),
    };
    number
        .checked_mul(multiplier)
        .ok_or(GateParseError::Overflow)
}

/// Why a `Gate` / `SizePredicate` string failed to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateParseError {
    /// The size predicate did not start with `>` or `>=`.
    Operator,
    /// The numeric portion was missing or not an integer.
    Number,
    /// The unit suffix was not one of B/KB/MB/GB/KiB/MiB/GiB.
    Unit,
    /// The number × unit overflowed u64.
    Overflow,
}

impl std::fmt::Display for GateParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            GateParseError::Operator => "size predicate must start with '>' or '>='",
            GateParseError::Number => "size predicate needs an integer (e.g. '> 1MB')",
            GateParseError::Unit => "unknown size unit (use B/KB/MB/GB/KiB/MiB/GiB)",
            GateParseError::Overflow => "size predicate overflows a 64-bit byte count",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for GateParseError {}

/// The install-time gate: an optional glob AND an optional size predicate. A file
/// matches only if BOTH present predicates pass (an absent half is vacuously true).
/// `Gate::default()` is the fleet default — glob `**/*.node`, no size floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    glob: Option<String>,
    size: Option<SizePredicate>,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            glob: Some(DEFAULT_GLOB.to_string()),
            size: None,
        }
    }
}

/// The fleet default — every native addon, regardless of size.
pub const DEFAULT_GLOB: &str = "**/*.node";

impl Gate {
    /// A gate that matches everything (no glob, no size floor). Useful when the
    /// caller has already selected the file and just wants the one-pass writer.
    pub fn any() -> Self {
        Gate {
            glob: None,
            size: None,
        }
    }

    /// Build a gate from an optional glob and an optional size-predicate string. A
    /// `None` glob matches any name; a `None`/empty size string applies no floor.
    pub fn new(glob: Option<&str>, size: Option<&str>) -> Result<Gate, GateParseError> {
        let size = match size {
            None => None,
            Some(s) if s.trim().is_empty() => None,
            Some(s) => Some(SizePredicate::parse(s)?),
        };
        Ok(Gate {
            glob: glob.map(str::to_string),
            size,
        })
    }

    /// Replace the glob (chainable builder).
    pub fn with_glob(mut self, glob: &str) -> Self {
        self.glob = Some(glob.to_string());
        self
    }

    /// Replace the size predicate (chainable builder).
    pub fn with_size(mut self, size: SizePredicate) -> Self {
        self.size = Some(size);
        self
    }

    /// The glob pattern, if any.
    pub fn glob(&self) -> Option<&str> {
        self.glob.as_deref()
    }

    /// The size predicate, if any.
    pub fn size(&self) -> Option<SizePredicate> {
        self.size
    }

    /// True when `name` matches the glob (if set) AND `len` satisfies the size
    /// predicate (if set). The name is matched against the full path the caller
    /// passes — pass a `/`-normalized path so `**` segments line up.
    pub fn matches(&self, name: &str, len: u64) -> bool {
        if let Some(glob) = &self.glob
            && !glob_match(glob, name)
        {
            return false;
        }
        if let Some(size) = &self.size
            && !size.matches(len)
        {
            return false;
        }
        true
    }
}

/// A small glob matcher covering the subset the gate needs: `*` (any run within a
/// path segment, no `/`), `**` (any run across segments, including `/`), and `?`
/// (one non-`/` char). Literal bytes match themselves. No char classes — the gate
/// patterns are simple suffix/segment globs like `**/*.node`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    glob_inner(pattern.as_bytes(), text.as_bytes())
}

/// Recursive matcher — clean handling of nested stars (a `*` inside a `**` tail,
/// like `**/*.node`) that a single backtrack slot can't express. The pattern set
/// is small (suffix/segment globs), so the recursion depth is the number of stars,
/// not the input length.
fn glob_inner(pat: &[u8], text: &[u8]) -> bool {
    // Pattern exhausted: match iff text is too.
    let Some(&pc) = pat.first() else {
        return text.is_empty();
    };
    match pc {
        b'*' => {
            let double = pat.get(1) == Some(&b'*');
            if double {
                // `**` matches any run including `/`. If immediately followed by `/`, that
                // slash may also collapse to nothing (a/**/b ⊇ a/b).
                let after = &pat[2..];
                let collapsed = after.strip_prefix(b"/").unwrap_or(after);
                // Zero-width match (with the optional `/` collapse), then every longer run.
                if glob_inner(collapsed, text) || glob_inner(after, text) {
                    return true;
                }
                for i in 0..text.len() {
                    if glob_inner(after, &text[i + 1..]) || glob_inner(collapsed, &text[i + 1..]) {
                        return true;
                    }
                }
                false
            } else {
                // `*` matches a run within one path segment — never a `/`.
                let rest = &pat[1..];
                if glob_inner(rest, text) {
                    return true;
                }
                for i in 0..text.len() {
                    if text[i] == b'/' {
                        break;
                    }
                    if glob_inner(rest, &text[i + 1..]) {
                        return true;
                    }
                }
                false
            }
        }
        b'?' => match text.first() {
            Some(&c) if c != b'/' => glob_inner(&pat[1..], &text[1..]),
            _ => false,
        },
        c => match text.first() {
            Some(&t) if t == c => glob_inner(&pat[1..], &text[1..]),
            _ => false,
        },
    }
}
