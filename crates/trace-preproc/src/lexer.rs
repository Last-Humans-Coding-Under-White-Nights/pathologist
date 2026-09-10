use crate::Language;
use rustc_hash::FxHashSet;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Identifier(String),
    Number(String),
    /// A string literal spelled exactly as written — encoding prefix,
    /// quotes and, for a C++11 raw string, delimiters and embedded newlines
    /// (`"a"`, `L"a"`, `u8R"~(a "b")~"`). Carrying the spelling means the
    /// literal is re-emitted verbatim; the few consumers that need the body
    /// (`#include "…"`, `#if 'c'`) strip the delimiters themselves.
    String(String),
    /// A character literal spelled as written, prefix and quotes included
    /// (`'a'`, `L'\n'`).
    Char(String),
    /// A punctuator, spelled from the fixed set the lexer recognizes — so
    /// `&'static str` rather than an owned `String`. Punctuators are ~48% of
    /// the tokens in a C++ corpus and 94% of those are one character, so an
    /// owned spelling meant well over a million allocations per translation
    /// unit's worth of lexing, every one of them a copy of a string literal.
    /// The invariant the type states is that a punctuator's spelling is
    /// always a `'static` literal — the lexer's punctuator tables, or the
    /// fixed spellings `expand_gmock_method` synthesizes — never computed.
    Punct(&'static str),
    Hash, // #
    Newline,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub line: u32,
    pub col: u32,
    /// Macros that must not expand this token again (C11 6.10.3.4 hide set).
    pub(crate) hidden: Option<Arc<FxHashSet<String>>>,
    /// Whether this token touched the previous one in the token stream:
    /// no whitespace, comment or newline between them. `\`-newline is
    /// deleted in translation phase 2 (C11 5.1.1.2p1), before tokens are
    /// recognized, so a splice alone never separates two tokens. This is
    /// the *logical* adjacency `#` stringizing and the function-like
    /// `#define` test need; `line`/`col` are physical positions and, once
    /// splices are deleted, no longer answer "did these tokens touch" —
    /// `a\`-newline-`b` is one token starting at `a`, and `a-\`-newline-`>`
    /// one `->` whose halves sit on different lines. A synthesized token
    /// (`Token::new`) is never adjacent; a token substituted for a macro
    /// parameter takes the parameter's flag, like gcc's `PREV_WHITE`.
    pub(crate) adjacent_before: bool,
    /// For a token that came out of a macro replacement list: the
    /// `(line, col)` of the outermost invocation that produced it, in the
    /// file being processed. `line`/`col` keep the definition-site
    /// coordinates; this is what the [`crate::LineMap`] and `__LINE__` report, so
    /// macro-expanded code attributes to its expansion site even through
    /// forwarding macros.
    pub(crate) origin: Option<(u32, u32)>,
}

impl Token {
    #[must_use]
    pub fn new(kind: TokenKind, line: u32, col: u32) -> Self {
        Self {
            kind,
            line,
            col,
            hidden: None,
            origin: None,
            adjacent_before: false,
        }
    }

    #[must_use]
    pub fn is_newline(&self) -> bool {
        matches!(self.kind, TokenKind::Newline)
    }

    #[must_use]
    pub fn is_eof(&self) -> bool {
        matches!(self.kind, TokenKind::Eof)
    }

    #[must_use]
    pub fn is_punct(&self, punct: &str) -> bool {
        matches!(&self.kind, TokenKind::Punct(s) if *s == punct)
    }

    #[must_use]
    pub fn is_ident(&self, name: &str) -> bool {
        matches!(&self.kind, TokenKind::Identifier(s) if s == name)
    }

    /// Where this token attributes to: its own position for source text,
    /// the outermost invocation for macro-expanded text.
    #[must_use]
    pub(crate) fn expansion_site(&self) -> (u32, u32) {
        self.origin.unwrap_or((self.line, self.col))
    }

    #[must_use]
    pub(crate) fn is_hidden(&self, name: &str) -> bool {
        self.hidden.as_ref().is_some_and(|h| h.contains(name))
    }

    /// Paint this replacement-list token with the invoking token's hide set
    /// plus `name` so the macro is not re-expanded (C11 6.10.3.4), and with
    /// the invocation's expansion site. `origin` may itself be a painted
    /// token (a forwarding macro's body), so its own site is inherited
    /// rather than its definition coordinates.
    #[must_use]
    pub(crate) fn with_macro_hide(&self, origin: &Token, name: &str) -> Token {
        let mut set = FxHashSet::default();
        if let Some(h) = &origin.hidden {
            set.extend(h.iter().cloned());
        }
        if let Some(h) = &self.hidden {
            set.extend(h.iter().cloned());
        }
        set.insert(name.to_string());
        Token {
            kind: self.kind.clone(),
            line: self.line,
            col: self.col,
            hidden: Some(Arc::new(set)),
            origin: Some(origin.expansion_site()),
            adjacent_before: self.adjacent_before,
        }
    }

    #[must_use]
    pub(crate) fn union_hidden(left: &Token, right: &Token) -> Option<Arc<FxHashSet<String>>> {
        match (&left.hidden, &right.hidden) {
            (None, None) => None,
            (Some(x), None) | (None, Some(x)) => Some(Arc::clone(x)),
            (Some(x), Some(y)) => {
                let mut s = (**x).clone();
                s.extend(y.iter().cloned());
                Some(Arc::new(s))
            }
        }
    }
}

