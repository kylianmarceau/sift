//! A small arithmetic evaluator: numbers, `+ - * / % ^` (also `×`, `÷`,
//! `**`), parentheses, unary minus, implicit multiplication (`2pi`,
//! `3(1+2)`), the constants `pi`, `tau` and `e`, and common functions.
//!
//! Nothing is executed beyond arithmetic on `f64`: there are no variables,
//! assignments or user-defined functions. Nesting depth and input length
//! are bounded so hostile input cannot overflow the stack.

/// Longer input is not treated as an expression.
pub const MAX_INPUT: usize = 256;
const MAX_DEPTH: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token<'a> {
    Num(f64),
    Ident(&'a str),
    Op(char),
}

fn tokenize(input: &str) -> Option<Vec<Token<'_>>> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        let c = rest.chars().next()?;
        if c.is_whitespace() {
            i += c.len_utf8();
        } else if c.is_ascii_digit()
            || (c == '.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let mut end = i;
            while end < input.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
                end += 1;
            }
            // Exponent: 1e3, 2.5E-4. A bare `e` is the constant (2e = 2×e).
            if matches!(bytes.get(end), Some(b'e' | b'E')) {
                let mut exp = end + 1;
                if matches!(bytes.get(exp), Some(b'+' | b'-')) {
                    exp += 1;
                }
                if bytes.get(exp).is_some_and(u8::is_ascii_digit) {
                    while bytes.get(exp).is_some_and(u8::is_ascii_digit) {
                        exp += 1;
                    }
                    end = exp;
                }
            }
            tokens.push(Token::Num(input[i..end].parse().ok()?));
            i = end;
        } else if c.is_ascii_alphabetic() {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric())
                .map_or(input.len(), |n| i + n);
            tokens.push(Token::Ident(&input[i..end]));
            i = end;
        } else {
            let (op, len) = match c {
                '*' if rest.starts_with("**") => ('^', 2),
                '+' | '-' | '*' | '/' | '%' | '^' | '(' | ')' => (c, 1),
                '×' | '·' => ('*', c.len_utf8()),
                '÷' => ('/', c.len_utf8()),
                '−' => ('-', c.len_utf8()),
                _ => return None,
            };
            tokens.push(Token::Op(op));
            i += len;
        }
    }
    Some(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    pos: usize,
    depth: usize,
    /// Binary operations and function calls seen. A lone number or
    /// constant is not worth showing as a calculation.
    operations: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<Token<'a>> {
        self.tokens.get(self.pos).copied()
    }

    fn eat(&mut self, op: char) -> bool {
        if self.peek() == Some(Token::Op(op)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth <= MAX_DEPTH).then_some(())
    }

    fn expr(&mut self) -> Option<f64> {
        self.enter()?;
        let mut value = self.term()?;
        loop {
            if self.eat('+') {
                value += self.term()?;
            } else if self.eat('-') {
                value -= self.term()?;
            } else {
                break;
            }
            self.operations += 1;
        }
        self.depth -= 1;
        Some(value)
    }

    fn term(&mut self) -> Option<f64> {
        let mut value = self.unary()?;
        loop {
            if self.eat('*') {
                value *= self.unary()?;
            } else if self.eat('/') {
                value /= self.unary()?;
            } else if self.eat('%') {
                value %= self.unary()?;
            } else if matches!(self.peek(), Some(Token::Op('(') | Token::Ident(_))) {
                // Implicit multiplication: 2pi, 3(4+1), 2 sqrt(9).
                value *= self.power()?;
            } else {
                break;
            }
            self.operations += 1;
        }
        Some(value)
    }

    fn unary(&mut self) -> Option<f64> {
        self.enter()?;
        let value = if self.eat('-') {
            -self.unary()?
        } else if self.eat('+') {
            self.unary()?
        } else {
            self.power()?
        };
        self.depth -= 1;
        Some(value)
    }

    /// Right-associative, binding tighter than unary minus on its left:
    /// `-2^2` is -4 and `2^-1` is 0.5.
    fn power(&mut self) -> Option<f64> {
        let base = self.primary()?;
        if self.eat('^') {
            self.operations += 1;
            return Some(base.powf(self.unary()?));
        }
        Some(base)
    }

    fn primary(&mut self) -> Option<f64> {
        match self.peek()? {
            Token::Num(n) => {
                self.pos += 1;
                Some(n)
            }
            Token::Op('(') => {
                self.pos += 1;
                let value = self.expr()?;
                self.eat(')').then_some(value)
            }
            Token::Ident(name) => {
                self.pos += 1;
                let name = name.to_ascii_lowercase();
                if let Some(value) = constant(&name) {
                    return Some(value);
                }
                let f = function(&name)?;
                if !self.eat('(') {
                    return None;
                }
                let arg = self.expr()?;
                if !self.eat(')') {
                    return None;
                }
                self.operations += 1;
                Some(f(arg))
            }
            Token::Op(_) => None,
        }
    }
}

fn constant(name: &str) -> Option<f64> {
    Some(match name {
        "pi" => std::f64::consts::PI,
        "tau" => std::f64::consts::TAU,
        "e" => std::f64::consts::E,
        _ => return None,
    })
}

