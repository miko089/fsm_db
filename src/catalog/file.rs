use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, TableSchema};
use crate::types::DbError;

pub struct FileCatalog {
    path: PathBuf,
    tables: HashMap<String, TableSchema>,
}

impl FileCatalog {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let tables = match fs::read_to_string(path) {
            Ok(contents) => {
                let tables: HashMap<String, TableSchema> = serde_json::from_str(&contents)
                    .map_err(|e| DbError::StorageError(format!("failed to parse catalog: {e}")))?;
                for schema in tables.values() {
                    schema.validate()?;
                }
                tables
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(DbError::IoError(e)),
        };

        Ok(Self {
            path: path.to_path_buf(),
            tables,
        })
    }

    fn save(&self) -> Result<(), DbError> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = match self.path.file_name().and_then(|n| n.to_str()) {
            Some(name) => self.path.with_file_name(format!("{name}.tmp")),
            None => self.path.with_extension("tmp"),
        };

        let bytes = serde_json::to_vec_pretty(&self.tables)
            .map_err(|e| DbError::StorageError(format!("failed to serialize catalog: {e}")))?;

        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &self.path)?;

        Ok(())
    }
}

impl Catalog for FileCatalog {
    fn create_table(&mut self, schema: TableSchema) -> Result<(), DbError> {
        if self.tables.contains_key(&schema.name) {
            return Err(DbError::TableAlreadyExists(schema.name));
        }
        schema.validate()?;

        let name = schema.name.clone();
        self.tables.insert(name.clone(), schema);
        if let Err(err) = self.save() {
            self.tables.remove(&name);
            return Err(err);
        }

        Ok(())
    }

    fn get_table(&self, table_name: &str) -> Result<Option<&TableSchema>, DbError> {
        Ok(self.tables.get(table_name))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::catalog::Column;
    use crate::expr::{BinaryOp, Expr};
    use crate::types::{ColumnType, Fsm, Value};

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
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn open_missing_create_reopen_and_reject_duplicate() {
        let dir = TempDir::new("file_catalog");
        let catalog_path = dir.path.join("catalog.json");

        {
            let mut catalog = FileCatalog::open(&catalog_path).unwrap();
            assert!(catalog.get_table("tasks").unwrap().is_none());

            let mut allowed = HashMap::new();
            allowed.insert((0, 1), Expr::Literal(Value::Bool(true)));
            allowed.insert(
                (1, 2),
                Expr::Binary {
                    op: BinaryOp::Ge,
                    left: Box::new(Expr::Column(0)),
                    right: Box::new(Expr::Literal(Value::Integer(3))),
                },
            );

            let schema = TableSchema {
                name: "tasks".to_string(),
                columns: vec![
                    Column {
                        name: "retries".to_string(),
                        column_type: ColumnType::Integer,
                    },
                    Column {
                        name: "state".to_string(),
                        column_type: ColumnType::Fsm(0),
                    },
                ],
                fsms: vec![Fsm {
                    states: vec!["pending".into(), "running".into(), "failed".into()],
                    initial: 0,
                    allowed,
                }],
            };

            catalog.create_table(schema).unwrap();
            assert!(catalog_path.exists());

            let dup = TableSchema {
                name: "tasks".to_string(),
                columns: vec![],
                fsms: vec![],
            };
            assert!(matches!(
                catalog.create_table(dup),
                Err(DbError::TableAlreadyExists(_))
            ));
        }

        let reopened = FileCatalog::open(&catalog_path).unwrap();
        let stored = reopened.get_table("tasks").unwrap().unwrap();
        assert_eq!(stored.columns.len(), 2);
        assert_eq!(stored.fsms.len(), 1);
        assert_eq!(stored.fsms[0].states, vec!["pending", "running", "failed"]);
        assert_eq!(stored.fsms[0].initial, 0);
        assert!(stored.fsms[0].allowed.contains_key(&(0, 1)));
        assert!(stored.fsms[0].allowed.contains_key(&(1, 2)));
    }
}
