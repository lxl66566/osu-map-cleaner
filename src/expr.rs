//! osu-style filter expression: whitespace-separated conditions ANDed together.
//!
//! Each condition is `field op value`, e.g. `key=7 star<3 mode=mania`.

use std::fmt;

use thiserror::Error;

/// osu gamemode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Standard,
    Taiko,
    Catch,
    Mania,
}

impl Mode {
    fn from_ident(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "std" | "osu" | "standard" | "0" => Some(Self::Standard),
            "taiko" | "1" => Some(Self::Taiko),
            "catch" | "ctb" | "fruits" | "2" => Some(Self::Catch),
            "mania" | "keys" | "3" => Some(Self::Mania),
            _ => None,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Standard => "std",
            Self::Taiko => "taiko",
            Self::Catch => "catch",
            Self::Mania => "mania",
        })
    }
}

/// Ranked status. Pending/WIP/Graveyard share one raw value in osu!.db
/// and therefore cannot be distinguished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unknown,
    Unsubmitted,
    Pending,
    Ranked,
    Approved,
    Qualified,
    Loved,
}

impl Status {
    fn from_ident(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "unknown" => Some(Self::Unknown),
            "unsubmitted" => Some(Self::Unsubmitted),
            "pending" | "wip" | "graveyard" => Some(Self::Pending),
            "ranked" => Some(Self::Ranked),
            "approved" => Some(Self::Approved),
            "qualified" => Some(Self::Qualified),
            "loved" => Some(Self::Loved),
            _ => None,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "unknown",
            Self::Unsubmitted => "unsubmitted",
            Self::Pending => "pending",
            Self::Ranked => "ranked",
            Self::Approved => "approved",
            Self::Qualified => "qualified",
            Self::Loved => "loved",
        })
    }
}

/// Numeric beatmap fields usable in expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumField {
    /// Mania key count (= circle size, mania only; other modes never match).
    Key,
    Cs,
    Star,
    Ar,
    Od,
    Hp,
    /// Total length in seconds.
    Length,
    /// Drain time in seconds.
    Drain,
}

impl NumField {
    fn from_ident(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "key" => Some(Self::Key),
            "cs" => Some(Self::Cs),
            "star" | "sr" => Some(Self::Star),
            "ar" => Some(Self::Ar),
            "od" => Some(Self::Od),
            "hp" => Some(Self::Hp),
            "length" | "len" => Some(Self::Length),
            "drain" => Some(Self::Drain),
            _ => None,
        }
    }
}

impl fmt::Display for NumField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Key => "key",
            Self::Cs => "cs",
            Self::Star => "star",
            Self::Ar => "ar",
            Self::Od => "od",
            Self::Hp => "hp",
            Self::Length => "length",
            Self::Drain => "drain",
        })
    }
}

/// Comparison operators for numeric values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        })
    }
}

/// Equality-only operators for enum (mode/status) values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumOp {
    Eq,
    Ne,
}

impl fmt::Display for EnumOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Eq => "=",
            Self::Ne => "!=",
        })
    }
}

/// One parsed condition. Separate variants make invalid value/op
/// combinations unrepresentable at compile time.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    Num { field: NumField, op: Op, value: f64 },
    Mode { op: EnumOp, value: Mode },
    Status { op: EnumOp, value: Status },
}

/// Beatmap attributes an expression is evaluated against.
/// Decoupled from osu-db so this module stays dependency-free and testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapInfo {
    pub mode: Mode,
    pub status: Status,
    /// Nomod star rating; `None` when the db has no usable value, in which
    /// case star conditions evaluate to `false` (never delete on unknowns).
    pub star: Option<f64>,
    pub cs: f64,
    pub ar: f64,
    pub od: f64,
    pub hp: f64,
    /// Total length in seconds.
    pub length: f64,
    /// Drain time in seconds.
    pub drain: f64,
}

impl Cond {
    #[must_use]
    pub fn matches(&self, m: &MapInfo) -> bool {
        match self {
            Self::Num { field, op, value } => {
                let Some(a) = numeric(m, *field) else {
                    return false;
                };
                !a.is_nan() && op.eval(a, *value)
            },
            Self::Mode { op, value } => match op {
                EnumOp::Eq => m.mode == *value,
                EnumOp::Ne => m.mode != *value,
            },
            Self::Status { op, value } => match op {
                EnumOp::Eq => m.status == *value,
                EnumOp::Ne => m.status != *value,
            },
        }
    }
}

