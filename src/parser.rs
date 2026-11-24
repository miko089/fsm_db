use crate::{
    expr::BinaryOp,
    lexer::Token,
    types::{DbError, Value},
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    Begin,
    Commit,
    Rollback,
    Insert {
        table: String,
        values: Vec<AstExpr>,
    },
    Select {
        items: Vec<SelectItem>,
        from: Option<String>,
        filter: Option<AstExpr>,
        limit: Option<usize>,
    },
    Update {
        table: String,
        sets: Vec<(String, AstExpr)>,
        filter: Option<AstExpr>,
    },
    Delete {
        table: String,
        filter: Option<AstExpr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectItem {
    Wildcard,
    Expr(AstExpr),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnDef {
    pub(crate) name: String,
    pub(crate) col_type: ColumnTypeAst,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ColumnTypeAst {
    Integer,
    Float,
    Bool,
    String,
    Fsm(FsmDef),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FsmDef {
    pub(crate) states: Vec<String>,
    pub(crate) initial: String,
    pub(crate) transitions: Vec<TransitionDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TransitionDef {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) guard: Option<AstExpr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AstExpr {
    Literal(Value),
    Ident(String),
    CurrentTime,
    IsNull(Box<AstExpr>),
    Not(Box<AstExpr>),
    Neg(Box<AstExpr>),
    Binary {
        op: BinaryOp,
        left: Box<AstExpr>,
        right: Box<AstExpr>,
    },
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&'a Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn error(&self, msg: impl Into<String>) -> DbError {
        DbError::ParseError(msg.into())
    }

    fn peek_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Keyword(k)) if k.eq_ignore_ascii_case(kw))
    }

    fn consume_keyword(&mut self, kw: &str) -> bool {
        if self.peek_keyword(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), DbError> {
        if self.consume_keyword(kw) {
            Ok(())
        } else {
            Err(self.error(format!("expected keyword '{kw}', found {:?}", self.peek())))
        }
    }

    fn expect_token(&mut self, expected: &Token) -> Result<(), DbError> {
        match self.peek() {
            Some(tok) if tok == expected => {
                self.pos += 1;
                Ok(())
            }
            other => Err(self.error(format!("expected {expected:?}, found {other:?}"))),
        }
    }

    fn expect_ident(&mut self) -> Result<String, DbError> {
        match self.advance() {
            Some(Token::Ident(name)) => Ok(name.clone()),
            other => Err(self.error(format!("expected identifier, found {other:?}"))),
        }
    }

    fn expect_string(&mut self) -> Result<String, DbError> {
        match self.advance() {
            Some(Token::StringLit(s)) => Ok(s.clone()),
            other => Err(self.error(format!("expected string literal, found {other:?}"))),
        }
    }

    fn parse_comma_separated<T>(
        &mut self,
        mut parse_item: impl FnMut(&mut Self) -> Result<T, DbError>,
    ) -> Result<Vec<T>, DbError> {
        let mut items = vec![parse_item(self)?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.advance();
            items.push(parse_item(self)?);
        }
        Ok(items)
    }

    fn parse_statement(&mut self) -> Result<Statement, DbError> {
        if self.consume_keyword("BEGIN") {
            Ok(Statement::Begin)
        } else if self.consume_keyword("COMMIT") {
            Ok(Statement::Commit)
        } else if self.consume_keyword("ROLLBACK") {
            Ok(Statement::Rollback)
        } else if self.consume_keyword("CREATE") {
            self.parse_create_table()
        } else if self.consume_keyword("INSERT") {
            self.parse_insert()
        } else if self.consume_keyword("SELECT") {
            self.parse_select()
        } else if self.consume_keyword("UPDATE") {
            self.parse_update()
        } else if self.consume_keyword("DELETE") {
            self.parse_delete()
        } else {
            Err(self.error(format!(
                "unexpected token at start of statement: {:?}",
                self.peek()
            )))
        }
    }

    fn parse_create_table(&mut self) -> Result<Statement, DbError> {
        self.expect_keyword("TABLE")?;
        let name = self.expect_ident()?;
        self.expect_token(&Token::LParen)?;
        let columns = self.parse_comma_separated(Self::parse_column_def)?;
        self.expect_token(&Token::RParen)?;
        Ok(Statement::CreateTable { name, columns })
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef, DbError> {
        let name = self.expect_ident()?;
        let col_type = if self.consume_keyword("INTEGER") {
            ColumnTypeAst::Integer
        } else if self.consume_keyword("FLOAT") {
            ColumnTypeAst::Float
        } else if self.consume_keyword("BOOL") {
            ColumnTypeAst::Bool
        } else if self.consume_keyword("STRING") {
            ColumnTypeAst::String
        } else if self.consume_keyword("FSM") {
            ColumnTypeAst::Fsm(self.parse_fsm_def()?)
        } else {
            return Err(self.error(format!("expected column type, found {:?}", self.peek())));
        };
        Ok(ColumnDef { name, col_type })
    }

    fn parse_fsm_def(&mut self) -> Result<FsmDef, DbError> {
        self.expect_token(&Token::LParen)?;
        let mut states: Option<Vec<String>> = None;
        let mut initial: Option<String> = None;
        let mut transitions: Option<Vec<TransitionDef>> = None;

        loop {
            if self.consume_keyword("STATES") {
                if states.is_some() {
                    return Err(self.error("duplicate STATES clause in FSM definition"));
                }
                self.expect_token(&Token::LParen)?;
                let parsed = self.parse_comma_separated(Self::expect_string)?;
                self.expect_token(&Token::RParen)?;
                states = Some(parsed);
            } else if self.consume_keyword("INITIAL") {
                if initial.is_some() {
                    return Err(self.error("duplicate INITIAL clause in FSM definition"));
                }
                initial = Some(self.expect_string()?);
            } else if self.consume_keyword("TRANSITIONS") {
                if transitions.is_some() {
                    return Err(self.error("duplicate TRANSITIONS clause in FSM definition"));
                }
                self.expect_token(&Token::LParen)?;
                let parsed = if matches!(self.peek(), Some(Token::RParen)) {
                    Vec::new()
                } else {
                    self.parse_comma_separated(Self::parse_transition_def)?
                };
                self.expect_token(&Token::RParen)?;
                transitions = Some(parsed);
            } else {
                return Err(self.error(format!(
                    "expected STATES, INITIAL, or TRANSITIONS in FSM definition, found {:?}",
                    self.peek()
                )));
            }

            if matches!(self.peek(), Some(Token::Comma)) {
                self.advance();
            } else {
                break;
            }
        }

        self.expect_token(&Token::RParen)?;

        let states = states.ok_or_else(|| self.error("missing STATES clause in FSM definition"))?;
        let initial =
            initial.ok_or_else(|| self.error("missing INITIAL clause in FSM definition"))?;
        Ok(FsmDef {
            states,
            initial,
            transitions: transitions.unwrap_or_default(),
        })
    }

    fn parse_transition_def(&mut self) -> Result<TransitionDef, DbError> {
        let from = self.expect_string()?;
        self.expect_token(&Token::Arrow)?;
        let to = self.expect_string()?;
        let guard = if self.consume_keyword("WHEN") {
            Some(self.parse_or()?)
        } else {
            None
        };
        Ok(TransitionDef { from, to, guard })
    }

    fn parse_insert(&mut self) -> Result<Statement, DbError> {
        self.expect_keyword("INTO")?;
        let table = self.expect_ident()?;
        self.expect_keyword("VALUES")?;
        self.expect_token(&Token::LParen)?;
        let values = self.parse_comma_separated(Self::parse_or)?;
        self.expect_token(&Token::RParen)?;
        Ok(Statement::Insert { table, values })
    }

    fn parse_select(&mut self) -> Result<Statement, DbError> {
        let items = self.parse_comma_separated(|p| {
            if matches!(p.peek(), Some(Token::Star)) {
                p.advance();
                Ok(SelectItem::Wildcard)
            } else {
                Ok(SelectItem::Expr(p.parse_or()?))
            }
        })?;

        let from = if self.consume_keyword("FROM") {
            Some(self.expect_ident()?)
        } else {
            None
        };

        let filter = if self.consume_keyword("WHERE") {
            Some(self.parse_or()?)
        } else {
            None
        };

        let limit = if self.consume_keyword("LIMIT") {
            match self.advance() {
                Some(Token::IntLit(n)) if *n >= 0 => Some(*n as usize),
                other => {
                    return Err(self.error(format!(
                        "expected non-negative integer for LIMIT, found {other:?}"
                    )));
                }
            }
        } else {
            None
        };

        Ok(Statement::Select {
            items,
            from,
            filter,
            limit,
        })
    }

    fn parse_update(&mut self) -> Result<Statement, DbError> {
        let table = self.expect_ident()?;
        self.expect_keyword("SET")?;
        let sets = self.parse_comma_separated(|p| {
            let col = p.expect_ident()?;
            p.expect_token(&Token::Eq)?;
            let val = p.parse_or()?;
            Ok((col, val))
        })?;
        let filter = if self.consume_keyword("WHERE") {
            Some(self.parse_or()?)
        } else {
            None
        };
        Ok(Statement::Update {
            table,
            sets,
            filter,
        })
    }

    fn parse_delete(&mut self) -> Result<Statement, DbError> {
        self.expect_keyword("FROM")?;
        let table = self.expect_ident()?;
        let filter = if self.consume_keyword("WHERE") {
            Some(self.parse_or()?)
        } else {
            None
        };
        Ok(Statement::Delete { table, filter })
    }

    fn parse_or(&mut self) -> Result<AstExpr, DbError> {
        let mut expr = self.parse_and()?;
        while self.consume_keyword("OR") {
            let right = self.parse_and()?;
            expr = AstExpr::Binary {
                op: BinaryOp::Or,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<AstExpr, DbError> {
        let mut expr = self.parse_not()?;
        while self.consume_keyword("AND") {
            let right = self.parse_not()?;
            expr = AstExpr::Binary {
                op: BinaryOp::And,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
        Ok(expr)
    }

    fn parse_not(&mut self) -> Result<AstExpr, DbError> {
        if self.consume_keyword("NOT") {
            let inner = self.parse_not()?;
            Ok(AstExpr::Not(Box::new(inner)))
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<AstExpr, DbError> {
        let expr = self.parse_term()?;
        if self.consume_keyword("IS") {
            let negated = self.consume_keyword("NOT");
            self.expect_keyword("NULL")?;
            let is_null = AstExpr::IsNull(Box::new(expr));
            return Ok(if negated {
                AstExpr::Not(Box::new(is_null))
            } else {
                is_null
            });
        }

        let op = match self.peek() {
            Some(Token::Eq) => Some(BinaryOp::Eq),
            Some(Token::Neq) => Some(BinaryOp::Ne),
            Some(Token::Lt) => Some(BinaryOp::Lt),
            Some(Token::Le) => Some(BinaryOp::Le),
            Some(Token::Gt) => Some(BinaryOp::Gt),
            Some(Token::Ge) => Some(BinaryOp::Ge),
            _ => None,
        };

        if let Some(op) = op {
            self.advance();
            let right = self.parse_term()?;
            Ok(AstExpr::Binary {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            })
        } else {
            Ok(expr)
        }
    }

    fn parse_term(&mut self) -> Result<AstExpr, DbError> {
        let mut expr = self.parse_factor()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinaryOp::Add,
                Some(Token::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_factor()?;
            expr = AstExpr::Binary {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
        Ok(expr)
    }

    fn parse_factor(&mut self) -> Result<AstExpr, DbError> {
        let mut expr = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinaryOp::Mul,
                Some(Token::Slash) => BinaryOp::Div,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            expr = AstExpr::Binary {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<AstExpr, DbError> {
        if matches!(self.peek(), Some(Token::Minus)) {
            self.advance();
            let inner = self.parse_unary()?;
            Ok(AstExpr::Neg(Box::new(inner)))
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<AstExpr, DbError> {
        match self.advance() {
            Some(Token::IntLit(v)) => Ok(AstExpr::Literal(Value::Integer(*v))),
            Some(Token::FloatLit(v)) => Ok(AstExpr::Literal(Value::Float(*v))),
            Some(Token::StringLit(s)) => Ok(AstExpr::Literal(Value::String(s.clone()))),
            Some(Token::Ident(name)) => Ok(AstExpr::Ident(name.clone())),
            Some(Token::Keyword(kw)) if kw.eq_ignore_ascii_case("TRUE") => {
                Ok(AstExpr::Literal(Value::Bool(true)))
            }
            Some(Token::Keyword(kw)) if kw.eq_ignore_ascii_case("FALSE") => {
                Ok(AstExpr::Literal(Value::Bool(false)))
            }
            Some(Token::Keyword(kw)) if kw.eq_ignore_ascii_case("NULL") => {
                Ok(AstExpr::Literal(Value::Null))
            }
            Some(Token::Keyword(kw)) if kw.eq_ignore_ascii_case("NOW") => Ok(AstExpr::CurrentTime),
            Some(Token::LParen) => {
                let expr = self.parse_or()?;
                self.expect_token(&Token::RParen)?;
                Ok(expr)
            }
            other => Err(self.error(format!("expected expression, found {other:?}"))),
        }
    }
}

pub(crate) fn parse(tokens: &[Token]) -> Result<Vec<Statement>, DbError> {
    let mut parser = Parser::new(tokens);
    let mut stmts = Vec::new();
    while !parser.is_at_end() {
        stmts.push(parser.parse_statement()?);
        if !parser.is_at_end() {
            parser.expect_token(&Token::Semicolon)?;
        }
    }
    Ok(stmts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn ident(name: &str) -> AstExpr {
        AstExpr::Ident(name.to_string())
    }

    fn int(v: i64) -> AstExpr {
        AstExpr::Literal(Value::Integer(v))
    }

    fn bin(op: BinaryOp, left: AstExpr, right: AstExpr) -> AstExpr {
        AstExpr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    #[test]
    fn test_parse_example_sql() {
        let sql = include_str!("../docs/example.sql");
        let tokens = tokenize(sql).unwrap();
        let stmts = parse(&tokens).unwrap();

        assert_eq!(stmts.len(), 10);

        assert_eq!(
            stmts[0],
            Statement::CreateTable {
                name: "tasks".to_string(),
                columns: vec![
                    ColumnDef {
                        name: "retries".to_string(),
                        col_type: ColumnTypeAst::Integer,
                    },
                    ColumnDef {
                        name: "done".to_string(),
                        col_type: ColumnTypeAst::Bool,
                    },
                    ColumnDef {
                        name: "title".to_string(),
                        col_type: ColumnTypeAst::String,
                    },
                    ColumnDef {
                        name: "state".to_string(),
                        col_type: ColumnTypeAst::Fsm(FsmDef {
                            states: vec![
                                "pending".to_string(),
                                "running".to_string(),
                                "failed".to_string(),
                                "done".to_string(),
                            ],
                            initial: "pending".to_string(),
                            transitions: vec![
                                TransitionDef {
                                    from: "pending".to_string(),
                                    to: "running".to_string(),
                                    guard: None,
                                },
                                TransitionDef {
                                    from: "running".to_string(),
                                    to: "done".to_string(),
                                    guard: None,
                                },
                                TransitionDef {
                                    from: "running".to_string(),
                                    to: "failed".to_string(),
                                    guard: Some(bin(BinaryOp::Ge, ident("retries"), int(3))),
                                },
                            ],
                        }),
                    },
                ],
            }
        );

        assert_eq!(stmts[1], Statement::Begin);
        assert_eq!(stmts[2], Statement::Commit);
        assert_eq!(stmts[3], Statement::Rollback);

        assert_eq!(
            stmts[7],
            Statement::Select {
                items: vec![
                    SelectItem::Expr(AstExpr::Literal(Value::String("current time".to_string()))),
                    SelectItem::Expr(AstExpr::CurrentTime),
                    SelectItem::Expr(bin(BinaryOp::Mul, int(2), int(8))),
                ],
                from: None,
                filter: None,
                limit: None,
            }
        );

        assert_eq!(
            stmts[8],
            Statement::Update {
                table: "tasks".to_string(),
                sets: vec![
                    (
                        "retries".to_string(),
                        bin(BinaryOp::Add, ident("retries"), int(1))
                    ),
                    (
                        "state".to_string(),
                        AstExpr::Literal(Value::String("failed".to_string())),
                    ),
                ],
                filter: Some(bin(
                    BinaryOp::Eq,
                    ident("state"),
                    AstExpr::Literal(Value::String("running".to_string())),
                )),
            }
        );
    }

    #[test]
    fn test_expr_precedence_and_left_associativity() {
        let tokens = tokenize("a + b * c >= 10 AND NOT x IS NULL OR y = -5").unwrap();
        let expr = Parser::new(&tokens).parse_or().unwrap();

        let expected = bin(
            BinaryOp::Or,
            bin(
                BinaryOp::And,
                bin(
                    BinaryOp::Ge,
                    bin(
                        BinaryOp::Add,
                        ident("a"),
                        bin(BinaryOp::Mul, ident("b"), ident("c")),
                    ),
                    int(10),
                ),
                AstExpr::Not(Box::new(AstExpr::IsNull(Box::new(ident("x"))))),
            ),
            bin(BinaryOp::Eq, ident("y"), AstExpr::Neg(Box::new(int(5)))),
        );
        assert_eq!(expr, expected);

        let sub_tokens = tokenize("10 - 3 - 2").unwrap();
        let sub_expr = Parser::new(&sub_tokens).parse_or().unwrap();
        assert_eq!(
            sub_expr,
            bin(BinaryOp::Sub, bin(BinaryOp::Sub, int(10), int(3)), int(2))
        );
    }

    #[test]
    fn test_expr_parentheses_override_precedence() {
        let arith_tokens = tokenize("(a + b) * c").unwrap();
        let arith_expr = Parser::new(&arith_tokens).parse_or().unwrap();
        assert_eq!(
            arith_expr,
            bin(
                BinaryOp::Mul,
                bin(BinaryOp::Add, ident("a"), ident("b")),
                ident("c"),
            )
        );

        let logic_tokens = tokenize("(a OR b) AND c").unwrap();
        let logic_expr = Parser::new(&logic_tokens).parse_or().unwrap();
        assert_eq!(
            logic_expr,
            bin(
                BinaryOp::And,
                bin(BinaryOp::Or, ident("a"), ident("b")),
                ident("c"),
            )
        );
    }

    #[test]
    fn test_error_unclosed_paren() {
        let tokens = tokenize("(a + b").unwrap();
        assert!(Parser::new(&tokens).parse_or().is_err());

        let stmt_tokens = tokenize("SELECT (1 + 2;").unwrap();
        assert!(parse(&stmt_tokens).is_err());
    }

    #[test]
    fn test_error_trailing_token() {
        let tokens = tokenize("BEGIN extra;").unwrap();
        assert!(parse(&tokens).is_err());

        let select_tokens = tokenize("SELECT 1 2;").unwrap();
        assert!(parse(&select_tokens).is_err());
    }

    #[test]
    fn test_error_fsm_without_initial() {
        let sql = "CREATE TABLE t (s FSM (STATES ('a', 'b'), TRANSITIONS ('a' -> 'b')));";
        let tokens = tokenize(sql).unwrap();
        assert!(parse(&tokens).is_err());
    }

    #[test]
    fn test_error_negative_limit() {
        let tokens = tokenize("SELECT * FROM tasks LIMIT -1;").unwrap();
        assert!(parse(&tokens).is_err());
    }
}
