//! A per-pixel math expression, the cheapest way to make something wild. Port of `formula.py`.
//!
//! The Python version parses the text with `ast.parse`, checks every node against a whitelist and then
//! `eval`s it, because the expression can come from a model. Here nothing is ever executed as code: the text
//! is tokenized and parsed by a small Python-expression parser, the tree is checked against the same
//! whitelist (in the same breadth-first order, so the same first error is reported), and a tiny interpreter
//! evaluates it with Python's semantics: int and float are different types, `//` and `%` floor, `**` of a
//! negative base to a fractional power is complex, `and`/`or` return an operand, and any arithmetic, value
//! or type error just makes the pixel white.
//!
//! Allowed: the variables `x y u v r a W H`, the constants `pi tau e`, the functions `sin cos tan atan atan2
//! sqrt abs floor ceil exp log hypot min max round noise int pow`, numbers, the operators
//! `+ - * / // % ** | & ^`, unary `+ - not`, comparisons (chained), `and or`, and `a if c else b`.

use crate::patterns;

pub const MAX_LENGTH: usize = 400;

/// What Python raises as `FormulaError`; the message is meant for the agent that wrote the formula.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FormulaError(pub String);

/// A compiled formula. Call `test` for each pixel.
#[derive(Debug, Clone)]
pub struct Formula {
    root: Node,
}

/// Checks the text against the whitelist and returns something that can be evaluated per pixel.
pub fn compile_formula(src: &str) -> Result<Formula, FormulaError> {
    if src.chars().count() > MAX_LENGTH {
        return Err(FormulaError(format!(
            "Formula is over {MAX_LENGTH} characters."
        )));
    }
    let check = || -> Result<Node, FormulaError> {
        let root = parse_source(src)
            .map_err(|msg| FormulaError(format!("Formula has a syntax error: {msg}")))?;
        validate(&root)?;
        Ok(root)
    };
    // The parser is recursive and a formula can nest 200 brackets deep, so give it room instead of
    // trusting the caller's thread stack.
    let root = std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn_scoped(scope, check);
        match spawned {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|_| Err(FormulaError("Formula is nested too deeply.".into()))),
            Err(_) => Err(FormulaError("Formula could not be checked.".into())),
        }
    })?;
    Ok(Formula { root })
}

impl Formula {
    /// Is the pixel at (i, j) of a w x h canvas black? Math errors (division by zero, domain errors, wrong
    /// types) give false, like the Python `except (ArithmeticError, ValueError, TypeError)`.
    pub fn test(&self, i: i32, j: i32, w: i32, h: i32) -> bool {
        let cx = i as f64 - w as f64 / 2.0;
        let cy = j as f64 - h as f64 / 2.0;
        let env = Env {
            x: i as i128,
            y: j as i128,
            w: w as i128,
            h: h as i128,
            u: i as f64 / (w as i64 - 1).max(1) as f64,
            v: j as f64 / (h as i64 - 1).max(1) as f64,
            r: cx.hypot(cy),
            a: cy.atan2(cx),
        };
        match eval(&self.root, &env) {
            Ok(value) => truthy(value),
            Err(Raised) => false,
        }
    }
}

// ================================================================================================
// Tokenizer (CPython's rules and messages)
// ================================================================================================

#[derive(Clone, Debug, PartialEq)]
enum Lit {
    Int(i128),
    Float(f64),
    Imag,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Name(String),
    Number(Lit),
    Str {
        fstring: bool,
    },
    Op(&'static str),
    Newline,
    Indent,
    Dedent,
    /// A tokenizer error. `global` ones are real exceptions that CPython finds even when the parser failed
    /// earlier; the others (`E_LINECONT` and friends) only count if the parser actually reaches them.
    HardError {
        msg: String,
        global: bool,
    },
    /// End of input; `open` is the innermost bracket that was never closed.
    Eof {
        open: Option<(char, usize)>,
    },
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    line: usize,
    /// Bracket depth after this token.
    level: usize,
}

#[rustfmt::skip]
const OPERATORS: &[&str] = &[
    "**=",
    "//=",
    ">>=",
    "<<=",
    "...",
    "!=",
    "**",
    "//",
    "<<",
    ">>",
    "<=",
    ">=",
    "==",
    "->",
    ":=",
    "+=",
    "-=",
    "*=",
    "/=",
    "%=",
    "&=",
    "|=",
    "^=",
    "@=",
    "<>",
    "+",
    "-",
    "*",
    "/",
    "%",
    "@",
    "&",
    "|",
    "^",
    "~",
    "<",
    ">",
    "=",
    ".",
    ",",
    ":",
    ";",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
];

#[rustfmt::skip]
const KEYWORDS: &[&str] = &[
    "False",
    "None",
    "True",
    "and",
    "as",
    "assert",
    "async",
    "await",
    "break",
    "class",
    "continue",
    "def",
    "del",
    "elif",
    "else",
    "except",
    "finally",
    "for",
    "from",
    "global",
    "if",
    "import",
    "in",
    "is",
    "lambda",
    "nonlocal",
    "not",
    "or",
    "pass",
    "raise",
    "return",
    "try",
    "while",
    "with",
    "yield",
];

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic() || (!c.is_ascii() && c.is_alphabetic())
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric() || (!c.is_ascii() && c.is_alphanumeric())
}

/// CPython's `is_potential_identifier_char`: anything non-ASCII counts.
fn is_potential_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii()
}

fn printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    !(c.is_control()
        || c.is_whitespace()
        || matches!(c as u32, 0xad | 0x200b..=0x200f | 0x2028..=0x202e | 0x2060..=0x2064 | 0xfeff))
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    line: usize,
    stack: Vec<(char, usize)>,
    out: Vec<Token>,
}

impl Lexer {
    fn peek(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).copied()
    }

    fn push(&mut self, tok: Tok) {
        let level = self.stack.len();
        self.out.push(Token {
            tok,
            line: self.line,
            level,
        });
    }

    fn hard(&mut self, msg: impl Into<String>) {
        self.push(Tok::HardError {
            msg: msg.into(),
            global: true,
        });
    }

    /// An error that only surfaces when the parser asks for this token.
    fn hard_local(&mut self, msg: impl Into<String>) {
        self.push(Tok::HardError {
            msg: msg.into(),
            global: false,
        });
    }

    /// Consumes a newline (\n, \r\n or \r) if there is one.
    fn eat_newline(&mut self) -> bool {
        match self.peek(0) {
            Some('\n') => {
                self.i += 1;
                true
            }
            Some('\r') => {
                self.i += 1;
                if self.peek(0) == Some('\n') {
                    self.i += 1;
                }
                true
            }
            _ => false,
        }
    }

    fn last_is(&self, f: impl Fn(&Tok) -> bool) -> bool {
        self.out.last().is_some_and(|t| f(&t.tok))
    }
}

fn tokenize(src: &str) -> Vec<Token> {
    let mut lx = Lexer {
        chars: src.chars().collect(),
        i: 0,
        line: 1,
        stack: Vec::new(),
        out: Vec::new(),
    };
    if src.contains('\0') {
        lx.hard("source code string cannot contain null bytes");
        return lx.out;
    }
    let mut indents: Vec<usize> = vec![0];
    let mut at_line_start = true;
    loop {
        if at_line_start && lx.stack.is_empty() {
            // Indentation of a logical line; blank and comment-only lines do not count.
            let mut col = 0usize;
            let mut k = 0;
            loop {
                match lx.peek(k) {
                    Some(' ') => col += 1,
                    Some('\t') => col = (col / 8 + 1) * 8,
                    Some('\u{c}') => col = 0,
                    _ => break,
                }
                k += 1;
            }
            match lx.peek(k) {
                None if k == 0 => {}
                Some('#') | Some('\n') | Some('\r') => {
                    lx.i += k;
                    while lx.peek(0).is_some_and(|c| c != '\n' && c != '\r') {
                        lx.i += 1;
                    }
                    if lx.eat_newline() {
                        lx.line += 1;
                    }
                    continue;
                }
                Some('\\') if !matches!(lx.peek(k + 1), Some('\n' | '\r')) => {
                    // A backslash while the indentation is read is a line continuation error right away.
                    lx.hard_local("unexpected character after line continuation character");
                    return lx.out;
                }
                _ => {
                    // (Whitespace and then the end of the input, with no newline, counts as indentation.)
                    lx.i += k;
                    let top = *indents.last().unwrap_or(&0);
                    if col > top {
                        indents.push(col);
                        lx.push(Tok::Indent);
                    } else if col < top {
                        while col < *indents.last().unwrap_or(&0) {
                            indents.pop();
                            lx.push(Tok::Dedent);
                        }
                        if col != *indents.last().unwrap_or(&0) {
                            lx.hard_local("unindent does not match any outer indentation level");
                            return lx.out;
                        }
                    }
                }
            }
            at_line_start = false;
        }
        let Some(c) = lx.peek(0) else { break };
        match c {
            ' ' | '\t' | '\u{c}' => lx.i += 1,
            '#' => {
                while lx.peek(0).is_some_and(|c| c != '\n' && c != '\r') {
                    lx.i += 1;
                }
            }
            '\n' | '\r' => {
                lx.eat_newline();
                if lx.stack.is_empty() {
                    if !lx.last_is(|t| matches!(t, Tok::Newline)) && !lx.out.is_empty() {
                        lx.push(Tok::Newline);
                    }
                    at_line_start = true;
                }
                lx.line += 1;
            }
            '\\' => {
                lx.i += 1;
                if lx.eat_newline() {
                    lx.line += 1;
                    if lx.peek(0).is_none() {
                        lx.hard_local("unexpected EOF while parsing");
                        return lx.out;
                    }
                } else {
                    lx.hard_local("unexpected character after line continuation character");
                    return lx.out;
                }
            }
            c if is_ident_start(c) => {
                let start = lx.i;
                while lx.peek(0).is_some_and(is_ident_char) {
                    lx.i += 1;
                }
                let word: String = lx.chars[start..lx.i].iter().collect();
                let quote = lx.peek(0).filter(|q| *q == '\'' || *q == '"');
                let lower = word.to_ascii_lowercase();
                if quote.is_some()
                    && matches!(
                        lower.as_str(),
                        "r" | "u" | "b" | "f" | "br" | "rb" | "fr" | "rf"
                    )
                {
                    if !lex_string(&mut lx, lower.contains('r'), lower.contains('f')) {
                        return lx.out;
                    }
                } else {
                    lx.push(Tok::Name(word));
                }
            }
            c if c.is_ascii_digit()
                || (c == '.' && lx.peek(1).is_some_and(|d| d.is_ascii_digit())) =>
            {
                if !lex_number(&mut lx) {
                    return lx.out;
                }
            }
            '\'' | '"' => {
                if !lex_string(&mut lx, false, false) {
                    return lx.out;
                }
            }
            // CPython hands these to the parser as plain operator tokens that no rule accepts.
            '$' | '?' | '`' => {
                lx.i += 1;
                lx.push(Tok::Op("?"));
            }
            '!' if lx.peek(1) != Some('=') => {
                lx.i += 1;
                lx.push(Tok::Op("?"));
            }
            c if c.is_ascii() => {
                if let Some(op) = OPERATORS
                    .iter()
                    .find(|op| op.chars().enumerate().all(|(k, oc)| lx.peek(k) == Some(oc)))
                {
                    lx.i += op.len();
                    let ch = op.chars().next().unwrap_or(' ');
                    match *op {
                        "(" | "[" | "{" => {
                            if lx.stack.len() >= 200 {
                                lx.hard("too many nested parentheses");
                                return lx.out;
                            }
                            lx.stack.push((ch, lx.line));
                            lx.push(Tok::Op(op));
                        }
                        ")" | "]" | "}" => match lx.stack.pop() {
                            None => {
                                lx.hard(format!("unmatched '{ch}'"));
                                return lx.out;
                            }
                            Some((open, open_line)) => {
                                let want = match open {
                                    '(' => ')',
                                    '[' => ']',
                                    _ => '}',
                                };
                                if ch != want {
                                    let mut msg = format!(
                                        "closing parenthesis '{ch}' does not match opening parenthesis '{open}'"
                                    );
                                    if open_line != lx.line {
                                        msg.push_str(&format!(" on line {open_line}"));
                                    }
                                    lx.hard(msg);
                                    return lx.out;
                                }
                                lx.push(Tok::Op(op));
                            }
                        },
                        _ => lx.push(Tok::Op(op)),
                    }
                } else if !printable(c) {
                    lx.hard(format!(
                        "invalid non-printable character U+{:04X}",
                        c as u32
                    ));
                    return lx.out;
                } else {
                    lx.i += 1;
                    lx.push(Tok::Op("?"));
                }
            }
            c => {
                if !printable(c) {
                    lx.hard(format!(
                        "invalid non-printable character U+{:04X}",
                        c as u32
                    ));
                } else {
                    lx.hard(format!("invalid character '{c}' (U+{:04X})", c as u32));
                }
                return lx.out;
            }
        }
    }
    // End of input.
    if lx.stack.is_empty() && !lx.out.is_empty() && !lx.last_is(|t| matches!(t, Tok::Newline)) {
        lx.push(Tok::Newline);
    }
    if lx.stack.is_empty() {
        while indents.len() > 1 {
            indents.pop();
            lx.push(Tok::Dedent);
        }
    }
    let open = lx.stack.last().copied();
    lx.push(Tok::Eof { open });
    lx.out
}

