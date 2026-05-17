use crate::SourceFile;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Eof,
    Identifier,
    Number,
    // keywords
    Module,
    Endmodule,
    Input,
    Output,
    Inout,
    Parameter,
    Localparam,
    Wire,
    Reg,
    /// Non-1364 `logic` keyword — accepted only so declarations are not misparsed as instances. Prefer `wire`/`reg` in portable Verilog.
    Logic,
    Assign,
    // sequential / procedural keywords
    Always,
    Initial,
    Posedge,
    Negedge,
    If,
    Else,
    Begin,
    End,
    Case,
    Endcase,
    Default,
    For,
    Integer,
    /// `signed` (net type qualifier before vector dimensions).
    Signed,
    /// `generate` / `endgenerate` (generate regions).
    Generate,
    Endgenerate,
    /// `genvar` (loop index in generate; statement skipped at CST level).
    Genvar,
    Function,
    Endfunction,
    Task,
    Endtask,
    Casez,
    Casex,
    // non-blocking assign
    NonBlockAssign, // <=  (contextually distinct from Le)
    // punctuation / single-char
    LParen,
    RParen,
    Comma,
    Semicolon,
    LBracket,
    RBracket,
    Colon,
    Hash,
    LBrace,
    RBrace,
    Question,
    Dot,
    // arithmetic
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    // bitwise
    Amp,     // &
    Pipe,    // |
    Caret,   // ^
    Tilde,   // ~
    // comparison / equality
    Lt,      // <
    Gt,      // >
    Le,      // <=
    Ge,      // >=
    EqEq,    // ==
    Ne,      // !=
    // shifts
    Shl,     // <<
    Shr,     // >>
    Ashr,    // >>>
    // logical
    LogAnd,  // &&
    LogOr,   // ||
    Bang,    // !
    // assignment
    Eq,      // =
    // misc
    At,      // @
    Other,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub lexeme: String,
    pub offset: usize,
    /// Exclusive end byte offset: `offset + lexeme.len()` for normal tokens; equals `offset` for EOF.
    pub end: usize,
}

impl Token {
    #[inline]
    pub fn span_range(&self) -> (u32, u32) {
        (self.offset as u32, self.end as u32)
    }
}

/// Lex the contents of a [`SourceFile`] into a flat list of tokens.
pub fn lex(file: &SourceFile) -> Vec<Token> {
    Lexer::new(&file.content).lex()
}

