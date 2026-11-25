use crate::types::{DbError, Row, RowId};

mod disk;
mod in_memory;

pub use disk::DiskStorage;
pub(crate) use in_memory::InMemoryStorage;

pub trait Storage {
    fn create_table(&mut self, _table: &str) -> Result<(), DbError> {
        Ok(())
    }
    fn insert(&mut self, table: &str, row: Row) -> Result<RowId, DbError>;
    fn update(&mut self, table: &str, row_id: RowId, row: Row) -> Result<RowId, DbError>;
    fn delete(&mut self, table: &str, row_id: RowId) -> Result<(), DbError>;
    fn get(&self, table: &str, row_id: RowId) -> Result<Option<Row>, DbError>;
    fn scan(&self, table: &str) -> Result<Vec<(RowId, Row)>, DbError>;
    fn flush(&mut self) -> Result<(), DbError> {
        Ok(())
    }
}