/// A string literal (the quotes are not evaluated, only found). Returns false after pushing an error.
fn lex_string(lx: &mut Lexer, raw: bool, fstring: bool) -> bool {
    let start_line = lx.line;
    let Some(quote) = lx.peek(0) else {
        return false;
    };
    let triple = lx.peek(1) == Some(quote) && lx.peek(2) == Some(quote);
    lx.i += if triple { 3 } else { 1 };
    loop {
        match lx.peek(0) {
            None => {
                let msg = if triple {
                    format!(
                        "unterminated triple-quoted string literal (detected at line {})",
                        lx.line
                    )
                } else {
                    format!("unterminated string literal (detected at line {start_line})")
                };
                lx.hard(msg);
                return false;
            }
            Some('\\') => {
                lx.i += 1;
                if lx.peek(0).is_some() {
                    let was_newline = lx.eat_newline();
                    if was_newline {
                        lx.line += 1;
                    } else {
                        lx.i += 1;
                    }
                }
                let _ = raw;
            }
            Some('\n') | Some('\r') if !triple => {
                lx.hard(format!(
                    "unterminated string literal (detected at line {start_line})"
                ));
                return false;
            }
            Some('\n') | Some('\r') => {
                lx.eat_newline();
                lx.line += 1;
            }
            Some(c) if c == quote => {
                if triple {
                    if lx.peek(1) == Some(quote) && lx.peek(2) == Some(quote) {
                        lx.i += 3;
                        break;
                    }
                    lx.i += 1;
                } else {
                    lx.i += 1;
                    break;
                }
            }
            Some(_) => lx.i += 1,
        }
    }
    lx.push(Tok::Str { fstring });
    true
}

fn digits(lx: &mut Lexer, radix: u32, kind: &str) -> Result<String, String> {
    // At least one digit, with single underscores between digits.
    let mut text = String::new();
    let mut last_underscore = false;
    loop {
        match lx.peek(0) {
            Some(c) if c.is_digit(radix) => {
                text.push(c);
                last_underscore = false;
                lx.i += 1;
            }
            Some('_') if !text.is_empty() && !last_underscore => {
                last_underscore = true;
                lx.i += 1;
            }
            _ => break,
        }
    }
    if last_underscore || text.is_empty() {
        return Err(format!("invalid {kind} literal"));
    }
    Ok(text)
}

/// CPython's `verify_end_of_number`.
fn verify_end(lx: &mut Lexer, kind: &str) -> bool {
    let Some(c) = lx.peek(0) else { return true };
    if !is_potential_ident_char(c) {
        return true;
    }
    let rest: Vec<char> = lx.chars[lx.i..].iter().take(5).copied().collect();
    // `1and x`, `1else`, `1for`, `1or`, `1not` are still accepted (a keyword that ends there), and so is
    // `1if`, `1in`, `1is` whatever follows.
    let keyword_ends = |kw: &str| {
        let n = kw.chars().count();
        rest.len() >= n
            && rest.iter().take(n).copied().eq(kw.chars())
            && rest.get(n).is_none_or(|c| !is_potential_ident_char(*c))
    };
    let keyword_follows = keyword_ends("and")
        || keyword_ends("else")
        || keyword_ends("for")
        || keyword_ends("or")
        || keyword_ends("not")
        || (rest.first() == Some(&'i') && matches!(rest.get(1), Some('f' | 'n' | 's')));
    if keyword_follows {
        return true;
    }
    lx.hard(format!("invalid {kind} literal"));
    false
}

fn lex_number(lx: &mut Lexer) -> bool {
    let first = lx.peek(0).unwrap_or('0');
    let second = lx.peek(1).map(|c| c.to_ascii_lowercase());
    if first == '0' && matches!(second, Some('x' | 'o' | 'b')) {
        let (radix, kind) = match second {
            Some('x') => (16, "hexadecimal"),
            Some('o') => (8, "octal"),
            _ => (2, "binary"),
        };
        lx.i += 2;
        if lx.peek(0) == Some('_') {
            lx.i += 1;
        }
        let digits_text = match digits(lx, radix, kind) {
            Ok(t) => t,
            Err(e) => {
                // A digit that is valid in decimal but not in this base gets its own message.
                if let Some(c) = lx.peek(0).filter(|c| c.is_ascii_digit() && radix < 10) {
                    lx.hard(format!("invalid digit '{c}' in {kind} literal"));
                } else {
                    lx.hard(e);
                }
                return false;
            }
        };
        if let Some(c) = lx.peek(0).filter(|c| c.is_ascii_digit() && radix < 10) {
            lx.hard(format!("invalid digit '{c}' in {kind} literal"));
            return false;
        }
        if !verify_end(lx, kind) {
            return false;
        }
        let value = i128::from_str_radix(&digits_text, radix)
            .map(Lit::Int)
            .unwrap_or_else(|_| {
                let mut f = 0.0f64;
                for ch in digits_text.chars() {
                    f = f * radix as f64 + ch.to_digit(radix).unwrap_or(0) as f64;
                }
                Lit::Float(f)
            });
        lx.push(Tok::Number(value));
        return true;
    }
    let mut text = String::new();
    let mut is_float = false;
    let mut nonzero_leading = false;
    if first != '.' {
        match digits(lx, 10, "decimal") {
            Ok(t) => {
                if t.starts_with('0') && t.chars().any(|c| c != '0') {
                    nonzero_leading = true;
                }
                text.push_str(&t);
            }
            Err(e) => {
                lx.hard(e);
                return false;
            }
        }
    }
    if lx.peek(0) == Some('.') {
        is_float = true;
        lx.i += 1;
        text.push('.');
        if lx.peek(0).is_some_and(|c| c.is_ascii_digit()) {
            match digits(lx, 10, "decimal") {
                Ok(t) => text.push_str(&t),
                Err(e) => {
                    lx.hard(e);
                    return false;
                }
            }
        }
    }
    if matches!(lx.peek(0), Some('e' | 'E')) {
        let sign = matches!(lx.peek(1), Some('+' | '-'));
        let digit_at = if sign { 2 } else { 1 };
        if lx.peek(digit_at).is_some_and(|c| c.is_ascii_digit()) {
            is_float = true;
            text.push('e');
            if sign {
                text.push(lx.peek(1).unwrap_or('+'));
            }
            lx.i += digit_at;
            match digits(lx, 10, "decimal") {
                Ok(t) => text.push_str(&t),
                Err(e) => {
                    lx.hard(e);
                    return false;
                }
            }
        }
    }
    let mut imag = false;
    if matches!(lx.peek(0), Some('j' | 'J')) {
        imag = true;
        lx.i += 1;
    }
    // (`07else` goes down CPython's exponent path, which never checks for leading zeros.)
    if nonzero_leading && !is_float && !imag && !matches!(lx.peek(0), Some('e' | 'E')) {
        lx.hard("leading zeros in decimal integer literals are not permitted; use an 0o prefix for octal integers");
        return false;
    }
    if !verify_end(lx, if imag { "imaginary" } else { "decimal" }) {
        return false;
    }
    let lit = if imag {
        Lit::Imag
    } else if is_float {
        Lit::Float(text.parse::<f64>().unwrap_or(f64::INFINITY))
    } else {
        text.parse::<i128>()
            .map(Lit::Int)
            .unwrap_or_else(|_| Lit::Float(text.parse::<f64>().unwrap_or(f64::INFINITY)))
    };
    lx.push(Tok::Number(lit));
    true
}

// ================================================================================================
// Syntax tree
// ================================================================================================

#[derive(Clone, Debug)]
enum Node {
    BoolOp {
        op: &'static str,
        values: Vec<Node>,
    },
    BinOp {
        op: &'static str,
        left: Box<Node>,
        right: Box<Node>,
    },
    UnaryOp {
        op: &'static str,
        operand: Box<Node>,
    },
    Compare {
        left: Box<Node>,
        ops: Vec<&'static str>,
        comparators: Vec<Node>,
    },
    IfExp {
        test: Box<Node>,
        body: Box<Node>,
        orelse: Box<Node>,
    },
    Call {
        func: Box<Node>,
        args: Vec<Node>,
        keywords: Vec<Node>,
    },
    Name(String),
    /// An int, float or bool constant.
    Number(Value),
    /// Any other constant: a string, bytes, None, ..., a complex number.
    OtherConstant,
    /// An operator node (`Add`, `LShift`, ...), which `ast.walk` also visits.
    Op(&'static str),
    /// Anything the whitelist rejects, with the children `ast.walk` would visit.
    Other {
        kind: &'static str,
        children: Vec<Node>,
    },
}

fn other(kind: &'static str, children: Vec<Node>) -> Node {
    Node::Other { kind, children }
}

// ================================================================================================
// Parser
// ================================================================================================

enum Fail {
    /// A generic "invalid syntax" at this token.
    Generic(usize),
    /// A specific message from one of CPython's `invalid_*` rules, at this token.
    Custom(String, usize),
    /// A tokenizer exception.
    Hard(String),
}

type PResult<T> = Result<T, Fail>;

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// Set while parsing the second half of the "forgot a comma" probe, which has no custom errors.
    no_invalid: bool,
    /// The furthest token ever looked at (CPython's `p->fill - 1`).
    furthest: std::cell::Cell<usize>,
}

impl Parser {
    fn tok(&self) -> &Tok {
        self.tok_at(0)
    }

    fn tok_at(&self, k: usize) -> &Tok {
        let index = (self.pos + k).min(self.toks.len() - 1);
        if index > self.furthest.get() {
            self.furthest.set(index);
        }
        &self.toks[index].tok
    }

    fn fail<T>(&self) -> PResult<T> {
        match self.tok() {
            Tok::HardError { msg, .. } => Err(Fail::Hard(msg.clone())),
            Tok::Eof {
                open: Some((ch, _)),
            } => Err(Fail::Hard(format!("'{ch}' was never closed"))),
            _ => Err(Fail::Generic(self.pos.min(self.toks.len() - 1))),
        }
    }

    /// Runs a step that PEG would simply not match: a generic failure undoes the step. Tokenizer errors and
    /// custom messages still end the parse.
    fn optional<T>(&mut self, step: impl FnOnce(&mut Parser) -> PResult<T>) -> PResult<Option<T>> {
        let save = self.pos;
        match step(self) {
            Ok(v) => Ok(Some(v)),
            Err(Fail::Generic(_)) => {
                self.pos = save;
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn advance(&mut self) {
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
    }

    /// CPython raises "'(' was never closed" the moment its tokenizer is asked for the token after the
    /// last one while a bracket is open, even in the middle of guessing at another error.
    fn peek_unclosed(&self) -> PResult<()> {
        match self.tok() {
            Tok::Eof {
                open: Some((ch, _)),
            } => Err(Fail::Hard(format!("'{ch}' was never closed"))),
            _ => Ok(()),
        }
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.tok(), Tok::Op(o) if *o == op)
    }

    fn is_op_at(&self, k: usize, op: &str) -> bool {
        matches!(self.tok_at(k), Tok::Op(o) if *o == op)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.tok(), Tok::Name(n) if n == kw)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> PResult<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            self.fail()
        }
    }

    /// Can the current token begin an expression?
    fn starts_expression(&self) -> bool {
        match self.tok() {
            Tok::Name(n) => {
                !KEYWORDS.contains(&n.as_str())
                    || matches!(
                        n.as_str(),
                        "None" | "True" | "False" | "not" | "lambda" | "await"
                    )
            }
            Tok::Number(_) | Tok::Str { .. } => true,
            Tok::Op(o) => matches!(*o, "(" | "[" | "{" | "-" | "+" | "~" | "..."),
            _ => false,
        }
    }

    // --- expressions ---

    /// `expressions` at the top: one expression, or several joined by commas (a tuple).
    fn parse_top(&mut self) -> PResult<Node> {
        let first = self.parse_expression()?;
        if !self.is_op(",") {
            return Ok(first);
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.starts_expression() {
                elts.push(self.parse_expression()?);
            } else {
                break;
            }
        }
        Ok(other("Tuple", elts))
    }

