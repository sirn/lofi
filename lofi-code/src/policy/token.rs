//! Shell command tokenizer.
//!
//! Parses a bash command string into a structured token stream: words,
//! operators (`|`, `||`, `&&`, `;`, `&`), redirects (`>`, `>>`, `<`, `<<`,
//! `<<<`), and groups (subshells `()`, command substitution `$()`,
//! backticks). Variable expansions (`$VAR`, `${VAR}`) are consumed as word
//! fragments so the agent cannot sneak forbidden values past rules.
//!
//! Fails closed: a parse error (unclosed quote, unmatched paren, …) returns
//! `Err`, and the caller treats that as an `ask` decision.

/// Kind of nested group token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupKind {
    /// `( … )` — subshell.
    Subshell,
    /// `$( … )` — command substitution.
    Substitution,
    /// `` ` … ` `` — backtick substitution.
    Backtick,
}

/// One parsed token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A word (may include concatenated quoted segments).
    Word(String),
    /// A control operator: `|`, `||`, `&&`, `;`, `&`.
    Operator(String),
    /// A redirect: `{ op, target }`.
    Redirect { op: String, target: String },
    /// A nested group (subshell, substitution, or backtick).
    Group { tokens: Vec<Token>, kind: GroupKind },
}

/// Tokenize a shell command string.
///
/// # Errors
/// Returns `Err(String)` on syntax errors.
pub fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut p = Parser::new(input);
    p.parse()?;
    Ok(p.tokens)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    pending_heredocs: Vec<(String, bool)>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
            pending_heredocs: Vec::new(),
        }
    }

    fn peek(&self, offset: usize) -> u8 {
        self.input.get(self.pos + offset).copied().unwrap_or(0)
    }

    fn peek_from(&self, base: usize, offset: usize) -> u8 {
        self.input.get(base + offset).copied().unwrap_or(0)
    }

    fn parse(&mut self) -> Result<(), String> {
        let len = self.input.len();
        while self.pos < len {
            let ch = self.input[self.pos];
            match ch {
                b' ' | b'\t' => {
                    self.pos += 1;
                }
                b'\n' | b'\r' => {
                    if ch == b'\r' && self.peek(1) == b'\n' {
                        self.pos += 1;
                    }
                    self.pos += 1;
                    if self.pending_heredocs.is_empty() {
                        self.tokens.push(Token::Operator(";".into()));
                    } else {
                        self.consume_heredoc_bodies();
                    }
                }
                b'#' => {
                    while self.pos < len
                        && self.input[self.pos] != b'\n'
                        && self.input[self.pos] != b'\r'
                    {
                        self.pos += 1;
                    }
                }
                b'|' if self.peek(1) == b'|' => {
                    self.tokens.push(Token::Operator("||".into()));
                    self.pos += 2;
                }
                b'&' if self.peek(1) == b'&' => {
                    self.tokens.push(Token::Operator("&&".into()));
                    self.pos += 2;
                }
                b'|' => {
                    self.tokens.push(Token::Operator("|".into()));
                    self.pos += 1;
                }
                b';' => {
                    self.tokens.push(Token::Operator(";".into()));
                    self.pos += 1;
                }
                b'&' => {
                    self.tokens.push(Token::Operator("&".into()));
                    self.pos += 1;
                }
                b'(' => {
                    let g = self.read_subshell()?;
                    self.tokens.push(g);
                }
                b'$' if self.peek(1) == b'(' => {
                    let g = self.read_substitution()?;
                    self.tokens.push(g);
                }
                b'`' => {
                    let g = self.read_backtick()?;
                    self.tokens.push(g);
                }
                b'$' => {
                    self.skip_variable();
                }
                _ if self.try_redirect() => {}
                _ => {
                    let word = self.read_word()?;
                    if !word.is_empty() {
                        self.tokens.push(Token::Word(word));
                    } else {
                        // No progress — the character is unparseable.
                        // Fail closed rather than spin forever.
                        return Err(format!(
                            "unexpected character: {:?}",
                            self.input[self.pos] as char
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn skip_variable(&mut self) {
        self.pos += 1;
        if self.pos < self.input.len() && self.input[self.pos] == b'{' {
            self.pos += 1;
            while self.pos < self.input.len() && self.input[self.pos] != b'}' {
                self.pos += 1;
            }
            if self.pos < self.input.len() {
                self.pos += 1;
            }
        } else {
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_alphanumeric() {
                self.pos += 1;
            }
        }
    }

    fn read_single_quoted(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut val = String::new();
        while self.pos < self.input.len() && self.input[self.pos] != b'\'' {
            val.push(self.input[self.pos] as char);
            self.pos += 1;
        }
        if self.pos >= self.input.len() {
            return Err("Unmatched single quote".into());
        }
        self.pos += 1;
        Ok(val)
    }

    fn read_double_quoted(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut val = String::new();
        while self.pos < self.input.len() && self.input[self.pos] != b'"' {
            let ch = self.input[self.pos];
            if ch == b'\\' {
                self.pos += 1;
                if self.pos < self.input.len() {
                    val.push(self.input[self.pos] as char);
                    self.pos += 1;
                }
                continue;
            }
            if ch == b'$' && self.peek(1) == b'(' {
                self.pos += 2;
                let mut depth = 1;
                let mut inner = String::new();
                while self.pos < self.input.len() && depth > 0 {
                    if self.input[self.pos] == b'(' {
                        depth += 1;
                    } else if self.input[self.pos] == b')' {
                        depth -= 1;
                        if depth == 0 {
                            self.pos += 1;
                            break;
                        }
                    }
                    inner.push(self.input[self.pos] as char);
                    self.pos += 1;
                }
                if depth != 0 {
                    return Err("Unmatched $( inside double quote".into());
                }
                // Tokenize the inner command so the policy engine
                // evaluates it — the raw text is also kept in the word
                // for matching, but without this the substitution hides
                // inside a quoted word and bypasses policy.
                if let Ok(inner_tokens) = tokenize(&inner) {
                    self.tokens.push(Token::Group {
                        tokens: inner_tokens,
                        kind: GroupKind::Substitution,
                    });
                }
                val.push_str(&inner);
                continue;
            }
            if ch == b'`' {
                self.pos += 1;
                let mut inner = String::new();
                while self.pos < self.input.len() && self.input[self.pos] != b'`' {
                    if self.input[self.pos] == b'\\' {
                        self.pos += 1;
                        if self.pos < self.input.len() {
                            inner.push(self.input[self.pos] as char);
                            self.pos += 1;
                        }
                        continue;
                    }
                    inner.push(self.input[self.pos] as char);
                    self.pos += 1;
                }
                if self.pos >= self.input.len() {
                    return Err("Unmatched backtick inside double quote".into());
                }
                self.pos += 1;
                val.push_str(&inner);
                continue;
            }
            val.push(ch as char);
            self.pos += 1;
        }
        if self.pos >= self.input.len() {
            return Err("Unmatched double quote".into());
        }
        self.pos += 1;
        Ok(val)
    }

    fn read_subshell(&mut self) -> Result<Token, String> {
        self.pos += 1;
        let (inner, ok) = self.read_balanced(b')');
        if !ok {
            return Err("Unmatched (".into());
        }
        Ok(Token::Group {
            tokens: tokenize(&inner)?,
            kind: GroupKind::Subshell,
        })
    }

    fn read_substitution(&mut self) -> Result<Token, String> {
        self.pos += 2;
        let (inner, ok) = self.read_balanced(b')');
        if !ok {
            return Err("Unmatched $(".into());
        }
        Ok(Token::Group {
            tokens: tokenize(&inner)?,
            kind: GroupKind::Substitution,
        })
    }

    fn read_backtick(&mut self) -> Result<Token, String> {
        self.pos += 1;
        let mut inner = String::new();
        while self.pos < self.input.len() && self.input[self.pos] != b'`' {
            if self.input[self.pos] == b'\\' {
                self.pos += 1;
                if self.pos < self.input.len() {
                    inner.push(self.input[self.pos] as char);
                    self.pos += 1;
                }
                continue;
            }
            inner.push(self.input[self.pos] as char);
            self.pos += 1;
        }
        if self.pos >= self.input.len() {
            return Err("Unmatched backtick".into());
        }
        self.pos += 1;
        Ok(Token::Group {
            tokens: tokenize(&inner)?,
            kind: GroupKind::Backtick,
        })
    }

    /// Read until the matching closing paren, tracking nesting.
    fn read_balanced(&mut self, close: u8) -> (String, bool) {
        let mut depth = 1;
        let mut inner = String::new();
        while self.pos < self.input.len() && depth > 0 {
            if self.input[self.pos] == b'(' {
                depth += 1;
            } else if self.input[self.pos] == close {
                depth -= 1;
                if depth == 0 {
                    self.pos += 1;
                    break;
                }
            }
            inner.push(self.input[self.pos] as char);
            self.pos += 1;
        }
        (inner, depth == 0)
    }

    fn read_word(&mut self) -> Result<String, String> {
        let mut val = String::new();
        let len = self.input.len();
        while self.pos < len {
            let ch = self.input[self.pos];
            #[allow(clippy::match_same_arms)] // break arms differ by guard
            match ch {
                b' ' | b'\t' | b'\n' | b'\r' | b'|' | b'&' | b';' | b'`' => break,
                b'\'' => val.push_str(&self.read_single_quoted()?),
                b'"' => val.push_str(&self.read_double_quoted()?),
                b'#' if val.is_empty() => break,
                b'(' if val.is_empty() => break,
                b'$' if self.peek(1) == b'(' => break,
                b'\\' => {
                    self.pos += 1;
                    if self.pos < len && self.input[self.pos] == b'\n' {
                        self.pos += 1;
                        continue;
                    }
                    if self.pos < len {
                        val.push(self.input[self.pos] as char);
                        self.pos += 1;
                    }
                }
                b'>' | b'<' => break,
                _ => {
                    val.push(ch as char);
                    self.pos += 1;
                }
            }
        }
        Ok(val)
    }

    fn try_redirect(&mut self) -> bool {
        let len = self.input.len();
        let ch = self.input[self.pos];
        let mut ri = self.pos;
        let mut fd_prefix = String::new();
        if ch.is_ascii_digit()
            && !self.peek(1).is_ascii_digit()
            && (self.peek(1) == b'>' || self.peek(1) == b'<')
        {
            fd_prefix.push(ch as char);
            ri = self.pos + 1;
        }
        let rch = if ri < len {
            self.input[ri]
        } else {
            return false;
        };
        if rch != b'<' && rch != b'>' {
            return false;
        }
        // Process substitution: <(cmd) or >(cmd). Tokenize the inner
        // command as a substitution group so the policy engine evaluates
        // it. Without this, <( hangs the tokenizer (the redirect guard
        // returns false, read_word breaks on '<' without advancing, and
        // the main loop spins forever).
        if self.peek_from(ri, 1) == b'(' {
            self.pos = ri + 1;
            let (inner, ok) = self.read_balanced(b')');
            if !ok {
                return false;
            }
            match tokenize(&inner) {
                Ok(tokens) => {
                    self.tokens.push(Token::Group {
                        tokens,
                        kind: GroupKind::Substitution,
                    });
                }
                Err(_) => return false,
            }
            return true;
        }
        // Here-string: <<<
        if rch == b'<' && self.peek_from(ri, 1) == b'<' && self.peek_from(ri, 2) == b'<' {
            self.pos = ri + 3;
            let target = self.read_redirect_target();
            self.tokens.push(Token::Redirect {
                op: format!("{fd_prefix}<<<"),
                target,
            });
            return true;
        }
        // Heredoc: << or <<-
        if rch == b'<' && self.peek_from(ri, 1) == b'<' && self.peek_from(ri, 2) != b'<' {
            self.pos = ri + 2;
            let mut strip = false;
            if self.pos < len && self.input[self.pos] == b'-' {
                strip = true;
                self.pos += 1;
            }
            while self.pos < len && (self.input[self.pos] == b' ' || self.input[self.pos] == b'\t')
            {
                self.pos += 1;
            }
            let delim = self.read_redirect_target();
            let op = format!("{fd_prefix}{}", if strip { "<<-" } else { "<<" });
            self.tokens.push(Token::Redirect {
                op,
                target: delim.clone(),
            });
            self.pending_heredocs.push((delim, strip));
            return true;
        }
        // Output: > or >>
        if rch == b'>' {
            self.pos = ri + 1;
            let mut op = format!("{fd_prefix}>");
            if self.pos < len && self.input[self.pos] == b'>' {
                op.push('>');
                self.pos += 1;
            }
            if self.pos < len && self.input[self.pos] == b'&' {
                self.pos += 1;
                let mut fd = String::new();
                while self.pos < len && self.input[self.pos].is_ascii_digit() {
                    fd.push(self.input[self.pos] as char);
                    self.pos += 1;
                }
                self.tokens.push(Token::Redirect {
                    op: format!("{op}&"),
                    target: if fd.is_empty() { "-".into() } else { fd },
                });
                return true;
            }
            let target = self.read_redirect_target();
            self.tokens.push(Token::Redirect { op, target });
            return true;
        }
        // Input: <
        if rch == b'<' && self.peek_from(ri, 1) != b'(' {
            self.pos = ri + 1;
            let target = self.read_redirect_target();
            self.tokens.push(Token::Redirect {
                op: format!("{fd_prefix}<"),
                target,
            });
            return true;
        }
        false
    }

    fn read_redirect_target(&mut self) -> String {
        let len = self.input.len();
        while self.pos < len && (self.input[self.pos] == b' ' || self.input[self.pos] == b'\t') {
            self.pos += 1;
        }
        if self.pos >= len || self.input[self.pos] == b'\n' || self.input[self.pos] == b'\r' {
            return String::new();
        }
        if self.input[self.pos] == b'\'' {
            return self.read_single_quoted().unwrap_or_default();
        }
        if self.input[self.pos] == b'"' {
            return self.read_double_quoted().unwrap_or_default();
        }
        let mut target = String::new();
        while self.pos < len {
            let ch = self.input[self.pos];
            if matches!(
                ch,
                b' ' | b'\t' | b'\n' | b'\r' | b'|' | b';' | b'&' | b')' | b'#'
            ) {
                break;
            }
            if ch == b'\\' {
                self.pos += 1;
                if self.pos < len {
                    target.push(self.input[self.pos] as char);
                    self.pos += 1;
                }
                continue;
            }
            target.push(ch as char);
            self.pos += 1;
        }
        target
    }

    fn consume_heredoc_bodies(&mut self) {
        let len = self.input.len();
        for (delimiter, strip) in std::mem::take(&mut self.pending_heredocs) {
            while self.pos < len {
                let line_start = self.pos;
                while self.pos < len && self.input[self.pos] != b'\n' {
                    self.pos += 1;
                }
                let mut line = std::str::from_utf8(&self.input[line_start..self.pos])
                    .unwrap_or("")
                    .to_string();
                if self.pos < len {
                    self.pos += 1;
                }
                if strip {
                    line = line.trim_start_matches('\t').to_string();
                }
                if line == delimiter {
                    break;
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn words(tokens: &[Token]) -> Vec<String> {
        tokens
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w.clone()),
                _ => None,
            })
            .collect()
    }
    fn ops(tokens: &[Token]) -> Vec<String> {
        tokens
            .iter()
            .filter_map(|t| match t {
                Token::Operator(o) => Some(o.clone()),
                _ => None,
            })
            .collect()
    }
    fn rds(tokens: &[Token]) -> Vec<(String, String)> {
        tokens
            .iter()
            .filter_map(|t| match t {
                Token::Redirect { op, target } => Some((op.clone(), target.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn basic_word() {
        let t = tokenize("ls").unwrap();
        assert_eq!(words(&t), vec!["ls"]);
    }
    #[test]
    fn multiple_words() {
        let t = tokenize("echo hello world").unwrap();
        assert_eq!(words(&t), vec!["echo", "hello", "world"]);
    }
    #[test]
    fn single_quotes() {
        let t = tokenize("echo 'hello world'").unwrap();
        assert_eq!(words(&t), vec!["echo", "hello world"]);
    }
    #[test]
    fn double_quotes() {
        let t = tokenize("echo \"hello world\"").unwrap();
        assert_eq!(words(&t), vec!["echo", "hello world"]);
    }
    #[test]
    fn escaped_dollar() {
        let t = tokenize("echo \\$VAR").unwrap();
        assert_eq!(words(&t), vec!["echo", "$VAR"]);
    }
    #[test]
    fn pipe() {
        let t = tokenize("ls | grep foo").unwrap();
        assert_eq!(ops(&t), vec!["|"]);
    }
    #[test]
    fn and_or() {
        let t = tokenize("true && false || echo nope").unwrap();
        assert_eq!(ops(&t), vec!["&&", "||"]);
    }
    #[test]
    fn semicolon() {
        let t = tokenize("cd foo; ls").unwrap();
        assert_eq!(ops(&t), vec![";"]);
    }
    #[test]
    fn output_redirect() {
        let t = tokenize("echo hi > file.txt").unwrap();
        assert_eq!(rds(&t), vec![(">".into(), "file.txt".into())]);
    }
    #[test]
    fn append_redirect() {
        let t = tokenize("echo hi >> file.txt").unwrap();
        assert_eq!(rds(&t), vec![(">>".into(), "file.txt".into())]);
    }
    #[test]
    fn fd_redirect() {
        let t = tokenize("cmd 2> err.txt").unwrap();
        assert_eq!(rds(&t), vec![("2>".into(), "err.txt".into())]);
    }
    #[test]
    fn fd_dup() {
        let t = tokenize("cmd 2>&1").unwrap();
        assert_eq!(rds(&t), vec![("2>&".into(), "1".into())]);
    }
    #[test]
    fn input_redirect() {
        let t = tokenize("cat < file.txt").unwrap();
        assert_eq!(rds(&t), vec![("<".into(), "file.txt".into())]);
    }
    #[test]
    fn heredoc() {
        let t = tokenize("cat <<EOF\nhello\nEOF").unwrap();
        assert_eq!(rds(&t), vec![("<<".into(), "EOF".into())]);
    }
    #[test]
    fn here_string() {
        let t = tokenize("cat <<< \"hello\"").unwrap();
        assert_eq!(rds(&t), vec![("<<<".into(), "hello".into())]);
    }
    #[test]
    fn subshell() {
        let t = tokenize("(echo hi)").unwrap();
        match &t[0] {
            Token::Group { tokens, kind } => {
                assert_eq!(*kind, GroupKind::Subshell);
                assert_eq!(words(tokens), vec!["echo", "hi"]);
            }
            other => panic!("expected group, got {other:?}"),
        }
    }
    #[test]
    fn cmd_substitution() {
        let t = tokenize("echo $(date)").unwrap();
        let g: Vec<_> = t
            .iter()
            .filter_map(|tk| match tk {
                Token::Group { kind, .. } => Some(kind.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(g, vec![GroupKind::Substitution]);
    }
    #[test]
    fn backtick_sub() {
        let t = tokenize("echo `date`").unwrap();
        let g: Vec<_> = t
            .iter()
            .filter_map(|tk| match tk {
                Token::Group { kind, .. } => Some(kind.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(g, vec![GroupKind::Backtick]);
    }
    #[test]
    fn variable_skipped() {
        let t = tokenize("echo $VAR").unwrap();
        assert_eq!(words(&t), vec!["echo"]);
    }
    #[test]
    fn braced_var_skipped() {
        let t = tokenize("echo ${VAR}").unwrap();
        assert_eq!(words(&t), vec!["echo"]);
    }
    #[test]
    fn comment_ignored() {
        let t = tokenize("echo hi # comment").unwrap();
        assert_eq!(words(&t), vec!["echo", "hi"]);
    }
    #[test]
    fn unclosed_quote() {
        assert!(tokenize("echo 'unclosed").is_err());
        assert!(tokenize("echo \"unclosed").is_err());
    }
    #[test]
    fn unclosed_subshell() {
        assert!(tokenize("(echo hi").is_err());
    }
    #[test]
    fn complex_pipeline() {
        let t = tokenize("cat file | grep foo | sort | uniq -c").unwrap();
        assert_eq!(ops(&t), vec!["|", "|", "|"]);
    }
}