impl Op {
    // Exact comparison is intended: users filter on values like `key=7`
    // that are exact in the db, and epsilon semantics would be surprising.
    #[allow(clippy::float_cmp)]
    fn eval(self, a: f64, b: f64) -> bool {
        match self {
            Self::Eq => a == b,
            Self::Ne => a != b,
            Self::Lt => a < b,
            Self::Le => a <= b,
            Self::Gt => a > b,
            Self::Ge => a >= b,
        }
    }
}

/// Numeric view of a field; `None` means "cannot decide" (treated as no-match).
fn numeric(m: &MapInfo, f: NumField) -> Option<f64> {
    match f {
        NumField::Key => (m.mode == Mode::Mania).then_some(m.cs),
        NumField::Cs => Some(m.cs),
        NumField::Star => m.star,
        NumField::Ar => Some(m.ar),
        NumField::Od => Some(m.od),
        NumField::Hp => Some(m.hp),
        NumField::Length => Some(m.length),
        NumField::Drain => Some(m.drain),
    }
}

/// A parsed expression: all conditions must hold (AND).
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    conds: Vec<Cond>,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("表达式为空")]
    Empty,
    #[error("条件 `{0}` 格式错误，应为 字段+运算符+值，如 key=7")]
    Syntax(String),
    #[error("未知字段 `{0}`，支持: key cs star ar od hp length drain mode status")]
    UnknownField(String),
    #[error("字段 {field} 不支持运算符 {op}（仅支持 = / !=）")]
    OpNotAllowed { field: String, op: String },
    #[error("无效数值 `{0}`")]
    BadNumber(String),
    #[error("未知模式 `{0}`（std/taiko/catch/mania 或 0-3）")]
    UnknownMode(String),
    #[error("未知状态 `{0}`（unknown/unsubmitted/pending/ranked/approved/qualified/loved）")]
    UnknownStatus(String),
}

// Two-char operators must be tried before one-char ones when stripping.
const OPS: &[(&str, Op)] = &[
    ("!=", Op::Ne),
    ("<=", Op::Le),
    (">=", Op::Ge),
    ("==", Op::Eq),
    ("=", Op::Eq),
    ("<", Op::Lt),
    (">", Op::Gt),
];

impl Expr {
    /// Parse a whole expression, e.g. `"key=7 star<3"`.
    ///
    /// # Errors
    /// Returns [`ParseError`] when any condition is malformed.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut conds = Vec::new();
        for tok in input.split_whitespace() {
            conds.push(parse_cond(tok)?);
        }
        if conds.is_empty() {
            return Err(ParseError::Empty);
        }
        Ok(Self { conds })
    }

    #[must_use]
    pub fn matches(&self, m: &MapInfo) -> bool {
        self.conds.iter().all(|c| c.matches(m))
    }

    /// Whether any condition reads the given numeric field; used to decide
    /// whether backfilling that field is worth the effort.
    #[must_use]
    pub fn uses_num_field(&self, field: NumField) -> bool {
        self.conds
            .iter()
            .any(|c| matches!(c, Cond::Num { field: f, .. } if *f == field))
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for cond in &self.conds {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(f, "{cond}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Cond {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Num { field, op, value } => write!(f, "{field}{op}{value}"),
            Self::Mode { op, value } => write!(f, "mode{op}{value}"),
            Self::Status { op, value } => write!(f, "status{op}{value}"),
        }
    }
}