/// Tokenizer for one file. Translation phase 2 happens here, at the
/// character level: `advance_char` steps over every `\`-newline it lands
/// on, so `pos` never rests on a splice and every reader — identifiers,
/// numbers, punctuator munching, string bodies, comment delimiters — sees
/// the spliced text without knowing splices exist. The one exception is a
/// C++11 raw string literal, whose body reverts phase 2 and is copied
/// physically ([lex.pptoken]p3). Token positions stay physical: a token
/// starts where its first character sits in the file, which is what the
/// [`crate::LineMap`] wants; whether it touched its predecessor is recorded on the
/// token (`Token::adjacent_before`) rather than recomputed from positions.
pub struct Lexer<'a> {
    input: &'a str,
    /// Byte offset of the next character to read; never inside a splice.
    pos: usize,
    line: u32,
    col: u32,
    /// Byte offset of every `\`-newline in `input`, ascending, found once
    /// up front so the per-character hot path compares `pos` against
    /// `next_splice` instead of loading a byte.
    splices: Vec<usize>,
    /// `splices[splice_idx]`, or `usize::MAX` once they are used up.
    next_splice: usize,
    splice_idx: usize,
    /// The previous token was a newline (or there was none): whatever comes
    /// next does not touch it.
    after_newline: bool,
    /// Decides the C++-only token shapes: raw string literals and
    /// user-defined-literal suffixes are one token in C++ and identifier +
    /// literal (or literal + identifier) in C, where the identifier can be
    /// a macro that must still expand; and `->*` is one token in C++ but
    /// `->` + `*` in C (#37). `MacroDef::relexed` reconciles exactly these
    /// shapes when a header is reachable from units of both languages.
    language: Language,
}

impl<'a> Lexer<'a> {
    #[must_use]
    pub fn new(input: &'a str, language: Language) -> Self {
        let mut lexer = Self {
            input,
            pos: 0,
            line: 1,
            col: 1,
            splices: find_splices(input),
            next_splice: usize::MAX,
            splice_idx: 0,
            after_newline: true,
            language,
        };
        lexer.resync_splices();
        lexer
    }

    fn is_cpp(&self) -> bool {
        self.language == Language::Cpp
    }

    #[must_use]
    pub fn tokenize(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token();
            let is_eof = matches!(tok.kind, TokenKind::Eof);
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        tokens
    }

    fn next_token(&mut self) -> Token {
        // Where the previous token ended. A token that starts right there,
        // with nothing skipped in between, touched it; a splice is not
        // "in between" because `pos` never stops on one.
        let end_of_prev = self.pos;
        loop {
            self.skip_whitespace_and_comments();
            let adjacent = self.pos == end_of_prev && !self.after_newline;
            let line = self.line;
            let col = self.col;
            let Some(mut tok) = self.lex_token(line, col) else {
                // Unknown character: skip it and lex what follows.
                self.advance_char();
                continue;
            };
            tok.adjacent_before = adjacent;
            self.after_newline = matches!(tok.kind, TokenKind::Newline);
            return tok;
        }
    }

    /// The token starting at the current position, or `None` for a
    /// character no token starts with.
    fn lex_token(&mut self, line: u32, col: u32) -> Option<Token> {
        if self.is_at_end() {
            return Some(Token::new(TokenKind::Eof, line, col));
        }

        let ch = self.peek_char();

        if ch == '\n' {
            self.advance_char();
            return Some(Token::new(TokenKind::Newline, line, col));
        }

        if ch == '#' {
            if self.peek_char_at(1) == '#' {
                self.advance_char();
                self.advance_char();
                return Some(Token::new(TokenKind::Punct("##"), line, col));
            }
            self.advance_char();
            return Some(Token::new(TokenKind::Hash, line, col));
        }

        if ch == '"' {
            return Some(self.read_string(self.pos, line, col));
        }

        if ch == '\'' {
            return Some(self.read_char(self.pos, line, col));
        }

        if ch.is_ascii_digit() {
            return Some(self.read_number(line, col));
        }

        if is_ident_start(ch) {
            // Only `R`, `u`, `U` and `L` can prefix a literal; every other
            // identifier skips the probe (this is the lexer's hottest path).
            if matches!(ch, 'R' | 'u' | 'U' | 'L') {
                if let Some(tok) = self.read_prefixed_literal(line, col) {
                    return Some(tok);
                }
            }
            return Some(self.read_identifier(line, col));
        }

        if let Some(one) = single_char_punct(ch) {
            self.advance_char();
            // Longest match wins, and the spelling goes straight into the
            // token: the tables already returned `&'static str`, so the one
            // `to_string()` that used to end this path is now gone and a
            // punctuator costs no allocation. `next` is read once and reused
            // by the three- and two-character cases.
            let next = self.peek_char();
            // Only `.` and `-` open an entry in `three_char_punct` — not
            // the wider C++ set, whose `<<=` / `>>=` / `<=>` are still one
            // token per character here — so every other punctuator skips
            // the second lookahead.
            let three = if matches!(ch, '.' | '-') {
                three_char_punct(ch, next, self.peek_char_at(1), self.is_cpp())
            } else {
                None
            };
            let spelling = three.or_else(|| two_char_punct(ch, next)).unwrap_or(one);
            // `ch` is consumed; take the rest. Punctuators are all ASCII, so
            // the byte length is the character count.
            for _ in 1..spelling.len() {
                self.advance_char();
            }
            return Some(Token::new(TokenKind::Punct(spelling), line, col));
        }

        None
    }

    /// An ordinary string literal whose opening quote is at the current
    /// position; `start` is where the token began (before any encoding
    /// prefix). Escapes are kept as written. A literal cut off by a newline
    /// or end of input gets its closing quote back so the output stays
    /// well-formed.
    fn read_string(&mut self, start: usize, line: u32, col: u32) -> Token {
        self.advance_char(); // opening "
        while !self.is_at_end() && self.peek_char() != '"' {
            if self.peek_char() == '\\' {
                self.advance_char();
                if !self.is_at_end() {
                    self.advance_char();
                }
            } else if self.peek_char() == '\n' {
                break;
            } else {
                self.advance_char();
            }
        }
        let mut spelling = self.close_literal(start, '"');
        self.read_ud_suffix(&mut spelling);
        Token::new(TokenKind::String(spelling), line, col)
    }

    fn read_char(&mut self, start: usize, line: u32, col: u32) -> Token {
        self.advance_char(); // opening '
        while !self.is_at_end() && self.peek_char() != '\'' {
            if self.peek_char() == '\\' {
                self.advance_char();
                if !self.is_at_end() {
                    self.advance_char();
                }
            } else {
                self.advance_char();
            }
        }
        let mut spelling = self.close_literal(start, '\'');
        self.read_ud_suffix(&mut spelling);
        Token::new(TokenKind::Char(spelling), line, col)
    }

