//! DHIS2 indicator expressions, for `--source data-values`: the numerator
//! and denominator kiln computes itself from raw data values.
//!
//! Covered: numbers, `#{de}`, `#{de.coc}`, `#{de.coc.aoc}` and `#{de.*.aoc}`
//! (a data element's total, one category option combo, one attribute option
//! combo), `C{constant}`, `+ - * /`, unary minus and parentheses. Anything
//! else — `R{…}` reporting rates, `I{…}` program indicators, `D{…}`/`A{…}`
//! tracker items, `OUG{…}`, `[days]`, functions such as `if()` or
//! `isNull()` — is refused with its name, so an indicator is computed
//! exactly or not at all.
//!
//! Missing data: an item with no value counts as 0, unless every item in
//! the expression is missing, in which case the expression has no value
//! (DHIS2's rule for indicator expressions). Division by zero has no value.

use std::collections::HashMap;

/// A data element reference: `#{de}`, `#{de.coc}`, `#{de.coc.aoc}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Item {
    pub de: String,
    pub coc: Option<String>,
    pub aoc: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Item(Item),
    Const(String),
    Neg(Box<Expr>),
    Bin(char, Box<Expr>, Box<Expr>),
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
}

impl Parser<'_> {
    fn skip(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip();
        self.s.get(self.i).copied()
    }

    fn expr(&mut self) -> Result<Expr, String> {
        let mut left = self.term()?;
        while let Some(op @ (b'+' | b'-')) = self.peek() {
            self.i += 1;
            left = Expr::Bin(op as char, Box::new(left), Box::new(self.term()?));
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while let Some(op @ (b'*' | b'/')) = self.peek() {
            self.i += 1;
            left = Expr::Bin(op as char, Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(b'-') => {
                self.i += 1;
                Ok(Expr::Neg(Box::new(self.unary()?)))
            }
            Some(b'+') => {
                self.i += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }

    /// The text inside `{…}` starting at `self.i` (just after the `{`).
    fn braced(&mut self) -> Result<String, String> {
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i] != b'}' {
            self.i += 1;
        }
        if self.i >= self.s.len() {
            return Err(format!("unclosed {{ in {:?}", self.text));
        }
        let inner = self.text[start..self.i].trim().to_string();
        self.i += 1;
        Ok(inner)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let Some(c) = self.peek() else {
            return Err(format!("{:?} ends too soon", self.text));
        };
        let rest = &self.text[self.i..];
        if c == b'(' {
            self.i += 1;
            let e = self.expr()?;
            if self.peek() != Some(b')') {
                return Err(format!("missing ) in {:?}", self.text));
            }
            self.i += 1;
            return Ok(e);
        }
        if c.is_ascii_digit() || c == b'.' {
            let start = self.i;
            while self.i < self.s.len()
                && (self.s[self.i].is_ascii_digit() || self.s[self.i] == b'.')
            {
                self.i += 1;
            }
            return self.text[start..self.i]
                .parse()
                .map(Expr::Num)
                .map_err(|_| format!("bad number in {:?}", self.text));
        }
        if rest.starts_with("#{") {
            self.i += 2;
            let inner = self.braced()?;
            let mut parts = inner.split('.');
            let de = parts.next().unwrap_or_default().to_string();
            let part = |p: Option<&str>| p.filter(|p| *p != "*").map(str::to_string);
            let (coc, aoc) = (part(parts.next()), part(parts.next()));
            if de.is_empty() || parts.next().is_some() {
                return Err(format!("#{{{inner}}} is not a data element reference"));
            }
            return Ok(Expr::Item(Item { de, coc, aoc }));
        }
        if rest.starts_with("C{") {
            self.i += 2;
            return Ok(Expr::Const(self.braced()?));
        }
        for (prefix, what) in [
            ("R{", "a reporting rate"),
            ("I{", "a program indicator"),
            ("D{", "a program data element"),
            ("A{", "a program attribute"),
            ("OUG{", "an org unit group count"),
            ("N{", "an indicator reference"),
            ("[days]", "[days]"),
        ] {
            if rest.starts_with(prefix) {
                return Err(format!("{what} ({prefix}…)"));
            }
        }
        if c.is_ascii_alphabetic() {
            let word: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            return Err(format!("the function or name {word}"));
        }
        Err(format!("{:?} at {:?}", c as char, rest))
    }
}

impl Expr {
    /// The expression, or what in it cannot be computed from data values.
    pub fn parse(text: &str) -> Result<Expr, String> {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
            text,
        };
        let e = p.expr()?;
        match p.peek() {
            None => Ok(e),
            Some(c) => Err(format!("unexpected {:?} in {text:?}", c as char)),
        }
    }

    pub fn items(&self, out: &mut Vec<Item>) {
        match self {
            Expr::Item(i) => out.push(i.clone()),
            Expr::Neg(e) => e.items(out),
            Expr::Bin(_, a, b) => {
                a.items(out);
                b.items(out);
            }
            Expr::Num(_) | Expr::Const(_) => {}
        }
    }

    pub fn constants(&self, out: &mut Vec<String>) {
        match self {
            Expr::Const(c) => out.push(c.clone()),
            Expr::Neg(e) => e.constants(out),
            Expr::Bin(_, a, b) => {
                a.constants(out);
                b.constants(out);
            }
            Expr::Num(_) | Expr::Item(_) => {}
        }
    }

    /// The value, with `value` giving each item's (`None` when missing).
    pub fn eval(
        &self,
        value: &dyn Fn(&Item) -> Option<f64>,
        constants: &HashMap<String, f64>,
    ) -> Option<f64> {
        let mut items = Vec::new();
        self.items(&mut items);
        if !items.is_empty() && items.iter().all(|i| value(i).is_none()) {
            return None;
        }
        self.calc(&|i| value(i).unwrap_or(0.0), constants)
    }

    fn calc(&self, value: &dyn Fn(&Item) -> f64, constants: &HashMap<String, f64>) -> Option<f64> {
        let r = match self {
            Expr::Num(n) => *n,
            Expr::Item(i) => value(i),
            Expr::Const(c) => *constants.get(c)?,
            Expr::Neg(e) => -e.calc(value, constants)?,
            Expr::Bin(op, a, b) => {
                let (a, b) = (a.calc(value, constants)?, b.calc(value, constants)?);
                match op {
                    '+' => a + b,
                    '-' => a - b,
                    '*' => a * b,
                    _ if b == 0.0 => return None,
                    _ => a / b,
                }
            }
        };
        r.is_finite().then_some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(item: &Item) -> Option<f64> {
        if item.aoc.is_some() {
            return None;
        }
        match (item.de.as_str(), item.coc.as_deref()) {
            ("a", None) => Some(9.0),
            ("a", Some("f")) => Some(7.0),
            ("b", None) => Some(20.0),
            _ => None,
        }
    }

    #[test]
    fn items_constants_and_arithmetic() {
        let k = HashMap::from([("k".to_string(), 2.0)]);
        let e = |s: &str| Expr::parse(s).unwrap().eval(&values, &k);
        assert_eq!(e("#{a}"), Some(9.0));
        assert_eq!(e("#{a.f}"), Some(7.0));
        assert_eq!(e(" #{a} + #{b} * C{k} "), Some(49.0));
        assert_eq!(e("(#{a} + #{a.f}) / -2"), Some(-8.0));
        assert_eq!(e("#{a.*.x}"), None);
        assert_eq!(
            Expr::parse("#{a.f.x}").unwrap(),
            Expr::Item(Item {
                de: "a".into(),
                coc: Some("f".into()),
                aoc: Some("x".into())
            })
        );
    }

    #[test]
    fn missing_items_count_as_zero_unless_all_are_missing() {
        let k = HashMap::new();
        let e = |s: &str| Expr::parse(s).unwrap().eval(&values, &k);
        assert_eq!(e("#{a} + #{c}"), Some(9.0));
        assert_eq!(e("#{c} + #{d}"), None);
        assert_eq!(e("#{a} / #{c}"), None);
        assert_eq!(e("1"), Some(1.0));
    }

    #[test]
    fn what_cannot_be_computed_is_named() {
        for (text, want) in [
            ("R{ds.REPORTING_RATE}", "reporting rate"),
            ("I{pi}", "program indicator"),
            ("#{a} * [days]", "[days]"),
            ("if(#{a} > 0, 1, 0)", "function or name if"),
            ("OUG{g}", "org unit group"),
            ("#{a} ^ 2", "unexpected '^'"),
            ("(#{a}", "missing )"),
        ] {
            let e = Expr::parse(text).unwrap_err();
            assert!(e.contains(want), "{text}: {e}");
        }
    }
}
