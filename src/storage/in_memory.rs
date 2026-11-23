use std::collections::HashMap;

use crate::storage::Storage;
use crate::types::{DbError, Row, RowId};

pub(crate) struct InMemoryStorage {
    tables: HashMap<String, Vec<(RowId, Row)>>,
    last: usize,
}

impl InMemoryStorage {
    pub(crate) fn new() -> Self {
        InMemoryStorage {
            tables: HashMap::new(),
            last: 0,
        }
    }
}

impl Storage for InMemoryStorage {
    fn insert(&mut self, table: &str, row: Row) -> Result<RowId, crate::types::DbError> {
        let row_id = RowId {
            page_id: (self.last >> 16) as u32,
            row_id: self.last as u16,
        };

        self.last += 1;

        self.tables
            .entry(table.into())
            .or_insert(vec![])
            .push((row_id, row));

        Ok(row_id)
    }

    fn update(&mut self, table: &str, row_id: RowId, row: Row) -> Result<RowId, DbError> {
        let rows = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;

        let (_, existing_row) = rows
            .iter_mut()
            .find(|(id, _)| *id == row_id)
            .ok_or(DbError::RowNotFound(row_id))?;

        *existing_row = row;
        Ok(row_id)
    }

    fn get(&self, table: &str, row_id: RowId) -> Result<Option<Row>, crate::types::DbError> {
        let row = self
            .tables
            .get(table)
            .and_then(|rows| rows.iter().find(|(id, _)| *id == row_id))
            .map(|(_, row)| row.clone());
        Ok(row)
    }

    fn delete(&mut self, table: &str, row_id: RowId) -> Result<(), crate::types::DbError> {
        if let Some(t) = self.tables.get_mut(table) {
            t.retain(|(id, _)| *id != row_id)
        }
        Ok(())
    }

    fn scan(&self, table: &str) -> Result<Vec<(RowId, Row)>, crate::types::DbError> {
        Ok(self.tables.get(table).cloned().unwrap_or_else(Vec::new))
    }
}

#[cfg(test)]
mod tests {
    use crate::types::Value;

    use super::*;

    #[test]
    fn test_scan_empty() {
        const TABLE_NAME: &str = "meow1";

        let storage = InMemoryStorage::new();
        let scan_result = storage.scan(TABLE_NAME).unwrap();
        assert_eq!(scan_result.len(), 0);
    }

    #[test]
    fn test_scan_few_rows() {
        const TABLE_NAME: &str = "meow1";

        let mut storage = InMemoryStorage::new();
        storage.insert(TABLE_NAME, vec![]).unwrap();
        storage.insert(TABLE_NAME, vec![]).unwrap();
        let scan_result = storage.scan(TABLE_NAME).unwrap();
        assert_eq!(scan_result.len(), 2);
    }

    #[test]
    fn test_delete_row() {
        const TABLE_NAME: &str = "meow1";

        let mut storage = InMemoryStorage::new();
        let row_id: RowId = storage.insert(TABLE_NAME, vec![]).unwrap();
        let scan_res = storage.scan(TABLE_NAME).unwrap();
        assert_eq!(scan_res.len(), 1);
        assert!(storage.get(TABLE_NAME, row_id).unwrap().is_some());

        storage.delete(TABLE_NAME, row_id).unwrap();
        let scan_res = storage.scan(TABLE_NAME).unwrap();
        assert_eq!(scan_res.len(), 0);
        assert!(storage.get(TABLE_NAME, row_id).unwrap().is_none());
    }

    #[test]
    fn test_update_row() {
        const TABLE_NAME: &str = "meow1";

        let mut storage = InMemoryStorage::new();
        let row_id: RowId = storage.insert(TABLE_NAME, vec![Value::Integer(6)]).unwrap();

        let scan_res = storage.scan(TABLE_NAME).unwrap();
        assert_eq!(scan_res.len(), 1);
        assert_eq!(scan_res[0].1[0], Value::Integer(6));
        assert!(storage.get(TABLE_NAME, row_id).unwrap().is_some());

        let row_id: RowId = storage
            .update(TABLE_NAME, row_id, vec![Value::Integer(7)])
            .unwrap();
        let scan_res = storage.scan(TABLE_NAME).unwrap();
        assert_eq!(scan_res.len(), 1);
        assert_eq!(scan_res[0].1[0], Value::Integer(7));
        assert!(storage.get(TABLE_NAME, row_id).unwrap().is_some());
    }
}