    /// Append a user-defined-literal suffix (C++11 [lex.ext]): an identifier
    /// glued directly to the closing quote, as in `"x"_json` or `'c'_w`,
    /// is part of the literal token. Emitting it as a separate Identifier
    /// would put a space before it and change the program's meaning. An
    /// unterminated literal ends at a newline or end of input, so this
    /// never takes anything from the following line. C has no such suffix:
    /// `'a'C` is the literal followed by the identifier `C`, which may be
    /// a macro, so the C lexer leaves the identifier alone.
    fn read_ud_suffix(&mut self, spelling: &mut String) {
        if !self.is_cpp() || self.is_at_end() || !is_ident_start(self.peek_char()) {
            return;
        }
        while !self.is_at_end() && is_ident_continue(self.peek_char()) {
            spelling.push(self.peek_char());
            self.advance_char();
        }
    }

    /// Consume the closing `quote` if it is there and return the literal's
    /// spelling from `start`, with the quote appended when it was missing.
    /// The spelling is the text as written minus any splice the reader
    /// stepped over: the reader's view and `strip_splices` agree because
    /// both delete a `\`-newline wherever one starts.
    fn close_literal(&mut self, start: usize, quote: char) -> String {
        let closed = !self.is_at_end() && self.peek_char() == quote;
        if closed {
            self.advance_char();
        }
        let mut spelling = strip_splices(&self.input[start..self.pos]);
        if !closed {
            spelling.push(quote);
        }
        spelling
    }

    /// A literal introduced by an identifier character: an encoding prefix
    /// (`u8`, `u`, `U`, `L`) on a string or character literal, or, in C++,
    /// a raw string with or without one (C++11 [lex.string]). Returns
    /// `None` and consumes nothing when the text is an ordinary identifier,
    /// so `Rect`, `u8x`, `L` or `L "x"` (with a space) all lex as before.
    /// C has no raw strings: there `R"(x)"` is the identifier `R` (possibly
    /// a macro) followed by an ordinary string. The prefix is read through
    /// the spliced view like any other token, so `L\`-newline-`"x"` is the
    /// literal `L"x"`.
    fn read_prefixed_literal(&mut self, line: u32, col: u32) -> Option<Token> {
        let ahead = self.lookahead::<4>();
        let enc = match ahead {
            ['u', '8', ..] => 2,
            ['u' | 'U' | 'L', ..] => 1,
            _ => 0,
        };
        if self.is_cpp() && ahead[enc] == 'R' && ahead[enc + 1] == '"' {
            if let Some(tok) = self.read_raw_string(enc, line, col) {
                return Some(tok);
            }
        }
        if enc == 0 {
            return None;
        }
        let quote = ahead[enc];
        if quote != '"' && quote != '\'' {
            return None;
        }
        let start = self.pos;
        for _ in 0..enc {
            self.advance_char();
        }
        Some(if quote == '"' {
            self.read_string(start, line, col)
        } else {
            self.read_char(start, line, col)
        })
    }

    /// Lex a raw string literal: `enc` characters of encoding prefix, `R`,
    /// `"`, a d-char-sequence of at most 16 characters, `(`, an arbitrary
    /// body and `)` + the same d-char-sequence + `"`. Returns `None` and
    /// consumes nothing for a delimiter containing space, `\`, `)` or a
    /// control character, or a literal with no matching closer before end
    /// of input, leaving the text to the identifier/string paths so a
    /// malformed literal costs a couple of bad tokens instead of swallowing
    /// the file.
    ///
    /// From the opening `"` on, the text is taken physically: C++11
    /// [lex.pptoken]p3 reverts line splicing inside a raw string literal,
    /// so a `\`-newline in the body is two characters of the string, not a
    /// splice, and the literal is re-emitted exactly as written.
    fn read_raw_string(&mut self, enc: usize, line: u32, col: u32) -> Option<Token> {
        const MAX_DELIM: usize = 16;
        // The prefix and `R` are read through the spliced view; `pos` then
        // rests on the physical `"`.
        let prefix_end = self.pos;
        let mut prefix = self.lookahead_string(enc + 1);
        for _ in 0..=enc {
            self.advance_char();
        }
        let rest = &self.input[self.pos..];
        debug_assert!(rest.starts_with('"'));
        let after_quote = &rest[1..];
        // The delimiter is at most 16 d-chars, so look for `(` only that
        // far: an `R"..."` that is really a prefixed ordinary string must
        // not scan ahead to some unrelated `(` further down the file.
        let Some(delim_len) = after_quote
            .bytes()
            .take(MAX_DELIM + 1)
            .position(|b| b == b'(')
            .filter(|&n| after_quote.as_bytes()[..n].iter().all(|&b| is_d_char(b)))
        else {
            self.rewind(prefix_end, line, col);
            return None;
        };
        let delim = &after_quote[..delim_len];
        let body_start = 1 + delim_len + 1;
        let closer = format!("){delim}\"");
        let Some(body_len) = rest[body_start..].find(&closer) else {
            self.rewind(prefix_end, line, col);
            return None;
        };
        let total = body_start + body_len + closer.len();
        prefix.push_str(&rest[..total]);
        self.advance_physical(total);
        let mut spelling = prefix;
        self.read_ud_suffix(&mut spelling);
        Some(Token::new(TokenKind::String(spelling), line, col))
    }

    /// Put the lexer back at `pos`, a position it left from `(line, col)`.
    fn rewind(&mut self, pos: usize, line: u32, col: u32) {
        self.pos = pos;
        self.line = line;
        self.col = col;
        self.resync_splices();
    }

    /// Step over `bytes` of text as written, splices included — a raw
    /// string body — then back onto the spliced view.
    fn advance_physical(&mut self, bytes: usize) {
        let end = self.pos + bytes;
        while self.pos < end {
            let ch = self.input[self.pos..].chars().next().unwrap_or('\0');
            self.step(ch);
        }
        self.resync_splices();
    }

    fn read_number(&mut self, line: u32, col: u32) -> Token {
        let mut s = String::new();
        while !self.is_at_end() {
            let ch = self.peek_char();
            // `_` keeps a ud-suffix such as `10_km` inside the token.
            if ch.is_ascii_alphanumeric()
                || ch == '_'
                || ch == '.'
                || ch == 'x'
                || ch == 'X'
                || ch == 'u'
                || ch == 'U'
                || ch == 'l'
                || ch == 'L'
            {
                s.push(ch);
                self.advance_char();
            } else if ch == '\'' && self.peek_char_at(1).is_ascii_alphanumeric() {
                // C++14 digit separator (1'000'000): skip it so the whole
                // literal stays one Number token.
                self.advance_char();
            } else {
                break;
            }
        }
        Token::new(TokenKind::Number(s), line, col)
    }

