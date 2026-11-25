use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::buffer_pool::BufferPool;
use crate::disk::{DiskManager, PageId};
use crate::row_codec::{decode_row, encode_row};
use crate::slotted_page::{MAX_RECORD_SIZE, NO_NEXT_PAGE};
use crate::storage::Storage;
use crate::types::{DbError, Row, RowId};

pub struct DiskStorage {
    pool: RefCell<BufferPool>,
    tables_meta_path: PathBuf,
    first_pages: HashMap<String, PageId>,
    tail_cache: HashMap<String, PageId>,
}

impl DiskStorage {
    pub fn open(dir: &Path, pool_capacity: usize) -> Result<Self, DbError> {
        fs::create_dir_all(dir)?;
        let disk = DiskManager::open(&dir.join("data.db"))?;
        let pool = BufferPool::new(disk, pool_capacity);

        let tables_meta_path = dir.join("tables.json");
        let first_pages = match fs::read_to_string(&tables_meta_path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|e| DbError::StorageError(format!("failed to parse tables.json: {e}")))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(DbError::IoError(e)),
        };

        Ok(Self {
            pool: RefCell::new(pool),
            tables_meta_path,
            first_pages,
            tail_cache: HashMap::new(),
        })
    }

    fn save_tables_meta(&self) -> Result<(), DbError> {
        let tmp_path = match self.tables_meta_path.file_name().and_then(|n| n.to_str()) {
            Some(name) => self.tables_meta_path.with_file_name(format!("{name}.tmp")),
            None => self.tables_meta_path.with_extension("tmp"),
        };

        let bytes = serde_json::to_vec_pretty(&self.first_pages)
            .map_err(|e| DbError::StorageError(format!("failed to serialize tables.json: {e}")))?;

        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &self.tables_meta_path)?;
        Ok(())
    }

    fn find_tail_page(&mut self, table: &str) -> Result<PageId, DbError> {
        if let Some(&tail_id) = self.tail_cache.get(table) {
            return Ok(tail_id);
        }

        let mut current_id = *self
            .first_pages
            .get(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;

        let pool = self.pool.get_mut();
        loop {
            let page = pool.fetch_page(current_id)?;
            let next_id = page.next_page_id();
            pool.unpin_page(current_id, false)?;
            if next_id == NO_NEXT_PAGE {
                break;
            }
            current_id = next_id;
        }

        self.tail_cache.insert(table.to_string(), current_id);
        Ok(current_id)
    }

    fn insert_encoded(&mut self, table: &str, bytes: &[u8]) -> Result<RowId, DbError> {
        if bytes.is_empty() || bytes.len() > MAX_RECORD_SIZE {
            return Err(DbError::StorageError(format!(
                "encoded row size {} exceeds max record size {MAX_RECORD_SIZE}",
                bytes.len()
            )));
        }

        let tail_id = self.find_tail_page(table)?;
        let pool = self.pool.get_mut();

        let slot_opt = {
            let page = pool.fetch_page_mut(tail_id)?;
            page.insert(bytes)
        };

        if let Some(slot_id) = slot_opt {
            pool.unpin_page(tail_id, true)?;
            return Ok(RowId {
                page_id: tail_id,
                row_id: slot_id,
            });
        }
        pool.unpin_page(tail_id, false)?;

        let (new_id, new_slot_opt) = {
            let (new_id, new_page) = pool.new_page()?;
            (new_id, new_page.insert(bytes))
        };
        let Some(slot_id) = new_slot_opt else {
            pool.unpin_page(new_id, false)?;
            return Err(DbError::StorageError(
                "failed to insert row into fresh page".to_string(),
            ));
        };
        pool.unpin_page(new_id, true)?;

        {
            let tail_page = pool.fetch_page_mut(tail_id)?;
            tail_page.set_next_page_id(new_id);
        }
        pool.unpin_page(tail_id, true)?;

        self.tail_cache.insert(table.to_string(), new_id);
        Ok(RowId {
            page_id: new_id,
            row_id: slot_id,
        })
    }
}

