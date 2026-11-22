use crate::types::{DbError, Row, RowId};

mod in_memory;

trait Storage {
    fn insert(&mut self, table: &str, row: Row) -> Result<RowId, DbError>;
    fn update(&mut self, table: &str, row_id: RowId, row: Row) -> Result<RowId, DbError>;
    fn delete(&mut self, table: &str, row_id: RowId) -> Result<(), DbError>;
    fn get(&self, table: &str, row_id: RowId) -> Result<Option<Row>, DbError>;
    fn scan(&self, table: &str) -> Result<Vec<(RowId, Row)>, DbError>;
}