    fn parse_expression(&mut self) -> PResult<Node> {
        if self.is_kw("lambda") {
            return self.parse_lambda();
        }
        let start = self.pos;
        let a = self.parse_disjunction()?;
        if self.is_kw("if") {
            let start_of_if = self.pos;
            self.advance();
            let test = self.parse_disjunction()?;
            self.peek_unclosed()?;
            if !self.is_kw("else") {
                if !self.no_invalid && !self.is_op(":") {
                    return Err(Fail::Custom(
                        "expected 'else' after 'if' expression".into(),
                        self.pos,
                    ));
                }
                return self.fail();
            }
            self.advance();
            let Some(orelse) = self.optional(|p| p.parse_expression())? else {
                // `a if b else` without a usable tail: the ternary does not match, `a` stands alone.
                self.pos = start_of_if;
                return Ok(a);
            };
            return Ok(Node::IfExp {
                test: Box::new(test),
                body: Box::new(a),
                orelse: Box::new(orelse),
            });
        }
        if !self.no_invalid && self.starts_expression() {
            // `a b` inside brackets: CPython guesses a missing comma.
            let soft_start = match &self.toks[start].tok {
                Tok::Name(n) => {
                    matches!(
                        n.as_str(),
                        "match" | "case" | "type" | "_" | "print" | "exec"
                    ) || matches!(self.toks[start + 1].tok, Tok::Str { .. })
                }
                _ => false,
            };
            if !soft_start {
                let save = self.pos;
                self.no_invalid = true;
                let b = self.parse_expression();
                self.no_invalid = false;
                if let Err(Fail::Hard(msg)) = b {
                    return Err(Fail::Hard(msg));
                }
                if b.is_ok() {
                    self.peek_unclosed()?;
                    let last_level = self.toks[self.pos - 1].level;
                    if last_level != 0 {
                        return Err(Fail::Custom(
                            "invalid syntax. Perhaps you forgot a comma?".into(),
                            save,
                        ));
                    }
                }
                self.pos = save;
            }
        }
        Ok(a)
    }

    fn parse_lambda(&mut self) -> PResult<Node> {
        self.advance(); // lambda
        let mut params = Vec::new();
        while !self.is_op(":") {
            match self.tok().clone() {
                Tok::Name(n) if !KEYWORDS.contains(&n.as_str()) => {
                    self.advance();
                    params.push(other("arg", vec![]));
                    if self.eat_op("=") {
                        params.push(self.parse_expression()?);
                    }
                }
                Tok::Op("*") | Tok::Op("**") | Tok::Op("/") | Tok::Op(",") => self.advance(),
                _ => return self.fail(),
            }
        }
        self.advance(); // :
        let body = self.parse_expression()?;
        Ok(other("Lambda", vec![other("arguments", params), body]))
    }

    fn parse_disjunction(&mut self) -> PResult<Node> {
        let first = self.parse_conjunction()?;
        let mut values = vec![first];
        while self.is_kw("or") {
            let save = self.pos;
            self.advance();
            match self.optional(|p| p.parse_conjunction())? {
                Some(v) => values.push(v),
                None => {
                    self.pos = save;
                    break;
                }
            }
        }
        if values.len() == 1 {
            return Ok(values.remove(0));
        }
        Ok(Node::BoolOp { op: "Or", values })
    }

    fn parse_conjunction(&mut self) -> PResult<Node> {
        let first = self.parse_inversion()?;
        let mut values = vec![first];
        while self.is_kw("and") {
            let save = self.pos;
            self.advance();
            match self.optional(|p| p.parse_inversion())? {
                Some(v) => values.push(v),
                None => {
                    self.pos = save;
                    break;
                }
            }
        }
        if values.len() == 1 {
            return Ok(values.remove(0));
        }
        Ok(Node::BoolOp { op: "And", values })
    }

    fn parse_inversion(&mut self) -> PResult<Node> {
        if self.eat_kw("not") {
            let operand = self.parse_inversion()?;
            return Ok(Node::UnaryOp {
                op: "Not",
                operand: Box::new(operand),
            });
        }
        self.parse_comparison()
    }