impl Storage for DiskStorage {
    fn create_table(&mut self, table: &str) -> Result<(), DbError> {
        if self.first_pages.contains_key(table) {
            return Err(DbError::TableAlreadyExists(table.to_string()));
        }

        let pool = self.pool.get_mut();
        let (first_id, _) = pool.new_page()?;
        pool.unpin_page(first_id, true)?;

        self.first_pages.insert(table.to_string(), first_id);
        self.tail_cache.insert(table.to_string(), first_id);
        if let Err(err) = self.save_tables_meta() {
            self.first_pages.remove(table);
            self.tail_cache.remove(table);
            return Err(err);
        }

        self.pool.get_mut().flush_all()?;
        Ok(())
    }

    fn insert(&mut self, table: &str, row: Row) -> Result<RowId, DbError> {
        let bytes = encode_row(&row)?;
        self.insert_encoded(table, &bytes)
    }

    fn update(&mut self, table: &str, row_id: RowId, row: Row) -> Result<RowId, DbError> {
        if !self.first_pages.contains_key(table) {
            return Err(DbError::TableNotFound(table.to_string()));
        }

        let bytes = encode_row(&row)?;
        if bytes.is_empty() || bytes.len() > MAX_RECORD_SIZE {
            return Err(DbError::StorageError(format!(
                "encoded row size {} exceeds max record size {MAX_RECORD_SIZE}",
                bytes.len()
            )));
        }

        enum UpdateOutcome {
            NotFound,
            InPlace,
            Relocate,
        }

        let outcome = {
            let pool = self.pool.get_mut();
            let page = pool.fetch_page_mut(row_id.page_id)?;
            if page.get(row_id.row_id).is_none() {
                UpdateOutcome::NotFound
            } else if page.update_in_place(row_id.row_id, &bytes) {
                UpdateOutcome::InPlace
            } else {
                page.delete(row_id.row_id);
                UpdateOutcome::Relocate
            }
        };

        let pool = self.pool.get_mut();
        match outcome {
            UpdateOutcome::NotFound => {
                pool.unpin_page(row_id.page_id, false)?;
                Err(DbError::RowNotFound(row_id))
            }
            UpdateOutcome::InPlace => {
                pool.unpin_page(row_id.page_id, true)?;
                Ok(row_id)
            }
            UpdateOutcome::Relocate => {
                pool.unpin_page(row_id.page_id, true)?;
                self.insert_encoded(table, &bytes)
            }
        }
    }

    fn delete(&mut self, table: &str, row_id: RowId) -> Result<(), DbError> {
        if !self.first_pages.contains_key(table) {
            return Err(DbError::TableNotFound(table.to_string()));
        }

        let pool = self.pool.get_mut();
        let deleted = {
            let page = pool.fetch_page_mut(row_id.page_id)?;
            page.delete(row_id.row_id)
        };
        pool.unpin_page(row_id.page_id, deleted)?;
        Ok(())
    }

    fn get(&self, table: &str, row_id: RowId) -> Result<Option<Row>, DbError> {
        if !self.first_pages.contains_key(table) {
            return Err(DbError::TableNotFound(table.to_string()));
        }

        let mut pool = self.pool.borrow_mut();
        let page = pool.fetch_page(row_id.page_id)?;
        let decoded = page.get(row_id.row_id).map(decode_row);
        pool.unpin_page(row_id.page_id, false)?;
        decoded.transpose()
    }

    fn scan(&self, table: &str) -> Result<Vec<(RowId, Row)>, DbError> {
        let first_page_id = *self
            .first_pages
            .get(table)
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;

        let mut pool = self.pool.borrow_mut();
        let mut rows = Vec::new();
        let mut current_page_id = first_page_id;

        while current_page_id != NO_NEXT_PAGE {
            let page = pool.fetch_page(current_page_id)?;
            let next_page_id = page.next_page_id();
            let slot_count = page.slot_count();
            let mut decode_err: Option<DbError> = None;

            for slot_id in 0..slot_count {
                if let Some(bytes) = page.get(slot_id) {
                    match decode_row(bytes) {
                        Ok(row) => rows.push((
                            RowId {
                                page_id: current_page_id,
                                row_id: slot_id,
                            },
                            row,
                        )),
                        Err(err) => {
                            decode_err = Some(err);
                            break;
                        }
                    }
                }
            }

            pool.unpin_page(current_page_id, false)?;
            if let Some(err) = decode_err {
                return Err(err);
            }
            current_page_id = next_page_id;
        }

        Ok(rows)
    }