fn parse_cond(tok: &str) -> Result<Cond, ParseError> {
    let syntax_err = || ParseError::Syntax(tok.to_owned());
    let Some(pos) = tok.find(['=', '!', '<', '>']) else {
        return Err(syntax_err());
    };
    let (field_s, rest) = tok.split_at(pos);
    if field_s.is_empty() {
        return Err(syntax_err());
    }
    let (op, val_s) = OPS
        .iter()
        .find_map(|(s, op)| rest.strip_prefix(s).map(|r| (*op, r)))
        .ok_or_else(syntax_err)?;
    if val_s.is_empty() {
        return Err(syntax_err());
    }

    if let Some(field) = NumField::from_ident(field_s) {
        let value: f64 = val_s
            .parse()
            .map_err(|_| ParseError::BadNumber(val_s.to_owned()))?;
        if !value.is_finite() {
            return Err(ParseError::BadNumber(val_s.to_owned()));
        }
        return Ok(Cond::Num { field, op, value });
    }
    match field_s.to_ascii_lowercase().as_str() {
        "mode" => Ok(Cond::Mode {
            op: enum_op(op).ok_or_else(|| ParseError::OpNotAllowed {
                field: field_s.to_owned(),
                op: op.to_string(),
            })?,
            value: Mode::from_ident(val_s)
                .ok_or_else(|| ParseError::UnknownMode(val_s.to_owned()))?,
        }),
        "status" => Ok(Cond::Status {
            op: enum_op(op).ok_or_else(|| ParseError::OpNotAllowed {
                field: field_s.to_owned(),
                op: op.to_string(),
            })?,
            value: Status::from_ident(val_s)
                .ok_or_else(|| ParseError::UnknownStatus(val_s.to_owned()))?,
        }),
        _ => Err(ParseError::UnknownField(field_s.to_owned())),
    }
}

fn enum_op(op: Op) -> Option<EnumOp> {
    match op {
        Op::Eq => Some(EnumOp::Eq),
        Op::Ne => Some(EnumOp::Ne),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mania() -> MapInfo {
        MapInfo {
            mode: Mode::Mania,
            status: Status::Ranked,
            star: Some(2.5),
            cs: 7.0,
            ar: 9.5,
            od: 8.0,
            hp: 8.0,
            length: 90.0,
            drain: 80.0,
        }
    }

    #[test]
    fn parse_roundtrip() {
        let e = Expr::parse("key=7 star<3 mode=mania status!=ranked").unwrap();
        assert_eq!(e.to_string(), "key=7 star<3 mode=mania status!=ranked");
        let mut m = mania();
        assert!(!e.matches(&m)); // status!=ranked fails on a ranked map
        m.status = Status::Pending;
        assert!(e.matches(&m));
    }

    #[test]
    fn parse_accepts_doubled_eq_and_case_insensitive() {
        let e = Expr::parse("Key==7 STAR<=3.5").unwrap();
        assert_eq!(e.to_string(), "key=7 star<=3.5");
    }

    #[test]
    fn parse_errors() {
        for bad in [
            "", "  ", "key", "=7", "key=", "key!7", "foo=1", "key=abc", "key=nan",
        ] {
            assert!(Expr::parse(bad).is_err(), "should fail: {bad:?}");
        }
        assert!(matches!(
            Expr::parse("mode<mania").unwrap_err(),
            ParseError::OpNotAllowed { .. }
        ));
        assert!(matches!(
            Expr::parse("mode=golf").unwrap_err(),
            ParseError::UnknownMode(_)
        ));
        assert!(matches!(
            Expr::parse("key=~7").unwrap_err(),
            ParseError::BadNumber(_)
        ));
    }

    #[test]
    fn key_is_mania_only() {
        let expr = Expr::parse("key=7").unwrap();
        let mut m = mania();
        assert!(expr.matches(&m));
        m.mode = Mode::Standard; // a std map with CS 7 is not a 7K map
        assert!(!expr.matches(&m));
        m.mode = Mode::Mania;
        m.cs = 4.0;
        assert!(!expr.matches(&m));
    }

    #[test]
    fn unknown_star_never_matches() {
        let mut m = mania();
        m.star = None;
        for e in ["star<3", "star>=3", "star!=3"] {
            assert!(
                !Expr::parse(e).unwrap().matches(&m),
                "should not match: {e}"
            );
        }
    }

    #[test]
    fn numeric_fields() {
        let m = mania();
        for (e, want) in [
            ("cs=7", true),
            ("cs>7", false),
            ("ar>=9.5", true),
            ("od<9", true),
            ("hp>8", false),
            ("length>60", true),
            ("length<=90", true),
            ("drain<80", false),
            ("drain<=80", true),
            ("star<3", true),
            ("mode=keys", true),
            ("mode!=std", true),
            ("status=ranked", true),
        ] {
            assert_eq!(Expr::parse(e).unwrap().matches(&m), want, "expr: {e}");
        }
    }
}