    fn read_identifier(&mut self, line: u32, col: u32) -> Token {
        let mut s = String::new();
        while !self.is_at_end() {
            let ch = self.peek_char();
            if is_ident_continue(ch) {
                s.push(ch);
                self.advance_char();
            } else {
                break;
            }
        }
        Token::new(TokenKind::Identifier(s), line, col)
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            if self.is_at_end() {
                return;
            }
            let ch = self.peek_char();
            if ch == ' ' || ch == '\t' || ch == '\r' {
                self.advance_char();
                continue;
            }
            if ch == '/' && self.peek_char_at(1) == '/' {
                while !self.is_at_end() && self.peek_char() != '\n' {
                    self.advance_char();
                }
                continue;
            }
            if ch == '/' && self.peek_char_at(1) == '*' {
                self.advance_char();
                self.advance_char();
                while !self.is_at_end() {
                    if self.peek_char() == '*' && self.peek_char_at(1) == '/' {
                        self.advance_char();
                        self.advance_char();
                        break;
                    }
                    self.advance_char();
                }
                continue;
            }
            break;
        }
    }

    fn peek_char(&self) -> char {
        self.input[self.pos..].chars().next().unwrap_or('\0')
    }

    /// The characters from the current position on, in the spliced view:
    /// what `advance_char` would consume, one per step.
    fn spliced_chars(&self) -> impl Iterator<Item = char> + '_ {
        let input = self.input;
        let mut pos = self.pos;
        std::iter::from_fn(move || {
            let ch = input[pos..].chars().next()?;
            pos = splice_end(input, pos + ch.len_utf8());
            Some(ch)
        })
    }

    /// The character `offset` characters ahead in the spliced view, `\0`
    /// past the end.
    fn peek_char_at(&self, offset: usize) -> char {
        self.spliced_chars().nth(offset).unwrap_or('\0')
    }

    /// The next `N` characters of the spliced view, `\0`-padded.
    fn lookahead<const N: usize>(&self) -> [char; N] {
        let mut out = ['\0'; N];
        for (slot, ch) in out.iter_mut().zip(self.spliced_chars()) {
            *slot = ch;
        }
        out
    }

    /// The next `n` characters of the spliced view as text.
    fn lookahead_string(&self, n: usize) -> String {
        self.spliced_chars().take(n).collect()
    }

    /// Consume the current character and land on the next one of the
    /// spliced view. This runs once per character of input, so the splice
    /// test is one integer compare here and the deletion itself is out of
    /// line.
    #[inline]
    fn advance_char(&mut self) {
        let Some(ch) = self.input[self.pos..].chars().next() else {
            return;
        };
        self.step(ch);
        if self.pos == self.next_splice {
            self.skip_splices();
        }
    }

    /// Consume `ch`, the character at `pos`, tracking line and column.
    #[inline]
    fn step(&mut self, ch: char) {
        self.pos += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
    }

    /// Translation phase 2: delete every `\`-newline at the current
    /// position. Each one deleted moves to the start of the next line.
    #[inline(never)]
    fn skip_splices(&mut self) {
        while self.pos == self.next_splice {
            self.pos += splice_len(self.input, self.pos);
            self.line += 1;
            self.col = 1;
            self.splice_idx += 1;
            self.aim_next_splice();
        }
    }

    /// Re-aim `next_splice` after `pos` moved somewhere other than through
    /// `advance_char`, then delete any splice found there.
    fn resync_splices(&mut self) {
        self.splice_idx = self.splices.partition_point(|&at| at < self.pos);
        self.aim_next_splice();
        self.skip_splices();
    }

    fn aim_next_splice(&mut self) {
        self.next_splice = self
            .splices
            .get(self.splice_idx)
            .copied()
            .unwrap_or(usize::MAX);
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.input.len()
    }
}

/// Length in bytes of the line splice at `pos`, or 0 if there is none: a
/// `\` followed by a newline (`\n` or `\r\n`). gcc and clang also splice
/// when horizontal whitespace — space, tab, vertical tab or form feed —
/// sits between the `\` and the newline (with a warning), and so does
/// this. A `\r` is only the first half of a `\r\n`: `\`-CR-CR-LF and
/// `\`-CR-space-LF are not splices in either compiler, and not here.
fn splice_len(input: &str, pos: usize) -> usize {
    let bytes = input.as_bytes();
    if bytes.get(pos) != Some(&b'\\') {
        return 0;
    }
    let mut i = pos + 1;
    while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\x0b' | b'\x0c')) {
        i += 1;
    }
    if bytes.get(i) == Some(&b'\r') {
        i += 1;
    }
    if bytes.get(i) == Some(&b'\n') {
        i + 1 - pos
    } else {
        0
    }
}

/// Byte offsets of every line splice in `input`, ascending. `\` is rare
/// outside string escapes and directive continuations, so this is one fast
/// byte search over the file rather than a test on every character lexed.
fn find_splices(input: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = input[from..].find('\\') {
        let at = from + i;
        if splice_len(input, at) > 0 {
            out.push(at);
        }
        from = at + 1;
    }
    out
}

/// `text` with every line splice in it deleted.
fn strip_splices(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('\\') {
        let len = splice_len(rest, i);
        if len == 0 {
            out.push_str(&rest[..=i]);
            rest = &rest[i + 1..];
        } else {
            out.push_str(&rest[..i]);
            rest = &rest[i + len..];
        }
    }
    out.push_str(rest);
    out
}

/// `pos` moved past every line splice that starts there.
fn splice_end(input: &str, mut pos: usize) -> usize {
    loop {
        let len = splice_len(input, pos);
        if len == 0 {
            return pos;
        }
        pos += len;
    }
}