    fn flush(&mut self) -> Result<(), DbError> {
        self.pool.get_mut().flush_all()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::types::Value;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("fsm_db_{prefix}_{}_{id}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn make_row(id: i64, text: &str) -> Row {
        vec![Value::Integer(id), Value::String(text.to_string())]
    }

    #[test]
    fn tight_pool_multipage_crud_relocation_and_reopen() {
        let dir = TempDir::new("disk_storage_tight");
        let table = "items";
        let mut expected: HashMap<i64, Row> = HashMap::new();
        let mut row_ids: HashMap<i64, RowId> = HashMap::new();

        {
            let mut storage = DiskStorage::open(&dir.path, 2).unwrap();
            storage.create_table(table).unwrap();

            for i in 0..150_i64 {
                let row = make_row(i, &format!("payload-{i:04}-{}", "x".repeat(40)));
                let rid = storage.insert(table, row.clone()).unwrap();
                expected.insert(i, row);
                row_ids.insert(i, rid);
            }

            let scanned = storage.scan(table).unwrap();
            assert_eq!(scanned.len(), 150);
            let unique_pages: HashSet<PageId> =
                scanned.iter().map(|(rid, _)| rid.page_id).collect();
            assert!(
                unique_pages.len() >= 4,
                "expected at least 4 pages, got {}",
                unique_pages.len()
            );
            for (_, row) in &scanned {
                let Value::Integer(id) = row[0] else {
                    panic!("expected integer id");
                };
                assert_eq!(row, &expected[&id]);
            }

            // In-place update (shorter string) keeps the same RowId.
            let id_shrink = row_ids[&0];
            let shrunk = make_row(0, "tiny");
            let updated_shrink_id = storage.update(table, id_shrink, shrunk.clone()).unwrap();
            assert_eq!(updated_shrink_id, id_shrink);
            assert_eq!(storage.get(table, id_shrink).unwrap(), Some(shrunk.clone()));
            expected.insert(0, shrunk);

            // Growing update that exceeds remaining space on page 0 relocates the row.
            let id_grow = row_ids[&1];
            let grown = make_row(1, &"g".repeat(1900));
            let relocated_id = storage.update(table, id_grow, grown.clone()).unwrap();
            assert_ne!(relocated_id, id_grow);
            assert_eq!(storage.get(table, id_grow).unwrap(), None);
            assert_eq!(
                storage.get(table, relocated_id).unwrap(),
                Some(grown.clone())
            );
            expected.insert(1, grown);
            row_ids.insert(1, relocated_id);

            // Delete a row.
            let id_del = row_ids[&5];
            storage.delete(table, id_del).unwrap();
            assert_eq!(storage.get(table, id_del).unwrap(), None);
            expected.remove(&5);

            // Oversized insert and update must fail cleanly without corrupting data.
            let oversized = make_row(999, &"z".repeat(MAX_RECORD_SIZE));
            assert!(matches!(
                storage.insert(table, oversized.clone()),
                Err(DbError::StorageError(_))
            ));
            let id_keep = row_ids[&2];
            assert!(matches!(
                storage.update(table, id_keep, oversized),
                Err(DbError::StorageError(_))
            ));
            assert_eq!(
                storage.get(table, id_keep).unwrap(),
                Some(expected[&2].clone())
            );

            storage.flush().unwrap();
        }

        // Reopen with tight pool and verify all live rows and deletions persisted.
        let reopened = DiskStorage::open(&dir.path, 2).unwrap();
        let scanned = reopened.scan(table).unwrap();
        assert_eq!(scanned.len(), expected.len());
        let mut seen_ids = HashSet::new();
        for (rid, row) in scanned {
            let Value::Integer(id) = row[0] else {
                panic!("expected integer id");
            };
            assert!(seen_ids.insert(id));
            assert_eq!(row, expected[&id]);
            assert_eq!(reopened.get(table, rid).unwrap(), Some(row));
        }
        assert!(!seen_ids.contains(&5));
    }
}
