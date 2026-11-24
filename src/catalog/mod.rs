mod in_memory;

pub(crate) use in_memory::InMemoryCatalog;

use std::collections::HashSet;

use crate::types::{ColumnType, DbError, Fsm};

pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) column_type: ColumnType,
}

pub struct TableSchema {
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    pub(crate) fsms: Vec<Fsm>,
}

impl TableSchema {
    pub(crate) fn validate(&self) -> Result<(), DbError> {
        let mut column_names = HashSet::with_capacity(self.columns.len());
        for col in &self.columns {
            if !column_names.insert(col.name.as_str()) {
                return Err(DbError::InvalidSchema(format!(
                    "duplicate column name: {}",
                    col.name
                )));
            }
            if let ColumnType::Fsm(fsm_idx) = col.column_type
                && (fsm_idx as usize) >= self.fsms.len()
            {
                return Err(DbError::InvalidSchema(format!(
                    "fsm index {} out of bounds for table {}",
                    fsm_idx, self.name
                )));
            }
        }

        for (idx, fsm) in self.fsms.iter().enumerate() {
            Self::validate_fsm(idx, fsm)?;
        }

        Ok(())
    }

    fn validate_fsm(fsm_idx: usize, fsm: &Fsm) -> Result<(), DbError> {
        if fsm.states.is_empty() {
            return Err(DbError::InvalidSchema(format!(
                "fsm {} has no states",
                fsm_idx
            )));
        }
        if (fsm.initial as usize) >= fsm.states.len() {
            return Err(DbError::InvalidSchema(format!(
                "fsm {} initial state {} out of bounds",
                fsm_idx, fsm.initial
            )));
        }

        let mut state_names = HashSet::with_capacity(fsm.states.len());
        for state in &fsm.states {
            if !state_names.insert(state.as_str()) {
                return Err(DbError::InvalidSchema(format!(
                    "fsm {} has duplicate state name: {}",
                    fsm_idx, state
                )));
            }
        }

        for &(from, to) in fsm.allowed.keys() {
            if (from as usize) >= fsm.states.len() || (to as usize) >= fsm.states.len() {
                return Err(DbError::InvalidSchema(format!(
                    "fsm {} transition ({}, {}) out of bounds",
                    fsm_idx, from, to
                )));
            }
        }

        Ok(())
    }
}

pub trait Catalog {
    fn create_table(&mut self, schema: TableSchema) -> Result<(), DbError>;
    fn get_table(&self, table_name: &str) -> Result<Option<&TableSchema>, DbError>;
}