fn function(name: &str) -> Option<fn(f64) -> f64> {
    Some(match name {
        "sqrt" => f64::sqrt,
        "cbrt" => f64::cbrt,
        "abs" => f64::abs,
        "round" => f64::round,
        "floor" => f64::floor,
        "ceil" => f64::ceil,
        "exp" => f64::exp,
        "ln" => f64::ln,
        "log" | "log10" => f64::log10,
        "log2" => f64::log2,
        "sin" => f64::sin,
        "cos" => f64::cos,
        "tan" => f64::tan,
        "asin" => f64::asin,
        "acos" => f64::acos,
        "atan" => f64::atan,
        _ => return None,
    })
}

/// Evaluates `input` if it is a calculation: at least one operation, every
/// token understood, and a finite result. Returns `None` for anything else
/// ("2048", "pi", "1password", "e-mail", "1/0"), so ordinary searches never
/// show a calculator result. A leading `=` is allowed.
pub fn evaluate(input: &str) -> Option<f64> {
    let input = input.trim();
    let input = input.strip_prefix('=').unwrap_or(input);
    if input.is_empty() || input.len() > MAX_INPUT {
        return None;
    }
    let mut parser = Parser {
        tokens: tokenize(input)?,
        pos: 0,
        depth: 0,
        operations: 0,
    };
    let value = parser.expr()?;
    let complete = parser.pos == parser.tokens.len();
    (complete && parser.operations > 0 && value.is_finite()).then_some(value)
}

/// Formats a result for display and copying: up to 10 decimal places with
/// trailing zeros removed (so 0.1 + 0.2 is "0.3"), scientific notation for
/// very large or very small magnitudes, and no thousands separators.
pub fn format(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-6..1e15).contains(&magnitude) {
        let text = format!("{value:.9e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        return format!("{}e{exponent}", trim_zeros(mantissa));
    }
    let text = trim_zeros(&format!("{value:.10}")).to_owned();
    if text == "-0" { "0".into() } else { text }
}

fn trim_zeros(text: &str) -> &str {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc(input: &str) -> Option<String> {
        evaluate(input).map(format)
    }

    #[test]
    fn arithmetic_with_precedence() {
        assert_eq!(calc("1 + 2 * 3").as_deref(), Some("7"));
        assert_eq!(calc("(1 + 2) * 3").as_deref(), Some("9"));
        assert_eq!(calc("10 / 4").as_deref(), Some("2.5"));
        assert_eq!(calc("10 % 4").as_deref(), Some("2"));
        assert_eq!(calc("2 ^ 3 ^ 2").as_deref(), Some("512"));
        assert_eq!(calc("2 ** 10").as_deref(), Some("1024"));
        assert_eq!(calc("-2^2").as_deref(), Some("-4"));
        assert_eq!(calc("2^-1").as_deref(), Some("0.5"));
        assert_eq!(calc("3 × 4 ÷ 2 − 1").as_deref(), Some("5"));
        assert_eq!(calc("=1+1").as_deref(), Some("2"));
        assert_eq!(calc("1.5e3 + 1").as_deref(), Some("1501"));
        assert_eq!(calc(".5 * 4").as_deref(), Some("2"));
    }

    #[test]
    fn functions_constants_and_implicit_multiplication() {
        assert_eq!(calc("sqrt(16)").as_deref(), Some("4"));
        assert_eq!(calc("2pi").as_deref(), Some("6.2831853072"));
        assert_eq!(calc("3(1+2)").as_deref(), Some("9"));
        assert_eq!(calc("2 sqrt(9)").as_deref(), Some("6"));
        assert_eq!(calc("log(1000)").as_deref(), Some("3"));
        assert_eq!(calc("ln(e)").as_deref(), Some("1"));
        assert_eq!(
            calc("round(2.5) + floor(1.9) + ceil(0.1)").as_deref(),
            Some("5")
        );
        assert_eq!(calc("SIN(0)").as_deref(), Some("0"));
    }

    #[test]
    fn ordinary_searches_are_not_calculations() {
        for input in [
            "",
            "2048",
            "3.5",
            "-5",
            "pi",
            "e",
            "1password",
            "7zip",
            "e-mail",
            "C++",
            "photo-2024",
            "notes.txt",
            "foo(1)",
            "sqrt 16",
            "(1+2",
            "1+",
            "1 2",
        ] {
            assert_eq!(evaluate(input), None, "{input:?}");
        }
    }

    #[test]
    fn non_finite_results_are_not_shown() {
        assert_eq!(evaluate("1/0"), None);
        assert_eq!(evaluate("sqrt(-1)"), None);
        assert_eq!(evaluate("10^400"), None);
    }

    #[test]
    fn hostile_input_is_bounded() {
        let deep = format!("{}1{}", "(".repeat(10_000), ")".repeat(10_000));
        assert_eq!(evaluate(&deep), None);
        let nested = format!("1+{}1{}", "(".repeat(50), ")".repeat(50));
        assert_eq!(calc(&nested).as_deref(), Some("2"));
        assert_eq!(evaluate(&"-".repeat(200)), None);
    }

    #[test]
    fn formats_cleanly() {
        assert_eq!(calc("0.1 + 0.2").as_deref(), Some("0.3"));
        assert_eq!(calc("1/3").as_deref(), Some("0.3333333333"));
        assert_eq!(calc("0 * -1").as_deref(), Some("0"));
        assert_eq!(calc("2^60").as_deref(), Some("1.152921505e18"));
        assert_eq!(calc("1/2^30").as_deref(), Some("9.313225746e-10"));
        assert_eq!(calc("123456789 * 1000").as_deref(), Some("123456789000"));
    }
}