/// The punctuator spelling for a character that can begin one, or `None` if
/// it cannot. Doubles as the "is this a punctuator?" test, so the dispatch
/// does not also scan a string of the alphabet.
fn single_char_punct(ch: char) -> Option<&'static str> {
    Some(match ch {
        '+' => "+",
        '-' => "-",
        '<' => "<",
        '>' => ">",
        '=' => "=",
        '!' => "!",
        '&' => "&",
        '|' => "|",
        '^' => "^",
        '~' => "~",
        '*' => "*",
        '/' => "/",
        '%' => "%",
        '.' => ".",
        ',' => ",",
        ';' => ";",
        ':' => ":",
        '(' => "(",
        ')' => ")",
        '[' => "[",
        ']' => "]",
        '{' => "{",
        '}' => "}",
        '?' => "?",
        '\\' => "\\",
        _ => return None,
    })
}

/// The three-character punctuator `a`+`b`+`c`, or `None`. These are the two
/// the token stream needs as single tokens: `...`, which the re-speller
/// otherwise wrote back as `. . .` (#28), and `->*`, which it wrote back as
/// `-> *` because the spacing rule puts a space before `*` after `>` (#37).
/// The rest of the C++ set — `::`, `.*`, `<<=`, `>>=`, `<=>` — is still one
/// token per character; see docs/PREPROCESSOR.md for why, and for what a
/// general longest-match table would have to migrate first.
///
/// `->*` is a **C++ punctuator only**: gcc and clang tokenize `a->*b` in C
/// as `->` then `*` (and then reject it, since `->` wants a member name).
/// Lexing is language-aware here for the same reason it is for raw strings
/// and ud-suffixes — a header reachable from both a C and a C++ unit is
/// warmed once per language, and each replay has to be the tokenization
/// that language's lexer would produce. `...` is in both languages.
fn three_char_punct(a: char, b: char, c: char, cpp: bool) -> Option<&'static str> {
    Some(match (a, b, c) {
        ('.', '.', '.') => "...",
        ('-', '>', '*') if cpp => "->*",
        _ => return None,
    })
}

