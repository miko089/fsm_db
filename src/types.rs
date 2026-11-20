#![allow(dead_code)]

use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Integer(i64),
    Float(f64),
    Fsm(u16),
    Bool(bool),
    String(String),
}

pub(crate) struct Expr {}

pub(crate) struct Fsm {
    states: Vec<String>,
    initial: u16,
    allowed: HashMap<(u16, u16), Expr>,
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

    #[error("no row with {0:?}")]
    RowNotFound(RowId),
}
