#![allow(dead_code)]

use std::collections::HashMap;
use thiserror::Error;

pub(crate) use crate::expr::Expr;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Integer(i64),
    Float(f64),
    Fsm(u16),
    Bool(bool),
    String(String),
    Null,
}

impl Value {
    pub(crate) fn tag(&self) -> ValueTag {
        match self {
            Value::Integer(_) => ValueTag::Integer,
            Value::Float(_) => ValueTag::Float,
            Value::Fsm(_) => ValueTag::Fsm,
            Value::Bool(_) => ValueTag::Bool,
            Value::String(_) => ValueTag::String,
            Value::Null => ValueTag::Null,
        }
    }
}

impl TryFrom<u8> for ValueTag {
    type Error = DbError;
    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            0 => Ok(Self::Integer),
            1 => Ok(Self::Float),
            2 => Ok(Self::Fsm),
            3 => Ok(Self::Bool),
            4 => Ok(Self::String),
            5 => Ok(Self::Null),
            _ => Err(DbError::ExecError(format!("unknown value tag: {byte}"))),
        }
    }
}

#[repr(u8)]
pub enum ValueTag {
    Integer = 0,
    Float = 1,
    Fsm = 2,
    Bool = 3,
    String = 4,
    Null = 5,
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

pub type Row = Vec<Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RowId {
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

    #[error("guard failed: {0}")]
    GuardFailed(String),

    #[error("lex error: {0}")]
    LexError(String),

    #[error("parse error: {0}")]
    ParseError(String),

    #[error("bind error: {0}")]
    BindError(String),

    #[error("execution error: {0}")]
    ExecError(String),

    #[error("error {0} row")]
    CodecError(String),
}
