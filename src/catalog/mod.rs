mod in_memory;

use crate::types::{ColumnType, DbError, Fsm};

pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) column_type: ColumnType,
}

pub(crate) struct TableSchema {
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    pub(crate) fsms: Vec<Fsm>,
}

pub(crate) trait Catalog {
    fn create_table(&mut self, schema: TableSchema) -> Result<(), DbError>;
    fn get_table(&self, table_name: &str) -> Result<Option<&TableSchema>, DbError>;
}
