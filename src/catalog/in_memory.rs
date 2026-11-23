use std::collections::HashMap;

use crate::catalog::{Catalog, TableSchema};
use crate::types::DbError;

pub(crate) struct InMemoryCatalog {
    tables: HashMap<String, TableSchema>,
}

impl InMemoryCatalog {
    fn new() -> Self {
        Self {
            tables: HashMap::new(),
        }
    }
}

impl Catalog for InMemoryCatalog {
    fn create_table(&mut self, schema: TableSchema) -> Result<(), DbError> {
        if self.tables.contains_key(&schema.name) {
            return Err(DbError::TableAlreadyExists(schema.name));
        }
        schema.validate()?;
        self.tables.insert(schema.name.clone(), schema);
        Ok(())
    }

    fn get_table(&self, table_name: &str) -> Result<Option<&TableSchema>, DbError> {
        Ok(self.tables.get(table_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Column, TableSchema};
    use crate::types::{ColumnType, DbError, Expr, Fsm};
    use std::collections::HashMap;

    #[test]
    fn test_table_not_exist() {
        let catalog = InMemoryCatalog::new();

        assert!(catalog.get_table("nonexistent table").unwrap().is_none())
    }

    #[test]
    fn test_table_exists() {
        const TABLE_NAME: &str = "existent table";
        let mut catalog = InMemoryCatalog::new();

        let schema = TableSchema {
            name: TABLE_NAME.to_string(),
            columns: vec![],
            fsms: vec![],
        };
        catalog.create_table(schema).unwrap();

        assert!(catalog.get_table(TABLE_NAME).unwrap().is_some())
    }

    #[test]
    fn test_duplicate_table_name() {
        let mut catalog = InMemoryCatalog::new();

        let first = TableSchema {
            name: "tasks".to_string(),
            columns: vec![Column {
                name: "id".to_string(),
                column_type: ColumnType::Integer,
            }],
            fsms: vec![],
        };
        let second = TableSchema {
            name: "tasks".to_string(),
            columns: vec![Column {
                name: "title".to_string(),
                column_type: ColumnType::String,
            }],
            fsms: vec![],
        };

        catalog.create_table(first).unwrap();
        let err = catalog.create_table(second).unwrap_err();
        assert!(matches!(err, DbError::TableAlreadyExists(_)));

        let stored = catalog.get_table("tasks").unwrap().unwrap();
        assert_eq!(stored.columns.len(), 1);
        assert_eq!(stored.columns[0].name, "id");
    }

    #[test]
    fn test_duplicate_column_name() {
        let mut catalog = InMemoryCatalog::new();

        let schema = TableSchema {
            name: "tasks".to_string(),
            columns: vec![
                Column {
                    name: "id".to_string(),
                    column_type: ColumnType::Integer,
                },
                Column {
                    name: "id".to_string(),
                    column_type: ColumnType::String,
                },
            ],
            fsms: vec![],
        };

        let err = catalog.create_table(schema).unwrap_err();
        assert!(matches!(err, DbError::InvalidSchema(_)));
    }

    #[test]
    fn test_nonexistent_fsm_index() {
        let mut catalog = InMemoryCatalog::new();

        let schema = TableSchema {
            name: "tasks".to_string(),
            columns: vec![Column {
                name: "state".to_string(),
                column_type: ColumnType::Fsm(1),
            }],
            fsms: vec![Fsm {
                states: vec!["pending".to_string()],
                initial: 0,
                allowed: HashMap::new(),
            }],
        };

        let err = catalog.create_table(schema).unwrap_err();
        assert!(matches!(err, DbError::InvalidSchema(_)));
    }

    #[test]
    fn test_invalid_fsm_empty_states_or_bad_initial() {
        let mut catalog = InMemoryCatalog::new();

        let empty_states_schema = TableSchema {
            name: "tasks_empty".to_string(),
            columns: vec![Column {
                name: "state".to_string(),
                column_type: ColumnType::Fsm(0),
            }],
            fsms: vec![Fsm {
                states: vec![],
                initial: 0,
                allowed: HashMap::new(),
            }],
        };
        assert!(matches!(
            catalog.create_table(empty_states_schema).unwrap_err(),
            DbError::InvalidSchema(_)
        ));

        let bad_initial_schema = TableSchema {
            name: "tasks_bad_initial".to_string(),
            columns: vec![Column {
                name: "state".to_string(),
                column_type: ColumnType::Fsm(0),
            }],
            fsms: vec![Fsm {
                states: vec!["pending".to_string()],
                initial: 1,
                allowed: HashMap::new(),
            }],
        };
        assert!(matches!(
            catalog.create_table(bad_initial_schema).unwrap_err(),
            DbError::InvalidSchema(_)
        ));
    }

    #[test]
    fn test_invalid_fsm_duplicate_state_names() {
        let mut catalog = InMemoryCatalog::new();

        let schema = TableSchema {
            name: "tasks".to_string(),
            columns: vec![Column {
                name: "state".to_string(),
                column_type: ColumnType::Fsm(0),
            }],
            fsms: vec![Fsm {
                states: vec!["pending".to_string(), "pending".to_string()],
                initial: 0,
                allowed: HashMap::new(),
            }],
        };

        assert!(matches!(
            catalog.create_table(schema).unwrap_err(),
            DbError::InvalidSchema(_)
        ));
    }

    #[test]
    fn test_invalid_fsm_out_of_bounds_allowed_transition() {
        let mut catalog = InMemoryCatalog::new();

        let mut allowed_bad_to = HashMap::new();
        allowed_bad_to.insert((0, 2), Expr {});

        let schema_bad_to = TableSchema {
            name: "tasks_bad_to".to_string(),
            columns: vec![Column {
                name: "state".to_string(),
                column_type: ColumnType::Fsm(0),
            }],
            fsms: vec![Fsm {
                states: vec!["pending".to_string(), "done".to_string()],
                initial: 0,
                allowed: allowed_bad_to,
            }],
        };
        assert!(matches!(
            catalog.create_table(schema_bad_to).unwrap_err(),
            DbError::InvalidSchema(_)
        ));

        let mut allowed_bad_from = HashMap::new();
        allowed_bad_from.insert((2, 0), Expr {});

        let schema_bad_from = TableSchema {
            name: "tasks_bad_from".to_string(),
            columns: vec![Column {
                name: "state".to_string(),
                column_type: ColumnType::Fsm(0),
            }],
            fsms: vec![Fsm {
                states: vec!["pending".to_string(), "done".to_string()],
                initial: 0,
                allowed: allowed_bad_from,
            }],
        };
        assert!(matches!(
            catalog.create_table(schema_bad_from).unwrap_err(),
            DbError::InvalidSchema(_)
        ));
    }

    #[test]
    fn test_valid_fsm() {
        let mut catalog = InMemoryCatalog::new();

        let mut allowed = HashMap::new();
        allowed.insert((0, 1), Expr {});
        allowed.insert((1, 2), Expr {});

        let schema = TableSchema {
            name: "tasks".to_string(),
            columns: vec![
                Column {
                    name: "id".to_string(),
                    column_type: ColumnType::Integer,
                },
                Column {
                    name: "state".to_string(),
                    column_type: ColumnType::Fsm(0),
                },
            ],
            fsms: vec![Fsm {
                states: vec![
                    "pending".to_string(),
                    "in_progress".to_string(),
                    "done".to_string(),
                ],
                initial: 0,
                allowed,
            }],
        };

        catalog.create_table(schema).unwrap();

        let stored = catalog.get_table("tasks").unwrap().unwrap();
        assert_eq!(stored.columns.len(), 2);
        assert_eq!(stored.fsms.len(), 1);
        assert_eq!(
            stored.fsms[0].states,
            vec!["pending", "in_progress", "done"]
        );
        assert_eq!(stored.fsms[0].initial, 0);
        assert_eq!(stored.fsms[0].allowed.len(), 2);
        assert!(stored.fsms[0].allowed.contains_key(&(0, 1)));
        assert!(stored.fsms[0].allowed.contains_key(&(1, 2)));
    }
}
