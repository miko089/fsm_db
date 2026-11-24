use crate::types::DbError;

#[derive(Debug, PartialEq)]
pub(crate) enum Token {
    Keyword(String),
    Ident(String),
    IntLit(i64),
    FloatLit(f64),
    StringLit(String),
    LParen,    // (
    RParen,    // )
    Comma,     // ,
    Semicolon, // ;
    Plus,      // +
    Minus,     // -
    Star,      // *
    Slash,     // /
    Eq,        // =
    Neq,       // !=
    Lt,        // <
    Le,        // <=
    Gt,        // >
    Ge,        // >=
    Arrow,     // ->
}

const KEYWORDS: &[&str] = &[
    "CREATE",
    "TABLE",
    "INTEGER",
    "FLOAT",
    "BOOL",
    "STRING",
    "FSM",
    "STATES",
    "INITIAL",
    "TRANSITIONS",
    "WHEN",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "INSERT",
    "INTO",
    "VALUES",
    "SELECT",
    "FROM",
    "WHERE",
    "LIMIT",
    "UPDATE",
    "SET",
    "DELETE",
    "AND",
    "OR",
    "NOT",
    "IS",
    "NULL",
    "TRUE",
    "FALSE",
    "NOW",
];

fn is_keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|kw| kw.eq_ignore_ascii_case(word))
}

