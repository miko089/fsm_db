#![allow(dead_code)]

use std::collections::HashMap;
use thiserror::Error;

pub(crate) use crate::expr::Expr;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Integer(i64),
    Float(f64),
    Fsm(u16),
    Bool(bool),
    String(String),
    Null,
}

pub(crate) struct Fsm {
    pub(crate) states: Vec<String>,
    pub(crate) initial: u16,
    pub(crate) allowed: HashMap<(u16, u16), Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColumnType {
    Integer,
    Float,
    Fsm(u32),
    Bool,
    String,
}

pub(crate) type Row = Vec<Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RowId {
    pub(crate) page_id: u32,
    pub(crate) row_id: u16,
}

#[derive(Error, Debug)]
pub enum DbError {
    #[error("no table with name {0}")]
    TableNotFound(String),

    #[error("table already exists: {0}")]
    TableAlreadyExists(String),

    #[error("invalid schema: {0}")]
    InvalidSchema(String),

    #[error("no row with {0:?}")]
    RowNotFound(RowId),

    #[error("evaluation error on: {0:?}")]
    EvalError(Expr),

    #[error("lex error: {0}")]
    LexError(String),

    #[error("parse error: {0}")]
    ParseError(String),

    #[error("bind error: {0}")]
    BindError(String),

    #[error("execution error: {0}")]
    ExecError(String),
}