struct Lexer<'a> {
    src: &'a str,
    offset: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, offset: 0 }
    }

    fn next_char(&self) -> Option<char> {
        self.src[self.offset..].chars().next()
    }

    fn peek_char(&self) -> Option<char> {
        let mut iter = self.src[self.offset..].chars();
        iter.next();
        iter.next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.next_char()?;
        self.offset += ch.len_utf8();
        Some(ch)
    }

    fn lex(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Some(ch) = self.next_char() {
            if ch.is_whitespace() {
                self.bump();
                continue;
            }
            // line comments
            if ch == '/' {
                if self.peek_char() == Some('/') {
                    self.bump();
                    self.bump();
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                    continue;
                } else if self.peek_char() == Some('*') {
                    self.bump();
                    self.bump();
                    loop {
                        match self.bump() {
                            Some('*') if self.next_char() == Some('/') => {
                                self.bump();
                                break;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                    continue;
                }
            }

            let start = self.offset;

            if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' {
                self.bump();
                while let Some(c) = self.next_char() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                        self.bump();
                    } else {
                        break;
                    }
                }
                let text = &self.src[start..self.offset];
                let kind = match text {
                    "module" => TokenKind::Module,
                    "endmodule" => TokenKind::Endmodule,
                    "input" => TokenKind::Input,
                    "output" => TokenKind::Output,
                    "inout" => TokenKind::Inout,
                    "parameter" => TokenKind::Parameter,
                    "localparam" => TokenKind::Localparam,
                    "wire" => TokenKind::Wire,
                    "reg" => TokenKind::Reg,
                    "logic" => TokenKind::Logic,
                    "assign" => TokenKind::Assign,
                    "always" => TokenKind::Always,
                    "initial" => TokenKind::Initial,
                    "posedge" => TokenKind::Posedge,
                    "negedge" => TokenKind::Negedge,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "begin" => TokenKind::Begin,
                    "end" => TokenKind::End,
                    "case" => TokenKind::Case,
                    "endcase" => TokenKind::Endcase,
                    "default" => TokenKind::Default,
                    "for" => TokenKind::For,
                    "integer" => TokenKind::Integer,
                    "signed" => TokenKind::Signed,
                    "generate" => TokenKind::Generate,
                    "endgenerate" => TokenKind::Endgenerate,
                    "genvar" => TokenKind::Genvar,
                    "function" => TokenKind::Function,
                    "endfunction" => TokenKind::Endfunction,
                    "task" => TokenKind::Task,
                    "endtask" => TokenKind::Endtask,
                    "casez" => TokenKind::Casez,
                    "casex" => TokenKind::Casex,
                    _ => TokenKind::Identifier,
                };
                tokens.push(Token {
                    kind,
                    lexeme: text.to_string(),
                    offset: start,
                    end: self.offset,
                });
            } else if ch.is_ascii_digit() {
                self.bump();
                // Sized literals like `8'b1???_????` include `?` in the body
                // (case-pattern wildcard). `x`/`z` are already accepted by
                // is_ascii_alphanumeric.
                while let Some(c) = self.next_char() {
                    if c.is_ascii_alphanumeric() || c == '\'' || c == '_' || c == '?' {
                        self.bump();
                    } else {
                        break;
                    }
                }
                tokens.push(Token {
                    kind: TokenKind::Number,
                    lexeme: self.src[start..self.offset].to_string(),
                    offset: start,
                    end: self.offset,
                });
            } else if ch == '\'' {
                // Unsized literals: 'd0, 'b1010, 'hFF (sizes like 8'd0 are one token from digit path)
                self.bump();
                let base = match self.next_char() {
                    Some(c) if matches!(c, 'b' | 'B' | 'd' | 'D' | 'h' | 'H' | 'o' | 'O') => c,
                    _ => {
                        tokens.push(Token {
                            kind: TokenKind::Other,
                            lexeme: "'".to_string(),
                            offset: start,
                            end: self.offset,
                        });
                        continue;
                    }
                };
                self.bump();
                match base {
                    'b' | 'B' => {
                        while let Some(c) = self.next_char() {
                            if matches!(c, '0' | '1' | '_' | 'x' | 'X' | 'z' | 'Z' | '?') {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    'd' | 'D' => {
                        while let Some(c) = self.next_char() {
                            if c.is_ascii_digit() || c == '_' {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    'h' | 'H' => {
                        while let Some(c) = self.next_char() {
                            if c.is_ascii_hexdigit()
                                || c == '_'
                                || matches!(c, 'x' | 'X' | 'z' | 'Z' | '?')
                            {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    'o' | 'O' => {
                        while let Some(c) = self.next_char() {
                            if matches!(c, '0'..='7' | '_') {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
                tokens.push(Token {
                    kind: TokenKind::Number,
                    lexeme: self.src[start..self.offset].to_string(),
                    offset: start,
                    end: self.offset,
                });
            } else {
                self.bump();
                let kind = match ch {
                    '(' => TokenKind::LParen,
                    ')' => TokenKind::RParen,
                    ',' => TokenKind::Comma,
                    ';' => TokenKind::Semicolon,
                    '[' => TokenKind::LBracket,
                    ']' => TokenKind::RBracket,
                    ':' => TokenKind::Colon,
                    '#' => TokenKind::Hash,
                    '{' => TokenKind::LBrace,
                    '}' => TokenKind::RBrace,
                    '?' => TokenKind::Question,
                    '.' => TokenKind::Dot,
                    '@' => TokenKind::At,
                    '+' => TokenKind::Plus,
                    '-' => TokenKind::Minus,
                    '*' => TokenKind::Star,
                    '%' => TokenKind::Percent,
                    '^' => TokenKind::Caret,
                    '~' => TokenKind::Tilde,
                    '/' => TokenKind::Slash,
                    '=' => {
                        if self.next_char() == Some('=') {
                            self.bump();
                            TokenKind::EqEq
                        } else {
                            TokenKind::Eq
                        }
                    }
                    '!' => {
                        if self.next_char() == Some('=') {
                            self.bump();
                            TokenKind::Ne
                        } else {
                            TokenKind::Bang
                        }
                    }
                    '<' => {
                        if self.next_char() == Some('<') {
                            self.bump();
                            TokenKind::Shl
                        } else if self.next_char() == Some('=') {
                            self.bump();
                            TokenKind::Le
                        } else {
                            TokenKind::Lt
                        }
                    }
                    '>' => {
                        if self.next_char() == Some('>') {
                            self.bump();
                            if self.next_char() == Some('>') {
                                self.bump();
                                TokenKind::Ashr
                            } else {
                                TokenKind::Shr
                            }
                        } else if self.next_char() == Some('=') {
                            self.bump();
                            TokenKind::Ge
                        } else {
                            TokenKind::Gt
                        }
                    }
                    '&' => {
                        if self.next_char() == Some('&') {
                            self.bump();
                            TokenKind::LogAnd
                        } else {
                            TokenKind::Amp
                        }
                    }
                    '|' => {
                        if self.next_char() == Some('|') {
                            self.bump();
                            TokenKind::LogOr
                        } else {
                            TokenKind::Pipe
                        }
                    }
                    _ => TokenKind::Other,
                };
                tokens.push(Token {
                    kind,
                    lexeme: self.src[start..self.offset].to_string(),
                    offset: start,
                    end: self.offset,
                });
            }
        }
        let eof_at = self.src.len();
        tokens.push(Token {
            kind: TokenKind::Eof,
            lexeme: String::new(),
            offset: eof_at,
            end: eof_at,
        });
        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceFile;

    #[test]
    fn logic_is_keyword() {
        let f = SourceFile::new("x.v", "logic");
        let t = lex(&f);
        assert_eq!(t[0].kind, TokenKind::Logic, "lexeme={}", t[0].lexeme);
    }
}