pub(crate) fn tokenize(input: &str) -> Result<Vec<Token>, DbError> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();

    while let Some(&(idx, ch)) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }

        match ch {
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            ',' => {
                chars.next();
                tokens.push(Token::Comma);
            }
            ';' => {
                chars.next();
                tokens.push(Token::Semicolon);
            }
            '+' => {
                chars.next();
                tokens.push(Token::Plus);
            }
            '*' => {
                chars.next();
                tokens.push(Token::Star);
            }
            '/' => {
                chars.next();
                tokens.push(Token::Slash);
            }
            '=' => {
                chars.next();
                tokens.push(Token::Eq);
            }
            '-' => {
                chars.next();
                if let Some(&(_, '>')) = chars.peek() {
                    chars.next();
                    tokens.push(Token::Arrow);
                } else {
                    tokens.push(Token::Minus);
                }
            }
            '!' => {
                chars.next();
                if let Some(&(_, '=')) = chars.peek() {
                    chars.next();
                    tokens.push(Token::Neq);
                } else {
                    return Err(DbError::LexError("unexpected '!'".to_string()));
                }
            }
            '<' => {
                chars.next();
                if let Some(&(_, '=')) = chars.peek() {
                    chars.next();
                    tokens.push(Token::Le);
                } else {
                    tokens.push(Token::Lt);
                }
            }
            '>' => {
                chars.next();
                if let Some(&(_, '=')) = chars.peek() {
                    chars.next();
                    tokens.push(Token::Ge);
                } else {
                    tokens.push(Token::Gt);
                }
            }
            '\'' => {
                chars.next();
                let mut s = String::new();
                let mut closed = false;
                while let Some((_, c)) = chars.next() {
                    if c == '\'' {
                        if let Some(&(_, '\'')) = chars.peek() {
                            chars.next();
                            s.push('\'');
                        } else {
                            closed = true;
                            break;
                        }
                    } else {
                        s.push(c);
                    }
                }
                if !closed {
                    return Err(DbError::LexError("unterminated string literal".to_string()));
                }
                tokens.push(Token::StringLit(s));
            }
            c if c.is_ascii_digit() => {
                let start = idx;
                let mut end = idx + c.len_utf8();
                chars.next();
                while let Some(&(next_idx, next_ch)) = chars.peek() {
                    if next_ch.is_ascii_digit() {
                        end = next_idx + next_ch.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }

                let mut is_float = false;
                if let Some(&(_, '.')) = chars.peek() {
                    // check if '.' is followed by an ASCII digit
                    let after_dot = &input[end + 1..];
                    if after_dot.chars().next().is_some_and(|d| d.is_ascii_digit()) {
                        is_float = true;
                        chars.next(); // consume '.'
                        end += 1;
                        while let Some(&(next_idx, next_ch)) = chars.peek() {
                            if next_ch.is_ascii_digit() {
                                end = next_idx + next_ch.len_utf8();
                                chars.next();
                            } else {
                                break;
                            }
                        }
                    }
                }

                let raw = &input[start..end];
                if is_float {
                    let val = raw
                        .parse::<f64>()
                        .map_err(|e| DbError::LexError(format!("invalid float '{raw}': {e}")))?;
                    tokens.push(Token::FloatLit(val));
                } else {
                    let val = raw
                        .parse::<i64>()
                        .map_err(|e| DbError::LexError(format!("invalid integer '{raw}': {e}")))?;
                    tokens.push(Token::IntLit(val));
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = idx;
                let mut end = idx + c.len_utf8();
                chars.next();
                while let Some(&(next_idx, next_ch)) = chars.peek() {
                    if next_ch.is_ascii_alphanumeric() || next_ch == '_' {
                        end = next_idx + next_ch.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                let word = &input[start..end];
                if is_keyword(word) {
                    tokens.push(Token::Keyword(word.to_string()));
                } else {
                    tokens.push(Token::Ident(word.to_string()));
                }
            }
            c => {
                return Err(DbError::LexError(format!("unexpected character: '{c}'")));
            }
        }
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kw(s: &str) -> Token {
        Token::Keyword(s.to_string())
    }

    fn ident(s: &str) -> Token {
        Token::Ident(s.to_string())
    }

    fn str_lit(s: &str) -> Token {
        Token::StringLit(s.to_string())
    }

    #[test]
    fn tokenize_example_sql() {
        use Token::*;

        let sql = include_str!("../docs/example.sql");
        let tokens = tokenize(sql).unwrap();

        let expected = vec![
            // CREATE TABLE tasks (...)
            kw("CREATE"),
            kw("TABLE"),
            ident("tasks"),
            LParen,
            ident("retries"),
            kw("INTEGER"),
            Comma,
            ident("done"),
            kw("BOOL"),
            Comma,
            ident("title"),
            kw("STRING"),
            Comma,
            ident("state"),
            kw("FSM"),
            LParen,
            kw("STATES"),
            LParen,
            str_lit("pending"),
            Comma,
            str_lit("running"),
            Comma,
            str_lit("failed"),
            Comma,
            str_lit("done"),
            RParen,
            Comma,
            kw("INITIAL"),
            str_lit("pending"),
            Comma,
            kw("TRANSITIONS"),
            LParen,
            str_lit("pending"),
            Arrow,
            str_lit("running"),
            Comma,
            str_lit("running"),
            Arrow,
            str_lit("done"),
            Comma,
            str_lit("running"),
            Arrow,
            str_lit("failed"),
            kw("WHEN"),
            ident("retries"),
            Ge,
            IntLit(3),
            RParen,
            RParen,
            RParen,
            Semicolon,
            // BEGIN; COMMIT; BEGIN; ROLLBACK;
            kw("BEGIN"),
            Semicolon,
            kw("COMMIT"),
            Semicolon,
            kw("BEGIN"),
            Semicolon,
            kw("ROLLBACK"),
            Semicolon,
            // INSERT INTO tasks VALUES (0, false, 'init', 'pending');
            kw("INSERT"),
            kw("INTO"),
            ident("tasks"),
            kw("VALUES"),
            LParen,
            IntLit(0),
            Comma,
            kw("false"),
            Comma,
            str_lit("init"),
            Comma,
            str_lit("pending"),
            RParen,
            Semicolon,
            // SELECT retries, state FROM tasks WHERE retries >= 1 LIMIT 10;
            kw("SELECT"),
            ident("retries"),
            Comma,
            ident("state"),
            kw("FROM"),
            ident("tasks"),
            kw("WHERE"),
            ident("retries"),
            Ge,
            IntLit(1),
            kw("LIMIT"),
            IntLit(10),
            Semicolon,
            // SELECT * FROM tasks LIMIT 10;
            kw("SELECT"),
            Star,
            kw("FROM"),
            ident("tasks"),
            kw("LIMIT"),
            IntLit(10),
            Semicolon,
            // SELECT 'current time', NOW, 2 * 8;
            kw("SELECT"),
            str_lit("current time"),
            Comma,
            kw("NOW"),
            Comma,
            IntLit(2),
            Star,
            IntLit(8),
            Semicolon,
            // UPDATE tasks SET retries = retries + 1, state = 'failed' WHERE state = 'running';
            kw("UPDATE"),
            ident("tasks"),
            kw("SET"),
            ident("retries"),
            Eq,
            ident("retries"),
            Plus,
            IntLit(1),
            Comma,
            ident("state"),
            Eq,
            str_lit("failed"),
            kw("WHERE"),
            ident("state"),
            Eq,
            str_lit("running"),
            Semicolon,
            // DELETE FROM tasks WHERE state = 'done';
            kw("DELETE"),
            kw("FROM"),
            ident("tasks"),
            kw("WHERE"),
            ident("state"),
            Eq,
            str_lit("done"),
            Semicolon,
        ];

        assert_eq!(tokens, expected);
    }

    #[test]
    fn tokenize_operators_and_literals() {
        use Token::*;

        let tokens = tokenize("a != 1.5 + b - c / 2 < <= > >= -> 'it''s ok'").unwrap();
        assert_eq!(
            tokens,
            vec![
                ident("a"),
                Neq,
                FloatLit(1.5),
                Plus,
                ident("b"),
                Minus,
                ident("c"),
                Slash,
                IntLit(2),
                Lt,
                Le,
                Gt,
                Ge,
                Arrow,
                str_lit("it's ok"),
            ]
        );
    }

    #[test]
    fn tokenize_errors() {
        assert!(matches!(
            tokenize("'unterminated"),
            Err(DbError::LexError(_))
        ));
        assert!(matches!(tokenize("!"), Err(DbError::LexError(_))));
        assert!(matches!(tokenize("@"), Err(DbError::LexError(_))));
        assert!(matches!(
            tokenize("99999999999999999999999999999"),
            Err(DbError::LexError(_))
        ));
    }
}