    fn compare_op(&mut self) -> Option<&'static str> {
        let op = match self.tok() {
            Tok::Op("==") => "Eq",
            Tok::Op("!=") => "NotEq",
            Tok::Op("<") => "Lt",
            Tok::Op("<=") => "LtE",
            Tok::Op(">") => "Gt",
            Tok::Op(">=") => "GtE",
            Tok::Name(n) if n == "in" => "In",
            Tok::Name(n) if n == "is" => {
                if matches!(self.tok_at(1), Tok::Name(m) if m == "not") {
                    self.advance();
                    "IsNot"
                } else {
                    "Is"
                }
            }
            Tok::Name(n) if n == "not" => {
                if matches!(self.tok_at(1), Tok::Name(m) if m == "in") {
                    self.advance();
                    "NotIn"
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        self.advance();
        Some(op)
    }

    fn parse_comparison(&mut self) -> PResult<Node> {
        let left = self.parse_bitwise_or()?;
        let mut ops = Vec::new();
        let mut comparators = Vec::new();
        loop {
            let save = self.pos;
            let Some(op) = self.compare_op() else { break };
            match self.optional(|p| p.parse_bitwise_or())? {
                Some(right) => {
                    ops.push(op);
                    comparators.push(right);
                }
                None => {
                    self.pos = save;
                    break;
                }
            }
        }
        if ops.is_empty() {
            Ok(left)
        } else {
            Ok(Node::Compare {
                left: Box::new(left),
                ops,
                comparators,
            })
        }
    }

    fn parse_binary(
        &mut self,
        ops: &[(&str, &'static str)],
        next: fn(&mut Parser) -> PResult<Node>,
    ) -> PResult<Node> {
        let mut left = next(self)?;
        loop {
            let Some(name) = ops
                .iter()
                .find(|(sym, _)| self.is_op(sym))
                .map(|(_, name)| *name)
            else {
                return Ok(left);
            };
            let save = self.pos;
            self.advance();
            let Some(right) = self.optional(next)? else {
                // An operator without a usable right side is simply not part of this expression.
                self.pos = save;
                return Ok(left);
            };
            left = Node::BinOp {
                op: name,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    fn parse_bitwise_or(&mut self) -> PResult<Node> {
        self.parse_binary(&[("|", "BitOr")], Parser::parse_bitwise_xor)
    }

    fn parse_bitwise_xor(&mut self) -> PResult<Node> {
        self.parse_binary(&[("^", "BitXor")], Parser::parse_bitwise_and)
    }

    fn parse_bitwise_and(&mut self) -> PResult<Node> {
        self.parse_binary(&[("&", "BitAnd")], Parser::parse_shift)
    }

    fn parse_shift(&mut self) -> PResult<Node> {
        self.parse_binary(&[("<<", "LShift"), (">>", "RShift")], Parser::parse_sum)
    }

    fn parse_sum(&mut self) -> PResult<Node> {
        self.parse_binary(&[("+", "Add"), ("-", "Sub")], Parser::parse_term)
    }

    fn parse_term(&mut self) -> PResult<Node> {
        self.parse_binary(
            &[
                ("*", "Mult"),
                ("/", "Div"),
                ("//", "FloorDiv"),
                ("%", "Mod"),
                ("@", "MatMult"),
            ],
            Parser::parse_factor,
        )
    }

    fn parse_factor(&mut self) -> PResult<Node> {
        let op = match self.tok() {
            Tok::Op("+") => "UAdd",
            Tok::Op("-") => "USub",
            Tok::Op("~") => "Invert",
            _ => return self.parse_power(),
        };
        self.advance();
        let operand = self.parse_factor()?;
        Ok(Node::UnaryOp {
            op,
            operand: Box::new(operand),
        })
    }

    fn parse_power(&mut self) -> PResult<Node> {
        let base = if self.eat_kw("await") {
            let value = self.parse_primary()?;
            other("Await", vec![value])
        } else {
            self.parse_primary()?
        };
        if self.is_op("**") {
            let save = self.pos;
            self.advance();
            return match self.optional(|p| p.parse_factor())? {
                Some(exponent) => Ok(Node::BinOp {
                    op: "Pow",
                    left: Box::new(base),
                    right: Box::new(exponent),
                }),
                None => {
                    self.pos = save;
                    Ok(base)
                }
            };
        }
        Ok(base)
    }

    fn parse_primary(&mut self) -> PResult<Node> {
        let mut node = self.parse_atom()?;
        loop {
            if self.eat_op(".") {
                match self.tok() {
                    Tok::Name(n) if !KEYWORDS.contains(&n.as_str()) => self.advance(),
                    _ => return self.fail(),
                }
                node = other("Attribute", vec![node]);
            } else if self.is_op("(") {
                node = self.parse_call(node)?;
            } else if self.is_op("[") {
                self.advance();
                let slice = self.parse_slices()?;
                self.expect_op("]")?;
                node = other("Subscript", vec![node, slice]);
            } else {
                return Ok(node);
            }
        }
    }

    fn parse_slices(&mut self) -> PResult<Node> {
        let mut items = vec![self.parse_slice()?];
        let mut comma = false;
        while self.eat_op(",") {
            comma = true;
            if self.is_op("]") {
                break;
            }
            items.push(self.parse_slice()?);
        }
        if comma {
            Ok(other("Tuple", items))
        } else {
            Ok(items.remove(0))
        }
    }

    fn parse_slice(&mut self) -> PResult<Node> {
        let mut parts = Vec::new();
        let mut is_slice = false;
        if !self.is_op(":") {
            if self.is_op("*") {
                self.advance();
                let value = self.parse_bitwise_or()?;
                return Ok(other("Starred", vec![value]));
            }
            parts.push(self.parse_named_expression()?);
        }
        if self.is_op(":") {
            is_slice = true;
            self.advance();
            if self.starts_expression() {
                parts.push(self.parse_expression()?);
            }
            if self.eat_op(":") && self.starts_expression() {
                parts.push(self.parse_expression()?);
            }
        }
        if is_slice {
            Ok(other("Slice", parts))
        } else {
            Ok(parts.remove(0))
        }
    }

    fn parse_named_expression(&mut self) -> PResult<Node> {
        if let Tok::Name(n) = self.tok().clone() {
            if !KEYWORDS.contains(&n.as_str()) && self.is_op_at(1, ":=") {
                self.advance();
                self.advance();
                let value = self.parse_expression()?;
                return Ok(other("NamedExpr", vec![Node::Name(n), value]));
            }
        }
        self.parse_expression()
    }

    fn parse_call(&mut self, func: Node) -> PResult<Node> {
        self.advance(); // (
        let mut args = Vec::new();
        let mut keywords = Vec::new();
        while !self.is_op(")") {
            if self.eat_op("*") {
                let value = self.parse_expression()?;
                args.push(other("Starred", vec![value]));
            } else if self.eat_op("**") {
                let value = self.parse_expression()?;
                keywords.push(other("keyword", vec![value]));
            } else if matches!(self.tok(), Tok::Name(n) if !KEYWORDS.contains(&n.as_str()))
                && self.is_op_at(1, "=")
            {
                self.advance();
                self.advance();
                let value = self.parse_expression()?;
                keywords.push(other("keyword", vec![value]));
            } else {
                let value = self.parse_named_expression()?;
                if self.is_kw("for") || self.is_kw("async") {
                    let generators = self.parse_comprehension_clauses()?;
                    let mut children = vec![value];
                    children.extend(generators);
                    args.push(other("GeneratorExp", children));
                } else {
                    args.push(value);
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(Node::Call {
            func: Box::new(func),
            args,
            keywords,
        })
    }

    fn parse_comprehension_clauses(&mut self) -> PResult<Vec<Node>> {
        let mut out = Vec::new();
        while self.is_kw("for") || self.is_kw("async") {
            self.eat_kw("async");
            if !self.eat_kw("for") {
                return self.fail();
            }
            let mut children = Vec::new();
            loop {
                self.eat_op("*");
                children.push(self.parse_bitwise_or()?);
                if !self.eat_op(",") {
                    break;
                }
                if self.is_kw("in") {
                    break;
                }
            }
            if !self.eat_kw("in") {
                return self.fail();
            }
            children.push(self.parse_disjunction()?);
            while self.eat_kw("if") {
                children.push(self.parse_disjunction()?);
            }
            out.push(other("comprehension", children));
        }
        Ok(out)
    }

    fn parse_star_or_named(&mut self) -> PResult<Node> {
        if self.eat_op("*") {
            let value = self.parse_bitwise_or()?;
            return Ok(other("Starred", vec![value]));
        }
        self.parse_named_expression()
    }

    fn parse_atom(&mut self) -> PResult<Node> {
        match self.tok().clone() {
            Tok::Name(n) => match n.as_str() {
                "True" => {
                    self.advance();
                    Ok(Node::Number(Value::Bool(true)))
                }
                "False" => {
                    self.advance();
                    Ok(Node::Number(Value::Bool(false)))
                }
                "None" => {
                    self.advance();
                    Ok(Node::OtherConstant)
                }
                _ if KEYWORDS.contains(&n.as_str()) => self.fail(),
                _ => {
                    self.advance();
                    Ok(Node::Name(n))
                }
            },
            Tok::Number(lit) => {
                self.advance();
                Ok(match lit {
                    Lit::Int(i) => Node::Number(Value::Int(i)),
                    Lit::Float(f) => Node::Number(Value::Float(f)),
                    Lit::Imag => Node::OtherConstant,
                })
            }
            Tok::Str { fstring } => {
                let mut any_f = fstring;
                self.advance();
                while let Tok::Str { fstring } = self.tok() {
                    any_f |= *fstring;
                    self.advance();
                }
                Ok(if any_f {
                    other("JoinedStr", vec![])
                } else {
                    Node::OtherConstant
                })
            }
            Tok::Op("...") => {
                self.advance();
                Ok(Node::OtherConstant)
            }
            Tok::Op("(") => {
                self.advance();
                self.parse_paren()
            }
            Tok::Op("[") => {
                self.advance();
                if self.eat_op("]") {
                    return Ok(other("List", vec![]));
                }
                let first = self.parse_star_or_named()?;
                if self.is_kw("for") || self.is_kw("async") {
                    let mut children = vec![first];
                    children.extend(self.parse_comprehension_clauses()?);
                    self.expect_op("]")?;
                    return Ok(other("ListComp", children));
                }
                let mut elts = vec![first];
                while self.eat_op(",") {
                    if self.is_op("]") {
                        break;
                    }
                    elts.push(self.parse_star_or_named()?);
                }
                self.expect_op("]")?;
                Ok(other("List", elts))
            }
            Tok::Op("{") => {
                self.advance();
                self.parse_brace()
            }
            _ => self.fail(),
        }
    }

    fn parse_paren(&mut self) -> PResult<Node> {
        if self.eat_op(")") {
            return Ok(other("Tuple", vec![]));
        }
        if self.is_kw("yield") {
            self.advance();
            let mut children = Vec::new();
            if self.starts_expression() {
                children.push(self.parse_expression()?);
            }
            self.expect_op(")")?;
            return Ok(other("Yield", children));
        }
        let starred = self.is_op("*");
        let first = self.parse_star_or_named()?;
        if self.is_kw("for") || self.is_kw("async") {
            let mut children = vec![first];
            children.extend(self.parse_comprehension_clauses()?);
            self.expect_op(")")?;
            return Ok(other("GeneratorExp", children));
        }
        if self.is_op(",") {
            let mut elts = vec![first];
            while self.eat_op(",") {
                if self.is_op(")") {
                    break;
                }
                elts.push(self.parse_star_or_named()?);
            }
            self.expect_op(")")?;
            return Ok(other("Tuple", elts));
        }
        if starred {
            if self.is_op(")") {
                return Err(Fail::Custom(
                    "cannot use starred expression here".into(),
                    self.pos,
                ));
            }
            return self.fail();
        }
        self.expect_op(")")?;
        Ok(first)
    }

    fn parse_brace(&mut self) -> PResult<Node> {
        if self.eat_op("}") {
            return Ok(other("Dict", vec![]));
        }
        if self.eat_op("**") {
            let value = self.parse_bitwise_or()?;
            let mut children = vec![value];
            while self.eat_op(",") {
                if self.is_op("}") {
                    break;
                }
                if self.eat_op("**") {
                    children.push(self.parse_bitwise_or()?);
                } else {
                    children.push(self.parse_expression()?);
                    self.expect_op(":")?;
                    children.push(self.parse_expression()?);
                }
            }
            self.expect_op("}")?;
            return Ok(other("Dict", children));
        }
        let first = self.parse_star_or_named()?;
        if self.eat_op(":") {
            let value = self.parse_expression()?;
            if self.is_kw("for") || self.is_kw("async") {
                let mut children = vec![first, value];
                children.extend(self.parse_comprehension_clauses()?);
                self.expect_op("}")?;
                return Ok(other("DictComp", children));
            }
            let mut children = vec![first, value];
            while self.eat_op(",") {
                if self.is_op("}") {
                    break;
                }
                if self.eat_op("**") {
                    children.push(self.parse_bitwise_or()?);
                } else {
                    children.push(self.parse_expression()?);
                    self.expect_op(":")?;
                    children.push(self.parse_expression()?);
                }
            }
            self.expect_op("}")?;
            return Ok(other("Dict", children));
        }
        if self.is_kw("for") || self.is_kw("async") {
            let mut children = vec![first];
            children.extend(self.parse_comprehension_clauses()?);
            self.expect_op("}")?;
            return Ok(other("SetComp", children));
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.is_op("}") {
                break;
            }
            elts.push(self.parse_star_or_named()?);
        }
        self.expect_op("}")?;
        Ok(other("Set", elts))
    }
}

/// Source text to a tree, or the message of the `SyntaxError` Python would raise.
fn parse_source(src: &str) -> Result<Node, String> {
    let toks = tokenize(src);
    let mut p = Parser {
        toks,
        pos: 0,
        no_invalid: false,
        furthest: std::cell::Cell::new(0),
    };
    let result = (|| -> PResult<Node> {
        let node = p.parse_top()?;
        while matches!(p.tok(), Tok::Newline) {
            p.advance();
        }
        if !matches!(p.tok(), Tok::Eof { .. }) {
            return p.fail();
        }
        Ok(node)
    })();
    let fail = match result {
        Ok(node) => return Ok(node),
        Err(fail) => fail,
    };
    let (index, custom) = match fail {
        // The parser reached a tokenizer error (or the end of input inside brackets) itself.
        Fail::Hard(msg) => return Err(msg),
        Fail::Generic(i) => (i, None),
        Fail::Custom(msg, i) => (i, Some(msg)),
    };
    // An indent where nothing may start is reported as such, before any scan of the rest.
    match &p.toks[index].tok {
        Tok::Indent => return Err("unexpected indent".into()),
        Tok::Dedent => return Err("unexpected unindent".into()),
        _ => {}
    }
    // After a syntax error CPython tokenizes the rest of the source, and an exception raised there
    // replaces the error found by the parser.
    let later = p.toks.iter().find_map(|t| match &t.tok {
        Tok::HardError { msg, global: true } => Some(msg.clone()),
        _ => None,
    });
    if let Some(msg) = later {
        return Err(msg);
    }
    // A bracket still open when the input ends is reported if the parser got past the line it was opened on.
    if let Some(Token {
        tok: Tok::Eof {
            open: Some((ch, line)),
        },
        ..
    }) = p.toks.last()
    {
        if p.toks[p.furthest.get().min(p.toks.len() - 1)].line > *line {
            return Err(format!("'{ch}' was never closed"));
        }
    }
    Err(custom.unwrap_or_else(|| "invalid syntax".into()))
}

// ================================================================================================
// The whitelist
// ================================================================================================

#[rustfmt::skip]
const FUNCTIONS: &[&str] = &[
    "sin",
    "cos",
    "tan",
    "atan",
    "atan2",
    "sqrt",
    "abs",
    "floor",
    "ceil",
    "exp",
    "log",
    "hypot",
    "min",
    "max",
    "round",
    "noise",
    "int",
    "pow",
];
const CONSTANTS: &[&str] = &["pi", "tau", "e"];
const VARIABLES: &[&str] = &["x", "y", "u", "v", "r", "a", "W", "H"];

#[rustfmt::skip]
const ALLOWED_KINDS: &[&str] = &[
    "Expression",
    "BinOp",
    "UnaryOp",
    "BoolOp",
    "Compare",
    "IfExp",
    "Call",
    "Name",
    "Load",
    "Constant",
    "Add",
    "Sub",
    "Mult",
    "Div",
    "FloorDiv",
    "Mod",
    "Pow",
    "USub",
    "UAdd",
    "Not",
    "And",
    "Or",
    "Lt",
    "LtE",
    "Gt",
    "GtE",
    "Eq",
    "NotEq",
    "BitXor",
    "BitAnd",
    "BitOr",
];

fn kind_of(node: &Node) -> &'static str {
    match node {
        Node::BoolOp { .. } => "BoolOp",
        Node::BinOp { .. } => "BinOp",
        Node::UnaryOp { .. } => "UnaryOp",
        Node::Compare { .. } => "Compare",
        Node::IfExp { .. } => "IfExp",
        Node::Call { .. } => "Call",
        Node::Name(_) => "Name",
        Node::Number(_) | Node::OtherConstant => "Constant",
        Node::Op(name) => name,
        Node::Other { kind, .. } => kind,
    }
}

fn children_of(node: &Node) -> Vec<&Node> {
    // Operator nodes are made on the fly in `validate`; here only real nodes.
    match node {
        Node::BoolOp { values, .. } => values.iter().collect(),
        Node::BinOp { left, right, .. } => vec![left, right],
        Node::UnaryOp { operand, .. } => vec![operand],
        Node::Compare {
            left, comparators, ..
        } => {
            let mut v: Vec<&Node> = vec![left];
            v.extend(comparators.iter());
            v
        }
        Node::IfExp { test, body, orelse } => vec![test, body, orelse],
        Node::Call {
            func,
            args,
            keywords,
        } => {
            let mut v: Vec<&Node> = vec![func];
            v.extend(args.iter());
            v.extend(keywords.iter());
            v
        }
        Node::Other { children, .. } => children.iter().collect(),
        _ => Vec::new(),
    }
}

/// `ast.walk` order: breadth first, children in field order (operators included).
fn walk_children(node: &Node) -> Vec<Node> {
    match node {
        Node::BoolOp { op, values } => {
            let mut v = vec![Node::Op(op)];
            v.extend(values.iter().cloned());
            v
        }
        Node::BinOp { op, left, right } => vec![(**left).clone(), Node::Op(op), (**right).clone()],
        Node::UnaryOp { op, operand } => vec![Node::Op(op), (**operand).clone()],
        Node::Compare {
            left,
            ops,
            comparators,
        } => {
            let mut v = vec![(**left).clone()];
            v.extend(ops.iter().map(|o| Node::Op(o)));
            v.extend(comparators.iter().cloned());
            v
        }
        other => children_of(other).into_iter().cloned().collect(),
    }
}

fn validate(root: &Node) -> Result<(), FormulaError> {
    let err = |m: String| Err(FormulaError(m));
    // The Expression wrapper is allowed and has the body as its only child.
    let mut queue: std::collections::VecDeque<Node> = std::collections::VecDeque::new();
    queue.push_back(root.clone());
    while let Some(node) = queue.pop_front() {
        let kind = kind_of(&node);
        if !ALLOWED_KINDS.contains(&kind) {
            return err(format!("{kind} isn't allowed in a formula."));
        }
        match &node {
            Node::Name(id) => {
                let known = VARIABLES.contains(&id.as_str())
                    || FUNCTIONS.contains(&id.as_str())
                    || CONSTANTS.contains(&id.as_str());
                if !known {
                    return err(format!(
                        "Unknown name {} in formula.",
                        patterns::python_repr(id)
                    ));
                }
            }
            Node::Call { func, .. } => {
                let ok = matches!(&**func, Node::Name(n) if FUNCTIONS.contains(&n.as_str()));
                if !ok {
                    return err("Only the built-in functions can be called.".into());
                }
            }
            Node::OtherConstant => return err("Only numbers are allowed in a formula.".into()),
            _ => {}
        }
        queue.extend(walk_children(&node));
    }
    Ok(())
}

// ================================================================================================
// Evaluation, with Python's numeric tower
// ================================================================================================

#[derive(Clone, Copy, Debug)]
enum Func {
    Sin,
    Cos,
    Tan,
    Atan,
    Atan2,
    Sqrt,
    Abs,
    Floor,
    Ceil,
    Exp,
    Log,
    Hypot,
    Min,
    Max,
    Round,
    Noise,
    Int,
    Pow,
}

fn func_by_name(name: &str) -> Option<Func> {
    Some(match name {
        "sin" => Func::Sin,
        "cos" => Func::Cos,
        "tan" => Func::Tan,
        "atan" => Func::Atan,
        "atan2" => Func::Atan2,
        "sqrt" => Func::Sqrt,
        "abs" => Func::Abs,
        "floor" => Func::Floor,
        "ceil" => Func::Ceil,
        "exp" => Func::Exp,
        "log" => Func::Log,
        "hypot" => Func::Hypot,
        "min" => Func::Min,
        "max" => Func::Max,
        "round" => Func::Round,
        "noise" => Func::Noise,
        "int" => Func::Int,
        "pow" => Func::Pow,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug)]
enum Value {
    Bool(bool),
    Int(i128),
    Float(f64),
    Complex(f64, f64),
    Func(Func),
}

/// Any exception Python's `except (ArithmeticError, ValueError, TypeError)` would catch.
#[derive(Debug, Clone, Copy)]
struct Raised;

type EvalResult = Result<Value, Raised>;

struct Env {
    x: i128,
    y: i128,
    w: i128,
    h: i128,
    u: f64,
    v: f64,
    r: f64,
    a: f64,
}

fn truthy(v: Value) -> bool {
    match v {
        Value::Bool(b) => b,
        Value::Int(i) => i != 0,
        Value::Float(f) => f != 0.0,
        Value::Complex(re, im) => re != 0.0 || im != 0.0,
        Value::Func(_) => true,
    }
}

/// An int-like value (bool or int).
fn as_int(v: Value) -> Option<i128> {
    match v {
        Value::Bool(b) => Some(b as i128),
        Value::Int(i) => Some(i),
        _ => None,
    }
}

/// A real number as a float (`float(v)`), or a TypeError.
fn to_float(v: Value) -> Result<f64, Raised> {
    match v {
        Value::Bool(b) => Ok(b as i32 as f64),
        Value::Int(i) => Ok(i as f64),
        Value::Float(f) => Ok(f),
        _ => Err(Raised),
    }
}

fn to_complex(v: Value) -> Result<(f64, f64), Raised> {
    match v {
        Value::Complex(re, im) => Ok((re, im)),
        other => Ok((to_float(other)?, 0.0)),
    }
}

fn is_real(v: Value) -> bool {
    matches!(v, Value::Bool(_) | Value::Int(_) | Value::Float(_))
}

fn int_or_float(i: Option<i128>, fallback: f64) -> Value {
    match i {
        Some(i) => Value::Int(i),
        None => Value::Float(fallback),
    }
}

fn float_floor_div_mod(a: f64, b: f64) -> Result<(f64, f64), Raised> {
    if b == 0.0 {
        return Err(Raised);
    }
    let mut modulus = a % b;
    let mut div = (a - modulus) / b;
    if modulus != 0.0 {
        if (b < 0.0) != (modulus < 0.0) {
            modulus += b;
            div -= 1.0;
        }
    } else {
        modulus = 0.0f64.copysign(b);
    }
    let floordiv = if div != 0.0 {
        let mut f = div.floor();
        if div - f > 0.5 {
            f += 1.0;
        }
        f
    } else {
        0.0f64.copysign(a / b)
    };
    Ok((floordiv, modulus))
}

fn is_odd_integer(v: f64) -> bool {
    v.is_finite() && v == v.floor() && (v % 2.0).abs() == 1.0
}

fn float_pow(v: f64, w: f64) -> EvalResult {
    if w == 0.0 {
        return Ok(Value::Float(1.0));
    }
    if v.is_nan() {
        return Ok(Value::Float(v));
    }
    if w.is_nan() {
        return Ok(Value::Float(if v == 1.0 { 1.0 } else { w }));
    }
    if w.is_infinite() {
        let iv = v.abs();
        return Ok(Value::Float(if iv == 1.0 {
            1.0
        } else if (w > 0.0) == (iv > 1.0) {
            f64::INFINITY
        } else {
            0.0
        }));
    }
    if v.is_infinite() {
        let odd = is_odd_integer(w);
        return Ok(Value::Float(if w > 0.0 {
            if odd {
                v
            } else {
                v.abs()
            }
        } else if odd {
            0.0f64.copysign(v)
        } else {
            0.0
        }));
    }
    if v == 0.0 {
        if w < 0.0 {
            return Err(Raised);
        }
        return Ok(Value::Float(if is_odd_integer(w) { v } else { 0.0 }));
    }
    if v < 0.0 && w != w.floor() {
        // A negative base to a fractional power is a complex number in Python 3.
        let r = v.abs().powf(w);
        let theta = std::f64::consts::PI * w;
        return Ok(Value::Complex(r * theta.cos(), r * theta.sin()));
    }
    let result = v.powf(w);
    if result.is_infinite() {
        return Err(Raised);
    }
    Ok(Value::Float(result))
}

fn binop(op: &str, a: Value, b: Value) -> EvalResult {
    if matches!(a, Value::Func(_)) || matches!(b, Value::Func(_)) {
        return Err(Raised);
    }
    match op {
        "BitOr" | "BitAnd" | "BitXor" => {
            if let (Value::Bool(p), Value::Bool(q)) = (a, b) {
                return Ok(Value::Bool(match op {
                    "BitOr" => p | q,
                    "BitAnd" => p & q,
                    _ => p ^ q,
                }));
            }
            match (as_int(a), as_int(b)) {
                (Some(p), Some(q)) => Ok(Value::Int(match op {
                    "BitOr" => p | q,
                    "BitAnd" => p & q,
                    _ => p ^ q,
                })),
                _ => Err(Raised),
            }
        }
        "Add" | "Sub" | "Mult" => {
            if let (Some(p), Some(q)) = (as_int(a), as_int(b)) {
                let exact = match op {
                    "Add" => p.checked_add(q),
                    "Sub" => p.checked_sub(q),
                    _ => p.checked_mul(q),
                };
                let approx = match op {
                    "Add" => p as f64 + q as f64,
                    "Sub" => p as f64 - q as f64,
                    _ => p as f64 * q as f64,
                };
                return Ok(int_or_float(exact, approx));
            }
            if matches!(a, Value::Complex(..)) || matches!(b, Value::Complex(..)) {
                let (ar, ai) = to_complex(a)?;
                let (br, bi) = to_complex(b)?;
                return Ok(match op {
                    "Add" => Value::Complex(ar + br, ai + bi),
                    "Sub" => Value::Complex(ar - br, ai - bi),
                    _ => Value::Complex(ar * br - ai * bi, ar * bi + ai * br),
                });
            }
            let (p, q) = (to_float(a)?, to_float(b)?);
            Ok(Value::Float(match op {
                "Add" => p + q,
                "Sub" => p - q,
                _ => p * q,
            }))
        }
        "Div" => {
            if matches!(a, Value::Complex(..)) || matches!(b, Value::Complex(..)) {
                let (ar, ai) = to_complex(a)?;
                let (br, bi) = to_complex(b)?;
                let d = br * br + bi * bi;
                if d == 0.0 {
                    return Err(Raised);
                }
                return Ok(Value::Complex(
                    (ar * br + ai * bi) / d,
                    (ai * br - ar * bi) / d,
                ));
            }
            let (p, q) = (to_float(a)?, to_float(b)?);
            if q == 0.0 {
                return Err(Raised);
            }
            Ok(Value::Float(p / q))
        }
        "FloorDiv" | "Mod" => {
            if !is_real(a) || !is_real(b) {
                return Err(Raised);
            }
            if let (Some(p), Some(q)) = (as_int(a), as_int(b)) {
                if q == 0 {
                    return Err(Raised);
                }
                let (Some(quot), Some(rem)) = (p.checked_div(q), p.checked_rem(q)) else {
                    return Ok(Value::Float(0.0));
                };
                let adjust = rem != 0 && ((rem < 0) != (q < 0));
                return Ok(Value::Int(if op == "FloorDiv" {
                    if adjust {
                        quot - 1
                    } else {
                        quot
                    }
                } else if adjust {
                    rem + q
                } else {
                    rem
                }));
            }
            let (floordiv, modulus) = float_floor_div_mod(to_float(a)?, to_float(b)?)?;
            Ok(Value::Float(if op == "FloorDiv" {
                floordiv
            } else {
                modulus
            }))
        }
        "Pow" => {
            if let (Some(p), Some(q)) = (as_int(a), as_int(b)) {
                if q < 0 {
                    if p == 0 {
                        return Err(Raised);
                    }
                    return float_pow(p as f64, q as f64);
                }
                let exact = u32::try_from(q).ok().and_then(|e| p.checked_pow(e));
                return Ok(match exact {
                    Some(i) => Value::Int(i),
                    None => {
                        let f = (p as f64).powf(q as f64);
                        if f.is_infinite() {
                            return Err(Raised);
                        }
                        Value::Float(f)
                    }
                });
            }
            if matches!(a, Value::Complex(..)) || matches!(b, Value::Complex(..)) {
                return Err(Raised);
            }
            float_pow(to_float(a)?, to_float(b)?)
        }
        _ => Err(Raised),
    }
}

fn cmp_real(op: &str, a: Value, b: Value) -> Result<bool, Raised> {
    if !is_real(a) || !is_real(b) {
        return Err(Raised);
    }
    if let (Some(p), Some(q)) = (as_int(a), as_int(b)) {
        return Ok(match op {
            "Lt" => p < q,
            "LtE" => p <= q,
            "Gt" => p > q,
            _ => p >= q,
        });
    }
    let (p, q) = (to_float(a)?, to_float(b)?);
    Ok(match op {
        "Lt" => p < q,
        "LtE" => p <= q,
        "Gt" => p > q,
        _ => p >= q,
    })
}

fn equal(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Func(p), Value::Func(q)) => {
            std::mem::discriminant(&p) == std::mem::discriminant(&q)
        }
        (Value::Func(_), _) | (_, Value::Func(_)) => false,
        (Value::Complex(..), _) | (_, Value::Complex(..)) => match (to_complex(a), to_complex(b)) {
            (Ok((ar, ai)), Ok((br, bi))) => ar == br && ai == bi,
            _ => false,
        },
        _ => {
            if let (Some(p), Some(q)) = (as_int(a), as_int(b)) {
                p == q
            } else {
                matches!((to_float(a), to_float(b)), (Ok(p), Ok(q)) if p == q)
            }
        }
    }
}

fn compare(op: &str, a: Value, b: Value) -> Result<bool, Raised> {
    match op {
        "Eq" => Ok(equal(a, b)),
        "NotEq" => Ok(!equal(a, b)),
        _ => cmp_real(op, a, b),
    }
}

fn unary(op: &str, v: Value) -> EvalResult {
    match op {
        "Not" => Ok(Value::Bool(!truthy(v))),
        "USub" => match v {
            Value::Bool(b) => Ok(Value::Int(-(b as i128))),
            Value::Int(i) => Ok(int_or_float(i.checked_neg(), -(i as f64))),
            Value::Float(f) => Ok(Value::Float(-f)),
            Value::Complex(re, im) => Ok(Value::Complex(-re, -im)),
            Value::Func(_) => Err(Raised),
        },
        "UAdd" => match v {
            Value::Bool(b) => Ok(Value::Int(b as i128)),
            Value::Func(_) => Err(Raised),
            other => Ok(other),
        },
        _ => Err(Raised),
    }
}

/// An f64 as a Python int, when it fits (otherwise the float itself stands in for the huge int).
fn float_to_int(f: f64) -> EvalResult {
    if f.is_nan() || f.is_infinite() {
        return Err(Raised);
    }
    if f.abs() < 1.0e38 {
        Ok(Value::Int(f as i128))
    } else {
        Ok(Value::Float(f))
    }
}

fn round_half_even(f: f64) -> f64 {
    let r = f.round();
    if (f - f.trunc()).abs() == 0.5 {
        2.0 * (f / 2.0).round()
    } else {
        r
    }
}

fn call(func: Func, args: &[Value]) -> EvalResult {
    let one = |args: &[Value]| -> Result<Value, Raised> {
        if args.len() == 1 {
            Ok(args[0])
        } else {
            Err(Raised)
        }
    };
    let math1 = |args: &[Value], f: fn(f64) -> f64, domain: bool| -> EvalResult {
        let v = to_float(one(args)?)?;
        if domain && v.is_infinite() {
            return Err(Raised);
        }
        Ok(Value::Float(f(v)))
    };
    match func {
        Func::Sin => math1(args, f64::sin, true),
        Func::Cos => math1(args, f64::cos, true),
        Func::Tan => math1(args, f64::tan, true),
        Func::Atan => math1(args, f64::atan, false),
        Func::Atan2 => {
            if args.len() != 2 {
                return Err(Raised);
            }
            Ok(Value::Float(to_float(args[0])?.atan2(to_float(args[1])?)))
        }
        Func::Sqrt => {
            let v = absolute(one(args)?)?;
            let f = to_float(v)?;
            if f < 0.0 {
                return Err(Raised);
            }
            Ok(Value::Float(f.sqrt()))
        }
        Func::Abs => absolute(one(args)?),
        Func::Floor | Func::Ceil => {
            let v = one(args)?;
            if let Some(i) = as_int(v) {
                return Ok(Value::Int(i));
            }
            let f = to_float(v)?;
            float_to_int(if matches!(func, Func::Floor) {
                f.floor()
            } else {
                f.ceil()
            })
        }
        Func::Exp => {
            let f = to_float(one(args)?)?;
            let result = f.exp();
            if result.is_infinite() && f.is_finite() {
                return Err(Raised);
            }
            Ok(Value::Float(result))
        }
        Func::Log => {
            let v = absolute(one(args)?)?;
            let f = to_float(v)? + 1e-9;
            if f <= 0.0 {
                return Err(Raised);
            }
            Ok(Value::Float(f.ln()))
        }
        Func::Hypot => {
            let mut floats = Vec::with_capacity(args.len());
            for a in args {
                floats.push(to_float(*a)?);
            }
            Ok(Value::Float(match floats.as_slice() {
                [] => 0.0,
                [a] => a.abs(),
                [a, b] => a.hypot(*b),
                many => {
                    if many.iter().any(|f| f.is_infinite()) {
                        f64::INFINITY
                    } else if many.iter().any(|f| f.is_nan()) {
                        f64::NAN
                    } else {
                        let max = many.iter().fold(0.0f64, |m, f| m.max(f.abs()));
                        if max == 0.0 {
                            0.0
                        } else {
                            max * many
                                .iter()
                                .map(|f| (f / max) * (f / max))
                                .sum::<f64>()
                                .sqrt()
                        }
                    }
                }
            }))
        }
        Func::Min | Func::Max => {
            if args.len() < 2 {
                return Err(Raised);
            }
            let want = if matches!(func, Func::Min) {
                "Lt"
            } else {
                "Gt"
            };
            let mut best = args[0];
            for item in &args[1..] {
                if cmp_real(want, *item, best)? {
                    best = *item;
                }
            }
            // Comparing a single item with itself would also raise for a function; do it for parity.
            Ok(best)
        }
        Func::Round => match args.len() {
            1 => {
                let v = args[0];
                if let Some(i) = as_int(v) {
                    return Ok(Value::Int(i));
                }
                let f = to_float(v)?;
                float_to_int(round_half_even(f))
            }
            2 => {
                let digits = as_int(args[1]).ok_or(Raised)?;
                match args[0] {
                    v if as_int(v).is_some() => {
                        let i = as_int(v).ok_or(Raised)?;
                        if digits >= 0 {
                            return Ok(Value::Int(i));
                        }
                        let k = (-digits).min(38) as u32;
                        let pow = 10i128.pow(k);
                        let (q, r) = (i.div_euclid(pow), i.rem_euclid(pow));
                        let half = pow / 2;
                        let rounded = if r > half || (r == half && q % 2 != 0) {
                            q + 1
                        } else {
                            q
                        };
                        Ok(Value::Int(rounded.saturating_mul(pow)))
                    }
                    Value::Float(f) => {
                        if !f.is_finite() || digits > 323 {
                            return Ok(Value::Float(f));
                        }
                        if digits < -308 {
                            return Ok(Value::Float(0.0f64.copysign(f)));
                        }
                        if digits >= 0 {
                            let text = format!("{:.*}", digits as usize, f);
                            return Ok(Value::Float(text.parse::<f64>().unwrap_or(f)));
                        }
                        let scale = 10f64.powi((-digits) as i32);
                        Ok(Value::Float(round_half_even(f / scale) * scale))
                    }
                    _ => Err(Raised),
                }
            }
            _ => Err(Raised),
        },
        Func::Noise => {
            if args.len() != 2 {
                return Err(Raised);
            }
            let (a, b) = (to_int_arg(args[0])?, to_int_arg(args[1])?);
            Ok(Value::Float(noise_hash(a, b)))
        }
        Func::Int => match args.len() {
            0 => Ok(Value::Int(0)),
            1 => Ok(Value::Int(to_int_arg(args[0])?)),
            _ => Err(Raised),
        },
        Func::Pow => {
            if args.len() != 2 {
                return Err(Raised);
            }
            binop("Pow", absolute(args[0])?, args[1])
        }
    }
}

/// `int(v)` for a real number.
fn to_int_arg(v: Value) -> Result<i128, Raised> {
    if let Some(i) = as_int(v) {
        return Ok(i);
    }
    let f = to_float(v)?;
    if !f.is_finite() {
        return Err(Raised);
    }
    Ok(f as i128)
}

/// `patterns._hash` for any int (only the low 32 bits of the products matter).
fn noise_hash(x: i128, y: i128) -> f64 {
    let low = |v: i128| (v.rem_euclid(1i128 << 32)) as u64;
    let value = (low(x)
        .wrapping_mul(374_761_393)
        .wrapping_add(low(y).wrapping_mul(668_265_263))) as u32;
    let value = (value ^ (value >> 13)).wrapping_mul(1_274_126_177);
    (value ^ (value >> 16)) as f64 / 4_294_967_295.0
}

fn absolute(v: Value) -> EvalResult {
    match v {
        Value::Bool(b) => Ok(Value::Int(b as i128)),
        Value::Int(i) => Ok(int_or_float(i.checked_abs(), (i as f64).abs())),
        Value::Float(f) => Ok(Value::Float(f.abs())),
        Value::Complex(re, im) => Ok(Value::Float(re.hypot(im))),
        Value::Func(_) => Err(Raised),
    }
}

fn eval(node: &Node, env: &Env) -> EvalResult {
    match node {
        Node::Number(v) => Ok(*v),
        Node::Name(name) => Ok(match name.as_str() {
            "x" => Value::Int(env.x),
            "y" => Value::Int(env.y),
            "W" => Value::Int(env.w),
            "H" => Value::Int(env.h),
            "u" => Value::Float(env.u),
            "v" => Value::Float(env.v),
            "r" => Value::Float(env.r),
            "a" => Value::Float(env.a),
            "pi" => Value::Float(std::f64::consts::PI),
            "tau" => Value::Float(std::f64::consts::TAU),
            "e" => Value::Float(std::f64::consts::E),
            other => Value::Func(func_by_name(other).ok_or(Raised)?),
        }),
        Node::UnaryOp { op, operand } => unary(op, eval(operand, env)?),
        Node::BinOp { op, left, right } => {
            let a = eval(left, env)?;
            let b = eval(right, env)?;
            binop(op, a, b)
        }
        Node::BoolOp { op, values } => {
            let mut last = Value::Bool(false);
            for (i, value) in values.iter().enumerate() {
                last = eval(value, env)?;
                let stop = if *op == "And" {
                    !truthy(last)
                } else {
                    truthy(last)
                };
                if stop || i + 1 == values.len() {
                    return Ok(last);
                }
            }
            Ok(last)
        }
        Node::Compare {
            left,
            ops,
            comparators,
        } => {
            let mut a = eval(left, env)?;
            for (op, comparator) in ops.iter().zip(comparators) {
                let b = eval(comparator, env)?;
                if !compare(op, a, b)? {
                    return Ok(Value::Bool(false));
                }
                a = b;
            }
            Ok(Value::Bool(true))
        }
        Node::IfExp { test, body, orelse } => {
            if truthy(eval(test, env)?) {
                eval(body, env)
            } else {
                eval(orelse, env)
            }
        }
        Node::Call { func, args, .. } => {
            let Node::Name(name) = &**func else {
                return Err(Raised);
            };
            let f = func_by_name(name).ok_or(Raised)?;
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(eval(arg, env)?);
            }
            call(f, &values)
        }
        _ => Err(Raised),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> String {
        match compile_formula(src) {
            Ok(_) => "OK".into(),
            Err(e) => e.0,
        }
    }

    #[test]
    fn formulas_cant_escape_the_whitelist() {
        for attack in [
            "__import__('os').system('true')",
            "().__class__",
            "open('/etc/passwd')",
            "x.real",
            "[i for i in (1,)]",
            "(lambda: 1)()",
            "eval('1')",
            "getattr(x,'real')",
            "'text'",
            "x if y else __builtins__",
            "f'{x}'",
        ] {
            assert!(compile_formula(attack).is_err(), "{attack}");
        }
    }

    #[test]
    fn formula_math_errors_just_paint_nothing() {
        assert!(!compile_formula("1/(x-5)>0").unwrap().test(5, 0, 10, 10));
        assert!(compile_formula("x>y").unwrap().test(5, 1, 10, 10));
    }

    #[test]
    fn the_length_limit_counts_characters() {
        assert_eq!(err(&"1+".repeat(201)), "Formula is over 400 characters.");
        assert_eq!(err(&format!("{}1", "1+".repeat(199))), "OK");
    }

    #[test]
    fn messages_match_python() {
        let mut wrong = Vec::new();
        for (src, expected) in REF_ERRORS {
            let got = err(src);
            if &got != expected {
                wrong.push(format!("{src:?}: got {got:?}, Python says {expected:?}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} differ:\n{}",
            wrong.len(),
            REF_ERRORS.len(),
            wrong.join("\n")
        );
    }

    /// Random token soup and random (valid) arithmetic must never panic, whatever Python would say.
    #[test]
    fn random_input_never_panics() {
        let atoms = [
            "x",
            "y",
            "u",
            "v",
            "r",
            "a",
            "W",
            "H",
            "pi",
            "e",
            "sin",
            "abs",
            "min",
            "max",
            "round",
            "noise",
            "int",
            "pow",
            "hypot",
            "floor",
            "exp",
            "log",
            "sqrt",
            "1",
            "0",
            "2.5",
            "1e308",
            "1e-320",
            "170141183460469231731687303715884105727",
            "99999999999999999999999999999999999999",
            "True",
            "(",
            ")",
            "(",
            ")",
            ",",
            "+",
            "-",
            "*",
            "/",
            "//",
            "%",
            "**",
            "|",
            "&",
            "^",
            "<",
            ">",
            "==",
            "!=",
            "<=",
            "and",
            "or",
            "not",
            "if",
            "else",
            "lambda",
            "[",
            "]",
            "{",
            "}",
            ":",
            ".",
            "'",
            "\n",
            " ",
            "\\",
            "$",
            "#",
            "é",
            "\u{a0}",
        ];
        let mut seed: u64 = 12345;
        let mut next = |n: usize| -> usize {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        let mut compiled = 0;
        for _ in 0..6000 {
            let src: String = (0..1 + next(14))
                .map(|_| format!("{}{}", atoms[next(atoms.len())], [" ", "", " "][next(3)]))
                .collect();
            if let Ok(f) = compile_formula(&src) {
                compiled += 1;
                for (i, j) in [(0, 0), (5, 3), (23, 17), (-4, 9), (1_000_000, -7)] {
                    f.test(i, j, 24, 18);
                    f.test(i, j, 0, 0);
                    f.test(i, j, 1, 1);
                }
            }
        }
        assert!(compiled > 100, "only {compiled} random formulas compiled");
        // Deeply nested input is fine and does not overflow the stack.
        let deep = format!("{}1{}", "(".repeat(190), ")".repeat(190));
        assert!(compile_formula(&deep).unwrap().test(0, 0, 1, 1));
        assert_eq!(
            err(&"-".repeat(300)),
            "Formula has a syntax error: invalid syntax"
        );
        let unary = format!("{}x", "-".repeat(300));
        assert!(compile_formula(&unary).is_ok());
        assert_eq!(
            err(&format!("{}1{}", "(".repeat(201), ")".repeat(201))),
            "Formula is over 400 characters."
        );
        assert_eq!(
            err(&format!("{}1{}", "(".repeat(199), ")".repeat(199))),
            "OK"
        );
    }

    #[test]
    fn evaluation_matches_python() {
        for (src, hash) in REF_EVAL {
            let f = compile_formula(src).unwrap_or_else(|e| panic!("{src}: {e}"));
            let mut h: u64 = 0xcbf29ce484222325;
            for j in 0..18 {
                for i in 0..24 {
                    h ^= f.test(i, j, 24, 18) as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
            assert_eq!(h, *hash, "{src}");
        }
    }

    #[rustfmt::skip]
    const REF_ERRORS: &[(&str, &str)] = &[
        ("", "Formula has a syntax error: invalid syntax"),
        (" ", "Formula has a syntax error: unexpected indent"),
        (" x", "Formula has a syntax error: unexpected indent"),
        ("x ", "OK"),
        ("\tx", "Formula has a syntax error: unexpected indent"),
        ("x\n", "OK"),
        ("\nx", "OK"),
        ("x\ny", "Formula has a syntax error: invalid syntax"),
        ("(x", "Formula has a syntax error: '(' was never closed"),
        ("x)", "Formula has a syntax error: unmatched ')'"),
        ("x y", "Formula has a syntax error: invalid syntax"),
        ("1 2", "Formula has a syntax error: invalid syntax"),
        ("x = 1", "Formula has a syntax error: invalid syntax"),
        ("x if y", "Formula has a syntax error: expected 'else' after 'if' expression"),
        ("x if y else", "Formula has a syntax error: invalid syntax"),
        ("3 +* 4", "Formula has a syntax error: invalid syntax"),
        ("1x", "Formula has a syntax error: invalid decimal literal"),
        ("'abc", "Formula has a syntax error: unterminated string literal (detected at line 1)"),
        ("x $ y", "Formula has a syntax error: invalid syntax"),
        ("\\", "Formula has a syntax error: unexpected character after line continuation character"),
        ("07", "Formula has a syntax error: leading zeros in decimal integer literals are not permitted; use an 0o prefix for octal integers"),
        ("1_", "Formula has a syntax error: invalid decimal literal"),
        ("x;y", "Formula has a syntax error: invalid syntax"),
        ("foo(", "Formula has a syntax error: '(' was never closed"),
        ("x ? y : z", "Formula has a syntax error: invalid syntax"),
        ("x +", "Formula has a syntax error: invalid syntax"),
        ("x **", "Formula has a syntax error: invalid syntax"),
        ("1e", "Formula has a syntax error: invalid decimal literal"),
        ("0x", "Formula has a syntax error: invalid hexadecimal literal"),
        ("0b2", "Formula has a syntax error: invalid digit '2' in binary literal"),
        ("1.2.3", "Formula has a syntax error: invalid syntax"),
        ("..", "Formula has a syntax error: invalid syntax"),
        ("x..y", "Formula has a syntax error: invalid syntax"),
        ("a b c", "Formula has a syntax error: invalid syntax"),
        ("(x y)", "Formula has a syntax error: invalid syntax. Perhaps you forgot a comma?"),
        ("[x y]", "Formula has a syntax error: invalid syntax. Perhaps you forgot a comma?"),
        ("f(x y)", "Formula has a syntax error: invalid syntax. Perhaps you forgot a comma?"),
        ("x +\n1", "Formula has a syntax error: invalid syntax"),
        ("(x +\n1)", "OK"),
        ("x # c", "OK"),
        ("# only", "Formula has a syntax error: invalid syntax"),
        ("x\\\n+1", "OK"),
        ("not", "Formula has a syntax error: invalid syntax"),
        ("x and", "Formula has a syntax error: invalid syntax"),
        ("x or or y", "Formula has a syntax error: invalid syntax"),
        ("lambda: 1", "Lambda isn't allowed in a formula."),
        ("lambda x: x", "Lambda isn't allowed in a formula."),
        ("yield 1", "Formula has a syntax error: invalid syntax"),
        ("await x", "Await isn't allowed in a formula."),
        ("x := 1", "Formula has a syntax error: invalid syntax"),
        ("(x := 1)", "NamedExpr isn't allowed in a formula."),
        ("x, y", "Tuple isn't allowed in a formula."),
        ("(x, y)", "Tuple isn't allowed in a formula."),
        ("[x]", "List isn't allowed in a formula."),
        ("{x}", "Set isn't allowed in a formula."),
        ("{x: y}", "Dict isn't allowed in a formula."),
        ("{}", "Dict isn't allowed in a formula."),
        ("()", "Tuple isn't allowed in a formula."),
        ("[]", "List isn't allowed in a formula."),
        ("x[0]", "Subscript isn't allowed in a formula."),
        ("x[0:1]", "Subscript isn't allowed in a formula."),
        ("x.real", "Attribute isn't allowed in a formula."),
        ("f'{x}'", "JoinedStr isn't allowed in a formula."),
        ("'a'", "Only numbers are allowed in a formula."),
        ("b'a'", "Only numbers are allowed in a formula."),
        ("None", "Only numbers are allowed in a formula."),
        ("True", "OK"),
        ("False", "OK"),
        ("...", "Only numbers are allowed in a formula."),
        ("1j", "Only numbers are allowed in a formula."),
        ("1+2j", "Only numbers are allowed in a formula."),
        ("x if y else z", "Unknown name 'z' in formula."),
        ("x < y < z", "Unknown name 'z' in formula."),
        ("x in y", "In isn't allowed in a formula."),
        ("x not in y", "NotIn isn't allowed in a formula."),
        ("x is y", "Is isn't allowed in a formula."),
        ("x is not y", "IsNot isn't allowed in a formula."),
        ("x << 1", "LShift isn't allowed in a formula."),
        ("x >> 1", "RShift isn't allowed in a formula."),
        ("~x", "Invert isn't allowed in a formula."),
        ("x @ y", "MatMult isn't allowed in a formula."),
        ("-x", "OK"),
        ("+x", "OK"),
        ("not x", "OK"),
        ("x ** -y", "OK"),
        ("x // y", "OK"),
        ("x % y", "OK"),
        ("x ^ y", "OK"),
        ("x & y", "OK"),
        ("x | y", "OK"),
        ("x and y or z", "Unknown name 'z' in formula."),
        ("sin", "OK"),
        ("sin(x)", "OK"),
        ("sin(x, y)", "OK"),
        ("sin(x=1)", "keyword isn't allowed in a formula."),
        ("sin(*x)", "Starred isn't allowed in a formula."),
        ("sin(**x)", "keyword isn't allowed in a formula."),
        ("sin(x for x in y)", "GeneratorExp isn't allowed in a formula."),
        ("(sin)(x)", "OK"),
        ("sin(x)(y)", "Only the built-in functions can be called."),
        ("pi", "OK"),
        ("tau", "OK"),
        ("e", "OK"),
        ("W", "OK"),
        ("H", "OK"),
        ("u", "OK"),
        ("v", "OK"),
        ("r", "OK"),
        ("a", "OK"),
        ("z", "Unknown name 'z' in formula."),
        ("__import__('os')", "Only the built-in functions can be called."),
        ("getattr(x,'real')", "Only the built-in functions can be called."),
        ("x if y else __builtins__", "Unknown name '__builtins__' in formula."),
        ("[i for i in (1,)]", "ListComp isn't allowed in a formula."),
        ("(lambda: 1)()", "Only the built-in functions can be called."),
        ("eval('1')", "Only the built-in functions can be called."),
        ("open('/etc/passwd')", "Only the built-in functions can be called."),
        ("().__class__", "Attribute isn't allowed in a formula."),
        ("__import__('os').system('true')", "Only the built-in functions can be called."),
        ("x.real", "Attribute isn't allowed in a formula."),
        ("1_000", "OK"),
        ("0x1F", "OK"),
        ("0b101", "OK"),
        ("0o17", "OK"),
        ("1e3", "OK"),
        ("1.5", "OK"),
        (".5", "OK"),
        ("5.", "OK"),
        ("1E-3", "OK"),
        ("00", "OK"),
        ("0", "OK"),
        ("0.0", "OK"),
        ("1__0", "Formula has a syntax error: invalid decimal literal"),
        ("9999999999999999999999999999999999999999", "OK"),
        ("1e400", "OK"),
        ("\u{e9}", "Unknown name '\u{e9}' in formula."),
        ("x\u{e9}", "Unknown name 'x\u{e9}' in formula."),
        ("x\u{2022}", "Formula has a syntax error: invalid character '\u{2022}' (U+2022)"),
        ("x\u{0}", "Formula has a syntax error: source code string cannot contain null bytes"),
        ("\u{20ac}", "Formula has a syntax error: invalid character '\u{20ac}' (U+20AC)"),
        ("x ! y", "Formula has a syntax error: invalid syntax"),
        ("x !", "Formula has a syntax error: invalid syntax"),
        ("!x", "Formula has a syntax error: invalid syntax"),
        ("x != y", "OK"),
        ("x <> y", "Formula has a syntax error: invalid syntax"),
        ("x =", "Formula has a syntax error: invalid syntax"),
        ("= x", "Formula has a syntax error: invalid syntax"),
        ("x == ", "Formula has a syntax error: invalid syntax"),
        ("x ==", "Formula has a syntax error: invalid syntax"),
        ("(", "Formula has a syntax error: '(' was never closed"),
        (")", "Formula has a syntax error: unmatched ')'"),
        ("((x)", "Formula has a syntax error: '(' was never closed"),
        ("(x))", "Formula has a syntax error: unmatched ')'"),
        ("[", "Formula has a syntax error: '[' was never closed"),
        ("]", "Formula has a syntax error: unmatched ']'"),
        ("{", "Formula has a syntax error: '{' was never closed"),
        ("}", "Formula has a syntax error: unmatched '}'"),
        ("x]", "Formula has a syntax error: unmatched ']'"),
        ("x}", "Formula has a syntax error: unmatched '}'"),
        ("(x]", "Formula has a syntax error: closing parenthesis ']' does not match opening parenthesis '('"),
        ("[x)", "Formula has a syntax error: closing parenthesis ')' does not match opening parenthesis '['"),
        ("x,", "Tuple isn't allowed in a formula."),
        (",", "Formula has a syntax error: invalid syntax"),
        ("x , , y", "Formula has a syntax error: invalid syntax"),
        ("*x", "Formula has a syntax error: invalid syntax"),
        ("*x, y", "Formula has a syntax error: invalid syntax"),
        ("x if", "Formula has a syntax error: invalid syntax"),
        ("if x", "Formula has a syntax error: invalid syntax"),
        ("else", "Formula has a syntax error: invalid syntax"),
        ("x else y", "Formula has a syntax error: invalid syntax"),
        ("x if y else z if w else v", "Unknown name 'w' in formula."),
        ("f(,)", "Formula has a syntax error: invalid syntax"),
        ("f(x,)", "Only the built-in functions can be called."),
        ("f(x,,)", "Formula has a syntax error: invalid syntax"),
        ("sin(x))", "Formula has a syntax error: unmatched ')'"),
        ("sin((x)", "Formula has a syntax error: '(' was never closed"),
        ("1 if 2 else 3 if 4 else 5", "OK"),
        ("'''a", "Formula has a syntax error: unterminated triple-quoted string literal (detected at line 1)"),
        ("\"a", "Formula has a syntax error: unterminated string literal (detected at line 1)"),
        ("'a\\'", "Formula has a syntax error: unterminated string literal (detected at line 1)"),
        ("x\ty", "Formula has a syntax error: invalid syntax"),
        ("x\u{a0}y", "Formula has a syntax error: invalid non-printable character U+00A0"),
        ("x\r\ny", "Formula has a syntax error: invalid syntax"),
        ("\"\"\"", "Formula has a syntax error: unterminated triple-quoted string literal (detected at line 1)"),
        ("x \\ y", "Formula has a syntax error: unexpected character after line continuation character"),
        ("x\\", "Formula has a syntax error: unexpected character after line continuation character"),
        ("a.b.c", "Attribute isn't allowed in a formula."),
        ("a . b", "Attribute isn't allowed in a formula."),
        ("1.real", "Formula has a syntax error: invalid decimal literal"),
        ("1 .real", "Attribute isn't allowed in a formula."),
        ("1..real", "Attribute isn't allowed in a formula."),
        ("x[", "Formula has a syntax error: '[' was never closed"),
        ("x[]", "Formula has a syntax error: invalid syntax"),
        ("x[1,]", "Subscript isn't allowed in a formula."),
        ("x[::]", "Subscript isn't allowed in a formula."),
        ("x[:,1]", "Subscript isn't allowed in a formula."),
        ("xxxxx+++", "Formula has a syntax error: invalid syntax"),
        ("1 if", "Formula has a syntax error: invalid syntax"),
        ("x if y else", "Formula has a syntax error: invalid syntax"),
        ("(1 +", "Formula has a syntax error: '(' was never closed"),
        ("((1)", "Formula has a syntax error: '(' was never closed"),
        ("sin(x", "Formula has a syntax error: '(' was never closed"),
        ("[1,", "Formula has a syntax error: '[' was never closed"),
        ("{1:", "Formula has a syntax error: '{' was never closed"),
        ("(x y", "Formula has a syntax error: '(' was never closed"),
        ("(x +\ny", "Formula has a syntax error: '(' was never closed"),
        ("x\n y", "Formula has a syntax error: unexpected indent"),
        ("x +\n", "Formula has a syntax error: invalid syntax"),
        ("1 +\n 2", "Formula has a syntax error: invalid syntax"),
        ("'a' 'b'", "Only numbers are allowed in a formula."),
        ("f'{x}' 'b'", "JoinedStr isn't allowed in a formula."),
        ("x:=1", "Formula has a syntax error: invalid syntax"),
        ("(x:=1)", "NamedExpr isn't allowed in a formula."),
        ("(*x)", "Formula has a syntax error: cannot use starred expression here"),
        ("(*x,)", "Tuple isn't allowed in a formula."),
        ("{**x}", "Dict isn't allowed in a formula."),
        ("{1, 2}", "Set isn't allowed in a formula."),
        ("{x: 1 for x in y}", "DictComp isn't allowed in a formula."),
        ("x[1:2:3]", "Subscript isn't allowed in a formula."),
        ("lambda x=1, *a, **k: x", "Lambda isn't allowed in a formula."),
        ("sin(x)(y)(z)", "Only the built-in functions can be called."),
        ("sin(1 for x in y)", "GeneratorExp isn't allowed in a formula."),
        ("not not x", "OK"),
        ("1 < 2 > 3 == 4", "OK"),
        ("-x ** 2", "OK"),
        ("x ** -1", "OK"),
        ("2 ** 3 ** 2", "OK"),
        ("x ^ y | 1 & 2", "OK"),
        ("x // 0", "OK"),
        ("0 if x else 1 if y else 2", "OK"),
        ("x or y and not z", "Unknown name 'z' in formula."),
        ("True + True", "OK"),
        ("None + 1", "Only numbers are allowed in a formula."),
        ("...", "Only numbers are allowed in a formula."),
        ("1if x else 2", "OK"),
        ("1_0", "OK"),
        ("0_7", "Formula has a syntax error: leading zeros in decimal integer literals are not permitted; use an 0o prefix for octal integers"),
        ("07.5", "OK"),
        ("1e5j", "Only numbers are allowed in a formula."),
        ("0xFFg", "Formula has a syntax error: invalid hexadecimal literal"),
        ("0o78", "Formula has a syntax error: invalid digit '8' in octal literal"),
        ("0b1_0", "OK"),
        ("1.e3", "OK"),
        ("5.real", "Formula has a syntax error: invalid decimal literal"),
        ("x@y", "MatMult isn't allowed in a formula."),
        ("x<>y", "Formula has a syntax error: invalid syntax"),
        ("x$", "Formula has a syntax error: invalid syntax"),
        ("$x", "Formula has a syntax error: invalid syntax"),
        ("x?y:z", "Formula has a syntax error: invalid syntax"),
        ("\u{e9}+1", "Unknown name '\u{e9}' in formula."),
        ("x\u{2028}y", "Formula has a syntax error: invalid non-printable character U+2028"),
        ("\u{bd}", "Formula has a syntax error: invalid character '\u{bd}' (U+00BD)"),
        ("(", "Formula has a syntax error: '(' was never closed"),
        ("((((", "Formula has a syntax error: '(' was never closed"),
        ("((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((()))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))", "Formula is over 400 characters."),
        ("[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]", "List isn't allowed in a formula."),
        ("x if y else z if", "Formula has a syntax error: invalid syntax"),
        ("pi(1)", "Only the built-in functions can be called."),
        ("sin()", "OK"),
        ("abs(1, 2)", "OK"),
        ("round(1.5, 'a')", "Only numbers are allowed in a formula."),
    ];
    #[rustfmt::skip]
    const REF_EVAL: &[(&str, u64)] = &[
        ("(x*7+y*3)%5<2", 0xb189b618e5f18804),
        ("x%3==0", 0x4500a3d4237ce535),
        ("x>y", 0x566af9d5219b745e),
        ("1/(x-5)>0", 0xf475c3161ee77879),
        ("sin(x/3)+cos(y/2)>0", 0x50d6d0f55ccfd0e0),
        ("sqrt(x*x+y*y)<10", 0xcb76f581d61da995),
        ("r<8", 0x0eba4606352c09dc),
        ("r%4<2", 0x36afaed5405d45c4),
        ("a>0", 0x1dd1f0f941fc3291),
        ("u>v", 0x211d7bdf81bb0c8e),
        ("u*v>0.25", 0x71c02d19a9278b13),
        ("x^y&1", 0x0225456eeca9437b),
        ("(x|y)&4", 0x745536a9ebb6e65d),
        ("x**2+y**2<100", 0xcb76f581d61da995),
        ("x**-1>0.1", 0x79bbc40dedca08c5),
        ("(x-12)**0.5>1", 0x337a2c699eefa169),
        ("(x-12)**0.5", 0xc02d48fc3932bd85),
        ("y-x**0.5>0", 0x68fbe5fb0fe540e1),
        ("-8**(1/3)", 0x93dd73ad029aa695),
        ("(-8)**(1/3)", 0x93dd73ad029aa695),
        ("abs((-8)**(1/3))>1.5", 0x93dd73ad029aa695),
        ("noise(x,y)<0.5", 0xf58b754f53e1f89a),
        ("noise(x/2,y/2)<0.3", 0x9720f5d630f6c6ed),
        ("noise(-x,y)>0.5", 0xec1657bd474ec305),
        ("int(u*4)%2==0", 0x67970b3d9a9d1945),
        ("round(u*3)==1", 0x523fcb2c01805675),
        ("round(x/2)%2", 0x4142b4286fb2a185),
        ("round(x*0.35, 1)>3", 0x8e16bf529e79a985),
        ("round(x/7, 0)>1", 0x37593b4837f59845),
        ("round(x, -1)==10", 0x1e40af0bf5ed6785),
        ("floor(x/5)+ceil(y/5)>4", 0x6a0357bb67cc86d0),
        ("min(x,y)>6", 0xe7fba1ad1162eac2),
        ("max(x,y)<10", 0xb3ec1fef7faa29b9),
        ("min(x,y,5)>3", 0xa4f37728d46ebb3d),
        ("max(u,v,0.5)<0.9", 0xc503f29bd5eb2de5),
        ("hypot(x-12,y-9)<6", 0x5f30505d22d4f5d0),
        ("hypot(x,y,5)>20", 0xba0593d130c7e46d),
        ("atan2(y-9,x-12)>0", 0x1dd1f0f941fc3291),
        ("atan(x-y)>0.5", 0x566af9d5219b745e),
        ("exp(x/4)>20", 0xc9cfb82703a2f97d),
        ("exp(x*100)>1", 0x61d461ba20335885),
        ("log(x)>1", 0x8f668fa3dd244945),
        ("log(x-5)>0", 0xc4253223b3de2745),
        ("pow(x-12,2)+pow(y-9,2)<30", 0xe31757701b5186f0),
        ("pow(x-12, 0.5)>1", 0xfc61678a2c86e085),
        ("x//5%2==0", 0xd338bd74b036259d),
        ("-x//5>-3", 0xa8d5dc2ddd102945),
        ("x%-5>-3", 0x17ad86de3b549641),
        ("(x-12)%5<2", 0xa01b6b53391d5bb9),
        ("(x-12)//5==0", 0x2767c1828cf1ed05),
        ("x%0>1", 0xe120542310fbb4e5),
        ("x//0>1", 0xe120542310fbb4e5),
        ("x and y", 0x674a5326b1f443ec),
        ("x or y", 0x75bebdcf3a254474),
        ("x and y>3", 0x6bcbc8c09a14a945),
        ("(x or y)>5", 0xa434a5c12558df01),
        ("not x", 0x41c070bb81019385),
        ("not (x%2)", 0x7bbdd8aca66de005),
        ("x if y>5 else y", 0x5ed0b9f0ab66209d),
        ("1 if x>y else 0", 0x566af9d5219b745e),
        ("x>5 and y>5 or x<3", 0x9cca83980a9a17c5),
        ("0 or 0.0", 0xe120542310fbb4e5),
        ("True", 0x93dd73ad029aa695),
        ("False", 0xe120542310fbb4e5),
        ("True+True==2", 0x93dd73ad029aa695),
        ("True and 7", 0x93dd73ad029aa695),
        ("x<y<10", 0xef807affe81597bc),
        ("0<x<5<y", 0xe1e5df123cc7d475),
        ("3<x>2", 0x3aef2f2ff4279b0d),
        ("x==y==5", 0x51fced537738e1fa),
        ("x!=y!=x", 0x8781bf26721e37bb),
        ("sin", 0x93dd73ad029aa695),
        ("sin==sin", 0x93dd73ad029aa695),
        ("sin==cos", 0xe120542310fbb4e5),
        ("sin<cos", 0xe120542310fbb4e5),
        ("sin+1", 0xe120542310fbb4e5),
        ("abs(sin)", 0xe120542310fbb4e5),
        ("min(1)", 0xe120542310fbb4e5),
        ("min()", 0xe120542310fbb4e5),
        ("max(2)", 0xe120542310fbb4e5),
        ("abs(1,2)", 0xe120542310fbb4e5),
        ("int()", 0xe120542310fbb4e5),
        ("int(x/2)%2", 0x5bcc2a3687ec445d),
        ("int(u)", 0xb43c840c39b60ac5),
        ("int(-u*3)==-1", 0x222c228846254575),
        ("int(x,2)", 0xe120542310fbb4e5),
        ("pi>x/4", 0xb8e41716d8173a05),
        ("tau>x/4", 0x93dd73ad029aa695),
        ("e>x/8", 0x2cadf6bcefc54f21),
        ("sin(pi*x/4)>0", 0xf65da7dd5b181aed),
        ("cos(tau*u)>0", 0xc19211765136d0bd),
        ("W>x", 0x93dd73ad029aa695),
        ("H>y*2", 0x0b358dc0f90aba3d),
        ("x*u>5", 0x37593b4837f59845),
        ("(x*0.1+y*0.1)%1<0.5", 0x17798e8b3fe7722a),
        ("(x+0.5)//1==x", 0x93dd73ad029aa695),
        ("2**x>1000", 0x4d3acb2ddb9dd0b1),
        ("2**100>x", 0x93dd73ad029aa695),
        ("10**30>x", 0x93dd73ad029aa695),
        ("x**x>1e10", 0x37593b4837f59845),
        ("x**30>1e30", 0x37593b4837f59845),
        ("0**x", 0x41c070bb81019385),
        ("0**-x", 0x41c070bb81019385),
        ("0.0**-x", 0x41c070bb81019385),
        ("0**0", 0x93dd73ad029aa695),
        ("x**0", 0x93dd73ad029aa695),
        ("(-1)**x>0", 0x7bbdd8aca66de005),
        ("(-2)**(x/4)>1", 0x693ed5cd5ca18aa5),
        ("x/3==int(x/3)", 0x4500a3d4237ce535),
        ("x/0.5>10", 0xf475c3161ee77879),
        ("1e308*10>x", 0x93dd73ad029aa695),
        ("1e308*10-1e308*10>0", 0xe120542310fbb4e5),
        ("x*1e308*1e308>1", 0xd043cfd5dbe70285),
        ("(x-12)*1e-320>0", 0x4767763132fca905),
        ("sqrt(-x)>1", 0xfcf309eb3f3e60c1),
        ("sqrt(x-12)>1", 0xfc61678a2c86e085),
        ("tan(x)>1", 0xc70bd41d5afdf319),
        ("tan(1e308)>1", 0xe120542310fbb4e5),
        ("sin(1e308*10)>0", 0xe120542310fbb4e5),
        ("atan(1e308*10)>1", 0x93dd73ad029aa695),
        ("exp(-1e308*10)<1", 0x93dd73ad029aa695),
        ("floor(1e308*10)>1", 0xe120542310fbb4e5),
        ("ceil(x/0.7)==x", 0x41c070bb81019385),
        ("floor(-x/2)<-3", 0x593a6aec476c2cc5),
        ("x%7<3 or y%5==0", 0xd998df27af99ef6d),
        ("(x%4<2)==(y%4<2)", 0xb10be7e5a9e8641d),
        ("x&y", 0xad2854aceb57394e),
        ("x|y|1", 0x93dd73ad029aa695),
        ("x^1", 0xcaf124c9c0c7ca45),
        ("True&x", 0xbe586549a2b00d45),
        ("True|False", 0x93dd73ad029aa695),
        ("1&2.0", 0xe120542310fbb4e5),
        ("x|1.5", 0xe120542310fbb4e5),
        ("x&-1", 0xd043cfd5dbe70285),
        ("-x&3", 0x757f79db5e75ad05),
        ("-x|3>1", 0x41c070bb81019385),
        ("x**2%7<3", 0x56626e2dbc0cbc05),
        ("x*y%9<4", 0x74d14fc408268d45),
        ("(x*x+y*y)**0.5%5<2", 0x3f9799cb3ae410a8),
        ("abs(x-12)+abs(y-9)<8", 0xf5f0e434c0d04984),
        ("abs(-x)>5", 0xf475c3161ee77879),
        ("abs(u-0.5)<0.2", 0xaae051fa4d7751d1),
    ];
}