/// The two-character operator `a`+`b` spell, or `None` if they do not form
/// one. `b` is `\0` at end of input, which matches nothing.
fn two_char_punct(a: char, b: char) -> Option<&'static str> {
    Some(match (a, b) {
        ('<', '<') => "<<",
        ('>', '>') => ">>",
        ('<', '=') => "<=",
        ('>', '=') => ">=",
        ('=', '=') => "==",
        ('!', '=') => "!=",
        ('&', '&') => "&&",
        ('|', '|') => "||",
        ('+', '+') => "++",
        ('-', '-') => "--",
        ('+', '=') => "+=",
        ('-', '=') => "-=",
        ('*', '=') => "*=",
        ('/', '=') => "/=",
        ('%', '=') => "%=",
        ('&', '=') => "&=",
        ('|', '=') => "|=",
        ('^', '=') => "^=",
        ('-', '>') => "->",
        _ => return None,
    })
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Member of a raw string's d-char-sequence: any basic source character
/// except space, parentheses, backslash and control characters.
fn is_d_char(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'(' | b')' | b'\\')
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Identifier(s) => write!(f, "id({s})"),
            TokenKind::Number(s) => write!(f, "num({s})"),
            TokenKind::String(s) => write!(f, "str({s})"),
            TokenKind::Char(s) => write!(f, "char({s})"),
            TokenKind::Punct(s) => write!(f, "{s}"),
            TokenKind::Hash => write!(f, "#"),
            TokenKind::Newline => write!(f, "\\n"),
            TokenKind::Eof => write!(f, "EOF"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_simple_c() {
        let tokens = Lexer::new("int x = 42;", Language::C).tokenize();
        assert!(tokens
            .iter()
            .any(|t| matches!(&t.kind, TokenKind::Identifier(s) if s == "int")));
        assert!(tokens
            .iter()
            .any(|t| matches!(&t.kind, TokenKind::Number(s) if s == "42")));
    }

    fn kinds_in(src: &str, language: Language) -> Vec<TokenKind> {
        Lexer::new(src, language)
            .tokenize()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Eof))
            .collect()
    }

    /// Token kinds under the C++ lexer (raw strings and ud-suffixes on).
    fn kinds(src: &str) -> Vec<TokenKind> {
        kinds_in(src, Language::Cpp)
    }

    fn kinds_c(src: &str) -> Vec<TokenKind> {
        kinds_in(src, Language::C)
    }

    /// A literal spelled as written (raw or prefixed).
    fn raw(s: &str) -> TokenKind {
        TokenKind::String(s.to_string())
    }

    fn id(s: &str) -> TokenKind {
        TokenKind::Identifier(s.to_string())
    }

    /// An ordinary string literal with the given body.
    fn string(body: &str) -> TokenKind {
        TokenKind::String(format!("\"{body}\""))
    }

    fn chr(s: &str) -> TokenKind {
        TokenKind::Char(s.to_string())
    }

    fn punct(s: &'static str) -> TokenKind {
        TokenKind::Punct(s)
    }

    #[test]
    fn raw_string_is_one_token_with_inner_quotes_and_parens() {
        // Issue #14: `R"(a "quoted" b)"` used to lex as `R "(a " quoted " b)"`.
        assert_eq!(
            kinds(r#"const char* j = R"(a "quoted" (b))";"#),
            vec![
                id("const"),
                id("char"),
                punct("*"),
                id("j"),
                punct("="),
                raw(r#"R"(a "quoted" (b))""#),
                punct(";"),
            ]
        );
    }

    #[test]
    fn raw_string_honours_d_char_sequence_delimiter() {
        // `)"` inside the body does not end a `~`-delimited literal.
        assert_eq!(
            kinds(r#"R"~({"k":")~" + v + R"~(",)~""#),
            vec![
                raw(r#"R"~({"k":")~""#),
                punct("+"),
                id("v"),
                punct("+"),
                raw(r#"R"~(",)~""#),
            ]
        );
        assert_eq!(
            kinds(r#"R"~(=((".*?")|(\S*)))~""#),
            vec![raw(r#"R"~(=((".*?")|(\S*)))~""#)]
        );
    }

    #[test]
    fn raw_string_accepts_encoding_prefixes() {
        for prefix in ["u8R", "uR", "UR", "LR"] {
            let src = format!("x = {prefix}\"(y)\";");
            assert_eq!(
                kinds(&src),
                vec![
                    id("x"),
                    punct("="),
                    raw(&format!("{prefix}\"(y)\"")),
                    punct(";")
                ],
                "{src}"
            );
        }
    }

    #[test]
    fn raw_string_spans_lines_as_one_token() {
        let src = "a = R\"~({\n  \"k\": 1,\n  \"v\": 2})~\";\nint z;";
        assert_eq!(
            kinds(src),
            vec![
                id("a"),
                punct("="),
                raw("R\"~({\n  \"k\": 1,\n  \"v\": 2})~\""),
                punct(";"),
                TokenKind::Newline,
                id("int"),
                id("z"),
                punct(";"),
            ]
        );
        // The `;` after the literal sits on line 3 right after `)~"`.
        let toks = Lexer::new(src, Language::Cpp).tokenize();
        let semi = &toks[3];
        assert_eq!((semi.line, semi.col), (3, 13), "{semi:?}");
        let int_tok = &toks[5];
        assert_eq!((int_tok.line, int_tok.col), (4, 1), "{int_tok:?}");
    }

    #[test]
    fn user_defined_literal_suffix_stays_in_the_token() {
        // A ud-suffix is part of the literal token (C++11 [lex.ext]); a
        // separate Identifier would gain a space on emission and break
        // `R"(json)"_json` into `R"(json)" _json`.
        assert_eq!(
            kinds(r#"auto j = R"(json)"_json;"#),
            vec![
                id("auto"),
                id("j"),
                punct("="),
                raw(r#"R"(json)"_json"#),
                punct(";")
            ]
        );
        assert_eq!(kinds(r#"u8R"~(x)~"_w"#), vec![raw(r#"u8R"~(x)~"_w"#)]);
        assert_eq!(
            kinds(r#""abc"_json + "s"s"#),
            vec![raw(r#""abc"_json"#), punct("+"), raw(r#""s"s"#)]
        );
        assert_eq!(kinds("L'c'_x"), vec![chr("L'c'_x")]);
        assert_eq!(
            kinds("10_km + 1.5_m"),
            vec![
                TokenKind::Number("10_km".to_string()),
                punct("+"),
                TokenKind::Number("1.5_m".to_string())
            ]
        );
        // Whitespace between the literal and an identifier keeps them apart.
        assert_eq!(
            kinds(r#"R"(x)" _json"#),
            vec![raw(r#"R"(x)""#), id("_json")]
        );
        assert_eq!(kinds(r#""x" s"#), vec![string("x"), id("s")]);
        // An unterminated string takes no suffix from the next line.
        assert_eq!(
            kinds("\"abc\nx"),
            vec![string("abc"), TokenKind::Newline, id("x")]
        );
    }

    #[test]
    fn raw_string_backslashes_and_comment_markers_are_literal() {
        // No escape processing and no comment stripping inside the body.
        assert_eq!(
            kinds(r#"R"(\n // not a comment /* nor this */ \")""#),
            vec![raw(r#"R"(\n // not a comment /* nor this */ \")""#)]
        );
    }

    #[test]
    fn r_not_starting_a_raw_string_stays_an_identifier() {
        // No `(` after the quote: an ordinary (prefixed) string literal.
        assert_eq!(kinds(r#"R"abc""#), vec![id("R"), string("abc")]);
        // `R` is only a prefix when it is the whole identifier before `"`.
        assert_eq!(kinds(r#"FOOR"(x)""#), vec![id("FOOR"), string("(x)")]);
        // A space between `R` and `"` makes it two tokens.
        assert_eq!(kinds(r#"R "(x)""#), vec![id("R"), string("(x)")]);
        // Lower-case `r` is not a raw-string prefix.
        assert_eq!(kinds(r#"r"(x)""#), vec![id("r"), string("(x)")]);
        // A plain identifier that happens to start with R.
        assert_eq!(kinds("Rect r;"), vec![id("Rect"), id("r"), punct(";")]);
    }

    #[test]
    fn malformed_raw_string_falls_back_to_ordinary_lexing() {
        // Delimiter longer than 16 chars is not a d-char-sequence.
        assert_eq!(
            kinds(r#"R"abcdefghijklmnopq(x)abcdefghijklmnopq""#),
            vec![id("R"), string("abcdefghijklmnopq(x)abcdefghijklmnopq")]
        );
        // Space / backslash / `)` are not d-chars.
        assert_eq!(kinds(r#"R"a b(x)a b""#), vec![id("R"), string("a b(x)a b")]);
        // (`\"` is an escape for the ordinary string reader, which then
        // runs to end of input — the pre-existing behaviour for a bad string.)
        assert_eq!(kinds(r#"R"a\(x)a\""#), vec![id("R"), string(r#"a\(x)a\""#)]);
        // No closer with the right delimiter before end of input: fall back
        // instead of swallowing the rest of the file.
        assert_eq!(
            kinds("R\"~(x)\" y\nz"),
            vec![
                id("R"),
                string("~(x)"),
                id("y"),
                TokenKind::Newline,
                id("z")
            ]
        );
    }

    #[test]
    fn encoding_prefixed_literals_are_one_token() {
        for prefix in ["u8", "u", "U", "L"] {
            let src = format!("a = {prefix}\"s\" + {prefix}'c';");
            assert_eq!(
                kinds(&src),
                vec![
                    id("a"),
                    punct("="),
                    raw(&format!("{prefix}\"s\"")),
                    punct("+"),
                    chr(&format!("{prefix}'c'")),
                    punct(";"),
                ],
                "{src}"
            );
        }
        // Escapes inside a prefixed literal are kept as written.
        assert_eq!(
            kinds(r#"L"a\"b" L'\n'"#),
            vec![raw(r#"L"a\"b""#), chr(r#"L'\n'"#)]
        );
    }

    #[test]
    fn prefix_lookalikes_stay_identifiers() {
        assert_eq!(
            kinds("u8x = L;"),
            vec![id("u8x"), punct("="), id("L"), punct(";")]
        );
        assert_eq!(kinds(r#"L "x""#), vec![id("L"), string("x")]);
        assert_eq!(kinds("U + u"), vec![id("U"), punct("+"), id("u")]);
        assert_eq!(kinds("Lx'c'"), vec![id("Lx"), chr("'c'")]);
    }

    #[test]
    fn literals_carry_their_spelling() {
        assert_eq!(
            kinds(r#""a\"b" 'c' '\''"#),
            vec![string(r#"a\"b"#), chr("'c'"), chr(r#"'\''"#)]
        );
        // A string cut off by a newline gets its closing quote back.
        assert_eq!(
            kinds("\"open\nint x;"),
            vec![
                string("open"),
                TokenKind::Newline,
                id("int"),
                id("x"),
                punct(";")
            ]
        );
    }

    #[test]
    fn c_has_no_raw_strings() {
        // In C `R` is an identifier (maybe a macro) and `"(x)"` an ordinary
        // string; the same text is one raw-string token in C++.
        assert_eq!(kinds_c(r#"R"(x)""#), vec![id("R"), string("(x)")]);
        assert_eq!(kinds(r#"R"(x)""#), vec![raw(r#"R"(x)""#)]);
        assert_eq!(
            kinds_c(r#"u8R"~(a "b")~""#),
            vec![id("u8R"), string("~(a "), id("b"), string(")~")]
        );
        // Encoding prefixes exist in C too and stay glued to the literal.
        assert_eq!(
            kinds_c(r#"L"w" u8"s" u'c'"#),
            vec![raw(r#"L"w""#), raw(r#"u8"s""#), chr("u'c'")]
        );
    }

    #[test]
    fn c_has_no_user_defined_literal_suffix() {
        // `'a'C` is the literal followed by the identifier `C` in C; the
        // identifier may be a macro and must stay its own token.
        assert_eq!(kinds_c("'a'C"), vec![chr("'a'"), id("C")]);
        assert_eq!(kinds("'a'C"), vec![chr("'a'C")]);
        assert_eq!(kinds_c(r#""x"_s"#), vec![string("x"), id("_s")]);
        assert_eq!(kinds_c("L'c'_x"), vec![chr("L'c'"), id("_x")]);
        // A pp-number swallows a trailing identifier in both languages
        // (C11 6.4.8): `10_km` is one token either way.
        assert_eq!(
            kinds_c("10_km"),
            vec![TokenKind::Number("10_km".to_string())]
        );
    }

    #[test]
    fn ellipsis_is_one_punctuator() {
        // Issue #28: `...` used to lex as three `.` tokens, which the token
        // re-speller wrote back as `. . .`, breaking every variadic
        // declaration.
        assert_eq!(
            kinds_c("int f(const char *fmt, ...);"),
            vec![
                id("int"),
                id("f"),
                punct("("),
                id("const"),
                id("char"),
                punct("*"),
                id("fmt"),
                punct(","),
                punct("..."),
                punct(")"),
                punct(";"),
            ]
        );
        assert_eq!(
            kinds("template <class... T> void f(T... a);")[3],
            punct("...")
        );
    }

    #[test]
    fn pointer_to_member_arrow_is_one_punctuator() {
        // #37; the why is on `three_char_punct`.
        assert_eq!(
            kinds("int v = c->*m;"),
            vec![
                id("int"),
                id("v"),
                punct("="),
                id("c"),
                punct("->*"),
                id("m"),
                punct(";"),
            ]
        );
        // The call form the corpus actually uses.
        assert_eq!(
            kinds("(c->*fp)();"),
            vec![
                punct("("),
                id("c"),
                punct("->*"),
                id("fp"),
                punct(")"),
                punct("("),
                punct(")"),
                punct(";"),
            ]
        );
    }

    #[test]
    fn pointer_to_member_arrow_is_cpp_only() {
        // `->*` is C++-only; see `three_char_punct`.
        assert_eq!(
            kinds_c("int v = c->*m;"),
            vec![
                id("int"),
                id("v"),
                punct("="),
                id("c"),
                punct("->"),
                punct("*"),
                id("m"),
                punct(";"),
            ]
        );
        // `...` is a punctuator in both languages and stays one token.
        assert_eq!(kinds_c("void f(int a, ...);")[6], punct("..."));
    }

    #[test]
    fn arrow_without_a_star_stays_two_characters() {
        // Maximal munch: `->` alone is unchanged and never over-munched.
        assert_eq!(
            kinds("a->b;"),
            vec![id("a"), punct("->"), id("b"), punct(";")]
        );
        assert_eq!(
            kinds("a->*b->c;"),
            vec![
                id("a"),
                punct("->*"),
                id("b"),
                punct("->"),
                id("c"),
                punct(";")
            ]
        );
        // `-` and `->` at end of input must not read past it.
        assert_eq!(kinds("a-"), vec![id("a"), punct("-")]);
        assert_eq!(kinds("a->"), vec![id("a"), punct("->")]);
    }

    #[test]
    fn two_dots_stay_separate_tokens() {
        // Only a full `...` is one token; a shorter run is still one `.`
        // each, so `x..y` keeps its token boundaries.
        assert_eq!(
            kinds_c("x..y"),
            vec![id("x"), punct("."), punct("."), id("y")]
        );
        assert_eq!(kinds_c("....."), vec![punct("..."), punct("."), punct(".")]);
    }

    /// Translation phase 2 (C11 5.1.1.2p1) deletes every `\`-newline before
    /// tokens are recognized, so a multi-character punctuator written
    /// across one is still munched as one token (#38).
    #[test]
    fn splice_split_punctuator_is_one_token() {
        assert_eq!(
            kinds_c("f(x, .\\\n..);"),
            vec![
                id("f"),
                punct("("),
                id("x"),
                punct(","),
                punct("..."),
                punct(")"),
                punct(";"),
            ]
        );
        assert_eq!(kinds_c("a-\\\n>b"), vec![id("a"), punct("->"), id("b")]);
        assert_eq!(kinds_c("a=\\\n=b"), vec![id("a"), punct("=="), id("b")]);
        assert_eq!(kinds("c-\\\n>\\\n*m"), vec![id("c"), punct("->*"), id("m")]);
        assert_eq!(kinds_c("#\\\n#"), vec![punct("##")]);
    }

    #[test]
    fn splice_split_identifier_and_number_are_one_token() {
        assert_eq!(
            kinds_c("int c\\\nd;"),
            vec![id("int"), id("cd"), punct(";")]
        );
        assert_eq!(
            kinds_c("x = 0x\\\n1F;"),
            vec![
                id("x"),
                punct("="),
                TokenKind::Number("0x1F".to_string()),
                punct(";")
            ]
        );
        // A splice inside a string or character literal is deleted too.
        assert_eq!(kinds_c("\"ab\\\ncd\""), vec![string("abcd")]);
        assert_eq!(kinds_c("'\\\\\nn'"), vec![chr(r"'\n'")]);
        // An encoding prefix split from its literal is still a prefix.
        assert_eq!(kinds_c("L\\\n\"w\""), vec![raw("L\"w\"")]);
    }

    #[test]
    fn splice_accepts_crlf_and_trailing_whitespace() {
        // A CRLF file splices the same way, and gcc/clang splice a `\`
        // separated from its newline by horizontal whitespace — space,
        // tab, vertical tab, form feed — with a warning.
        assert_eq!(kinds_c("a.\\\r\n..b"), vec![id("a"), punct("..."), id("b")]);
        assert_eq!(
            kinds_c("in\\ \t\nt x;"),
            vec![id("int"), id("x"), punct(";")]
        );
        assert_eq!(kinds_c("a\\\x0b\nb"), vec![id("ab")]);
        assert_eq!(kinds_c("a\\\x0c\r\nb"), vec![id("ab")]);
        // A `\r` that is not the first half of a `\r\n` ends the run:
        // clang keeps these as two lines (checked with `clang -E`), and so
        // the `\` stays a token.
        assert_eq!(
            kinds_c("a\\\r\r\nb"),
            vec![id("a"), punct("\\"), TokenKind::Newline, id("b")]
        );
        assert_eq!(
            kinds_c("a\\\r \nb"),
            vec![id("a"), punct("\\"), TokenKind::Newline, id("b")]
        );
    }

    #[test]
    fn splice_continues_a_comment() {
        // Phase 2 runs before comments are recognized (phase 3), so a `//`
        // comment ending in `\` swallows the next line, and `/\`+newline+`*`
        // opens a block comment. Both are what gcc and clang do.
        assert_eq!(
            kinds_c("a // c \\\n b\nd"),
            vec![id("a"), TokenKind::Newline, id("d")]
        );
        assert_eq!(kinds_c("a /\\\n* x *\\\n/ b"), vec![id("a"), id("b")]);
    }

    #[test]
    fn splice_inside_a_raw_string_is_kept() {
        // C++11 [lex.pptoken]p3: inside a raw string literal phase 2 is
        // reverted, so the body is emitted exactly as written.
        assert_eq!(kinds("R\"(a\\\nb)\""), vec![raw("R\"(a\\\nb)\"")]);
        // The prefix itself may be spliced; the body still is not.
        assert_eq!(kinds("R\\\n\"(a\\\nb)\""), vec![raw("R\"(a\\\nb)\"")]);
        // A splice right after the literal is deleted again — so much so
        // that an identifier there is a ud-suffix — and so is one after a
        // would-be raw string that fell back to identifier + string.
        assert_eq!(kinds("R\"(a)\"\\\nx"), vec![raw("R\"(a)\"x")]);
        assert_eq!(
            kinds("R\"(a)\"\\\n+x"),
            vec![raw("R\"(a)\""), punct("+"), id("x")]
        );
        assert_eq!(
            kinds("R\"a b(x)\"\\\n+y"),
            vec![id("R"), string("a b(x)"), punct("+"), id("y")]
        );
        // In C, `R"(...)"` is the identifier `R` and an ordinary string, so
        // a splice in the body is deleted like in any string — what clang
        // does under `-std=c11`; its default GNU mode and gcc's lex a raw
        // string there and keep the splice.
        assert_eq!(kinds_c("R\"(x\\\ny)\""), vec![id("R"), string("(xy)")]);
    }

    #[test]
    fn backslash_not_before_a_newline_is_a_token() {
        assert_eq!(kinds_c("a \\ b"), vec![id("a"), punct("\\"), id("b")]);
        assert_eq!(kinds_c("a\\"), vec![id("a"), punct("\\")]);
    }

    #[test]
    fn tokens_after_a_splice_keep_physical_positions() {
        let toks = Lexer::new("ab\\\ncd ef\\\n\ngh", Language::C).tokenize();
        let at = |i: usize| (toks[i].line, toks[i].col);
        assert_eq!(toks[0].kind, id("abcd"));
        assert_eq!(at(0), (1, 1));
        assert_eq!(toks[1].kind, id("ef"));
        assert_eq!(at(1), (2, 4));
        assert_eq!(toks[2].kind, TokenKind::Newline);
        assert_eq!(at(2), (3, 1));
        assert_eq!(toks[3].kind, id("gh"));
        assert_eq!(at(3), (4, 1));
    }

    /// `adjacent_before` answers "did this token touch the previous one in
    /// the phase-3 stream?" — what `#` stringizing and the function-like
    /// `#define` test need, and what physical positions stop answering
    /// once splices are deleted.
    #[test]
    fn adjacency_flag_sees_through_splices_and_not_through_whitespace() {
        let flags = |src: &str| -> Vec<bool> {
            Lexer::new(src, Language::C)
                .tokenize()
                .into_iter()
                .take_while(|t| !matches!(t.kind, TokenKind::Eof))
                .map(|t| t.adjacent_before)
                .collect()
        };
        assert_eq!(flags("a(b) c"), vec![false, true, true, true, false]);
        assert_eq!(flags("a\\\n(b"), vec![false, true, true]);
        assert_eq!(flags("a \\\n(b"), vec![false, false, true]);
        assert_eq!(flags("a\\\n (b"), vec![false, false, true]);
        assert_eq!(flags("a/**/b"), vec![false, false]);
        assert_eq!(flags("a\nb"), vec![false, true, false]);
        // Two tight splices in a row are still nothing: `ab` is one
        // identifier, and `(` after them touches `a`.
        assert_eq!(flags("a\\\n\\\nb"), vec![false]);
        assert_eq!(flags("a\\\n\\\n(b"), vec![false, true, true]);
    }
}
