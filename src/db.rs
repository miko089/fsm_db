use std::fs;
use std::path::Path;

use crate::{
    binder::{BoundStatement, bind},
    catalog::{Catalog, FileCatalog, InMemoryCatalog},
    executor::{ExecContext, Executor},
    expr::{Clock, Expr, SystemClock},
    lexer::tokenize,
    parser::parse,
    storage::{DiskStorage, InMemoryStorage, Storage},
    tx::Transaction,
    types::{ColumnType, DbError, Row},
};

const DEFAULT_BUFFER_POOL_CAPACITY: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsmRef {
    pub table: String,
    pub fsm_idx: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    Ok,
    Rows {
        rows: Vec<Row>,
        fsm_columns: Vec<Option<FsmRef>>,
    },
}

pub struct Database<S: Storage, C: Catalog> {
    storage: S,
    catalog: C,
    clock: Box<dyn Clock>,
    active_tx: Option<Transaction>,
}

impl<S: Storage, C: Catalog> Database<S, C> {
    pub fn new(storage: S, catalog: C, clock: Box<dyn Clock>) -> Self {
        Self {
            storage,
            catalog,
            clock,
            active_tx: None,
        }
    }

    pub fn catalog(&self) -> &C {
        &self.catalog
    }

    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, DbError> {
        let tokens = tokenize(sql)?;
        let statements = parse(&tokens)?;
        let mut results = Vec::with_capacity(statements.len());

        for stmt in statements {
            match bind(stmt, &self.catalog)? {
                BoundStatement::CreateTable(schema) => {
                    if self.active_tx.is_some() {
                        return Err(DbError::ExecError(
                            "DDL is not supported inside a transaction".to_string(),
                        ));
                    }
                    let table_name = schema.name.clone();
                    self.catalog.create_table(schema)?;
                    self.storage.create_table(&table_name)?;
                    results.push(QueryResult::Ok);
                }
                BoundStatement::Begin => {
                    if self.active_tx.is_some() {
                        return Err(DbError::ExecError("transaction already active".to_string()));
                    }
                    self.active_tx = Some(Transaction::new());
                    results.push(QueryResult::Ok);
                }
                BoundStatement::Commit => {
                    let mut tx = self
                        .active_tx
                        .take()
                        .ok_or_else(|| DbError::ExecError("no active transaction".to_string()))?;
                    tx.commit(&mut self.storage, &self.catalog, self.clock.as_ref())?;
                    results.push(QueryResult::Ok);
                }
                BoundStatement::Rollback => {
                    let mut tx = self
                        .active_tx
                        .take()
                        .ok_or_else(|| DbError::ExecError("no active transaction".to_string()))?;
                    tx.rollback();
                    results.push(QueryResult::Ok);
                }
                BoundStatement::Execute(mut exec) => {
                    let fsm_columns = Self::output_fsm_columns(&exec, &self.catalog)?;
                    let rows = if let Some(tx) = self.active_tx.as_mut() {
                        Self::drain_executor(&mut exec, tx, &self.storage, self.clock.as_ref())?
                    } else {
                        let mut tx = Transaction::new();
                        let rows = Self::drain_executor(
                            &mut exec,
                            &mut tx,
                            &self.storage,
                            self.clock.as_ref(),
                        )?;
                        tx.commit(&mut self.storage, &self.catalog, self.clock.as_ref())?;
                        rows
                    };
                    results.push(QueryResult::Rows { rows, fsm_columns });
                }
            }
        }

        Ok(results)
    }

    fn table_fsm_columns(table: &str, catalog: &C) -> Result<Vec<Option<FsmRef>>, DbError> {
        let schema = catalog
            .get_table(table)?
            .ok_or_else(|| DbError::TableNotFound(table.to_string()))?;
        Ok(schema
            .columns
            .iter()
            .map(|col| match col.column_type {
                ColumnType::Fsm(fsm_idx) => Some(FsmRef {
                    table: schema.name.clone(),
                    fsm_idx,
                }),
                _ => None,
            })
            .collect())
    }

    fn output_fsm_columns(exec: &Executor, catalog: &C) -> Result<Vec<Option<FsmRef>>, DbError> {
        match exec {
            Executor::SeqScan { table, .. }
            | Executor::Insert { table, .. }
            | Executor::Update { table, .. }
            | Executor::Delete { table, .. } => Self::table_fsm_columns(table, catalog),
            Executor::Filter { input, .. } | Executor::Limit { input, .. } => {
                Self::output_fsm_columns(input, catalog)
            }
            Executor::Project { input, cols } => {
                let input_cols = Self::output_fsm_columns(input, catalog)?;
                Ok(cols
                    .iter()
                    .map(|expr| match expr {
                        Expr::Column(idx) => input_cols.get(*idx).cloned().flatten(),
                        _ => None,
                    })
                    .collect())
            }
            Executor::SingleRow { .. } => Ok(Vec::new()),
        }
    }

    fn drain_executor(
        exec: &mut Executor,
        tx: &mut Transaction,
        storage: &S,
        clock: &dyn Clock,
    ) -> Result<Vec<Row>, DbError> {
        let mut ctx = ExecContext { tx, storage, clock };
        let mut rows = Vec::new();
        while let Some((_, row)) = exec.next(&mut ctx)? {
            rows.push(row);
        }
        Ok(rows)
    }
}

impl Database<InMemoryStorage, InMemoryCatalog> {
    pub fn in_memory() -> Self {
        Self::with_clock(Box::new(SystemClock))
    }

    pub fn with_clock(clock: Box<dyn Clock>) -> Self {
        Self::new(InMemoryStorage::new(), InMemoryCatalog::new(), clock)
    }
}

impl Database<DiskStorage, FileCatalog> {
    pub fn open(dir: &Path) -> Result<Self, DbError> {
        Self::open_with_clock(dir, Box::new(SystemClock))
    }

    pub fn open_with_clock(dir: &Path, clock: Box<dyn Clock>) -> Result<Self, DbError> {
        fs::create_dir_all(dir)?;
        let catalog = FileCatalog::open(&dir.join("catalog.json"))?;
        let storage = DiskStorage::open(dir, DEFAULT_BUFFER_POOL_CAPACITY)?;
        Ok(Self::new(storage, catalog, clock))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{expr::MockClock, types::Value};

    const EXAMPLE_SQL: &str = include_str!("../docs/example.sql");

    const TASKS_DDL: &str = "CREATE TABLE tasks (
        retries INTEGER,
        done BOOL,
        title STRING,
        state FSM (
            STATES ('pending', 'running', 'failed', 'done'),
            INITIAL 'pending',
            TRANSITIONS (
                'pending' -> 'running',
                'running' -> 'done',
                'running' -> 'failed' WHEN retries >= 3
            )
        )
    );";

    fn db_with_clock(now: i64) -> Database<InMemoryStorage, InMemoryCatalog> {
        Database::with_clock(Box::new(MockClock { now }))
    }

    fn tasks_fsm_ref() -> FsmRef {
        FsmRef {
            table: "tasks".to_string(),
            fsm_idx: 0,
        }
    }

    #[test]
    fn test_example_sql_with_mock_clock() {
        let mut db = db_with_clock(1_700_000_000);
        let results = db.execute(EXAMPLE_SQL).unwrap();
        let tasks_fsm = vec![None, None, None, Some(tasks_fsm_ref())];

        assert_eq!(
            results,
            vec![
                QueryResult::Ok, // CREATE TABLE tasks
                QueryResult::Ok, // BEGIN
                QueryResult::Ok, // COMMIT
                QueryResult::Ok, // BEGIN
                QueryResult::Ok, // ROLLBACK
                QueryResult::Rows {
                    rows: vec![vec![
                        Value::Integer(0),
                        Value::Bool(false),
                        Value::String("init".to_string()),
                        Value::Fsm(0),
                    ]],
                    fsm_columns: tasks_fsm.clone(),
                }, // INSERT
                QueryResult::Rows {
                    rows: vec![],
                    fsm_columns: vec![None, Some(tasks_fsm_ref())],
                }, // SELECT retries, state WHERE retries >= 1
                QueryResult::Rows {
                    rows: vec![vec![
                        Value::Integer(0),
                        Value::Bool(false),
                        Value::String("init".to_string()),
                        Value::Fsm(0),
                    ]],
                    fsm_columns: tasks_fsm.clone(),
                }, // SELECT * FROM tasks
                QueryResult::Rows {
                    rows: vec![vec![
                        Value::String("current time".to_string()),
                        Value::Integer(1_700_000_000),
                        Value::Integer(16),
                    ]],
                    fsm_columns: vec![None, None, None],
                }, // SELECT 'current time', NOW, 2 * 8
                QueryResult::Rows {
                    rows: vec![],
                    fsm_columns: tasks_fsm.clone(),
                }, // UPDATE WHERE state = 'running'
                QueryResult::Rows {
                    rows: vec![],
                    fsm_columns: tasks_fsm,
                }, // DELETE WHERE state = 'done'
            ]
        );
    }

    #[test]
    fn test_fsm_lifecycle_guard_in_autocommit() {
        let mut db = db_with_clock(100);
        db.execute(TASKS_DDL).unwrap();
        db.execute("INSERT INTO tasks VALUES (0, false, 'job', 'pending');")
            .unwrap();
        db.execute("UPDATE tasks SET retries = 1, state = 'running';")
            .unwrap();

        // Transition 'running' -> 'failed' requires retries >= 3; at retries = 1 it must fail
        // and leave the persisted row untouched.
        assert!(
            matches!(
                db.execute("UPDATE tasks SET state = 'failed';"),
                Err(DbError::GuardFailed(_))
            ),
            "transition to 'failed' with retries = 1 should violate FSM guard"
        );

        let after_failed_attempt = db
            .execute("SELECT retries, state FROM tasks;")
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            after_failed_attempt,
            QueryResult::Rows {
                rows: vec![vec![Value::Integer(1), Value::Fsm(1)]],
                fsm_columns: vec![None, Some(tasks_fsm_ref())],
            }
        );

        // Increment retries to 2, then increment to 3 while transitioning to 'failed'.
        db.execute("UPDATE tasks SET retries = retries + 1;")
            .unwrap();
        db.execute("UPDATE tasks SET retries = retries + 1, state = 'failed';")
            .unwrap();

        let final_rows = db
            .execute("SELECT retries, state FROM tasks;")
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            final_rows,
            QueryResult::Rows {
                rows: vec![vec![Value::Integer(3), Value::Fsm(2)]],
                fsm_columns: vec![None, Some(tasks_fsm_ref())],
            }
        );
    }

    #[test]
    fn test_explicit_transaction_visibility_rollback_and_errors() {
        let mut db = db_with_clock(500);
        db.execute(TASKS_DDL).unwrap();
        db.execute("INSERT INTO tasks VALUES (0, false, 'initial', 'pending');")
            .unwrap();

        // Uncommitted INSERT + UPDATE are visible inside the active transaction.
        let in_tx = db
            .execute(
                "BEGIN;
                 INSERT INTO tasks VALUES (1, false, 'temp', 'pending');
                 UPDATE tasks SET retries = 5, state = 'running' WHERE title = 'initial';
                 SELECT retries, title, state FROM tasks;",
            )
            .unwrap();
        let QueryResult::Rows {
            rows: mut rows_in_tx,
            fsm_columns,
        } = in_tx.into_iter().last().unwrap()
        else {
            panic!("expected Rows from SELECT inside tx");
        };
        assert_eq!(fsm_columns, vec![None, None, Some(tasks_fsm_ref())]);
        rows_in_tx.sort_by_key(|row| match row[0] {
            Value::Integer(n) => n,
            _ => unreachable!(),
        });
        assert_eq!(
            rows_in_tx,
            vec![
                vec![
                    Value::Integer(1),
                    Value::String("temp".to_string()),
                    Value::Fsm(0),
                ],
                vec![
                    Value::Integer(5),
                    Value::String("initial".to_string()),
                    Value::Fsm(1),
                ],
            ]
        );

        // ROLLBACK discards all uncommitted changes.
        let after_rollback = db
            .execute("ROLLBACK; SELECT retries, title, state FROM tasks;")
            .unwrap();
        assert_eq!(
            after_rollback,
            vec![
                QueryResult::Ok,
                QueryResult::Rows {
                    rows: vec![vec![
                        Value::Integer(0),
                        Value::String("initial".to_string()),
                        Value::Fsm(0),
                    ]],
                    fsm_columns: vec![None, None, Some(tasks_fsm_ref())],
                },
            ]
        );

        // COMMIT / ROLLBACK without BEGIN must error.
        assert!(matches!(db.execute("COMMIT;"), Err(DbError::ExecError(_))));
        assert!(matches!(
            db.execute("ROLLBACK;"),
            Err(DbError::ExecError(_))
        ));

        // Double BEGIN and DDL inside a transaction must error.
        db.execute("BEGIN;").unwrap();
        assert!(matches!(db.execute("BEGIN;"), Err(DbError::ExecError(_))));
        assert!(matches!(
            db.execute("CREATE TABLE other (id INTEGER);"),
            Err(DbError::ExecError(_))
        ));
        db.execute("ROLLBACK;").unwrap();

        // Failed COMMIT clears active_tx and leaves storage unchanged.
        db.execute(
            "BEGIN;
             UPDATE tasks SET state = 'done';",
        )
        .unwrap();
        assert!(db.execute("COMMIT;").is_err());
        assert!(matches!(db.execute("COMMIT;"), Err(DbError::ExecError(_))));

        let persisted = db
            .execute("BEGIN; ROLLBACK; SELECT retries, title, state FROM tasks;")
            .unwrap();
        assert_eq!(
            persisted.into_iter().last().unwrap(),
            QueryResult::Rows {
                rows: vec![vec![
                    Value::Integer(0),
                    Value::String("initial".to_string()),
                    Value::Fsm(0),
                ]],
                fsm_columns: vec![None, None, Some(tasks_fsm_ref())],
            }
        );
    }

    #[test]
    fn test_select_without_from_with_mock_clock() {
        let mut db = db_with_clock(123_456);
        let results = db.execute("SELECT 'current time', NOW, 2 * 8;").unwrap();
        assert_eq!(
            results,
            vec![QueryResult::Rows {
                rows: vec![vec![
                    Value::String("current time".to_string()),
                    Value::Integer(123_456),
                    Value::Integer(16),
                ]],
                fsm_columns: vec![None, None, None],
            }]
        );
    }

    #[test]
    fn test_in_memory_constructor_smoke() {
        let mut db = Database::in_memory();
        let results = db
            .execute(
                "CREATE TABLE items (id INTEGER, label STRING);
                 INSERT INTO items VALUES (42, 'ok');
                 SELECT id, label FROM items;",
            )
            .unwrap();
        assert_eq!(
            results,
            vec![
                QueryResult::Ok,
                QueryResult::Rows {
                    rows: vec![vec![Value::Integer(42), Value::String("ok".to_string()),]],
                    fsm_columns: vec![None, None],
                },
                QueryResult::Rows {
                    rows: vec![vec![Value::Integer(42), Value::String("ok".to_string()),]],
                    fsm_columns: vec![None, None],
                },
            ]
        );
    }

    #[test]
    fn test_disk_persistence_across_restarts_e2e() {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};

        static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

        struct TempDir {
            path: PathBuf,
        }

        impl TempDir {
            fn new(prefix: &str) -> Self {
                let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("fsm_db_{prefix}_{}_{id}", std::process::id()));
                let _ = fs::remove_dir_all(&path);
                Self { path }
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        let dir = TempDir::new("db_e2e_persist");
        let short_note = "a".repeat(200);
        let big_note = "b".repeat(1600);
        let grown_note = "g".repeat(1500);

        // Session 1: DDL with FSM + guard, INSERT, UPDATE growing string (forcing page relocation),
        // DELETE, and BEGIN -> INSERT -> ROLLBACK.
        {
            let mut db =
                Database::open_with_clock(&dir.path, Box::new(MockClock { now: 1000 })).unwrap();
            db.execute(
                "CREATE TABLE tasks (
                    id INTEGER,
                    note STRING,
                    retries INTEGER,
                    state FSM (
                        STATES ('pending', 'running', 'failed', 'done'),
                        INITIAL 'pending',
                        TRANSITIONS (
                            'pending' -> 'running',
                            'running' -> 'done',
                            'running' -> 'failed' WHEN retries >= 3
                        )
                    )
                );",
            )
            .unwrap();

            let insert_sql = format!(
                "INSERT INTO tasks VALUES (1, '{short_note}', 0, 'pending');
                 INSERT INTO tasks VALUES (2, '{big_note}', 0, 'pending');
                 INSERT INTO tasks VALUES (3, 'to_delete', 0, 'pending');"
            );
            db.execute(&insert_sql).unwrap();

            let update_sql = format!(
                "UPDATE tasks SET note = '{grown_note}', retries = 1, state = 'running' WHERE id = 1;"
            );
            db.execute(&update_sql).unwrap();
            db.execute("DELETE FROM tasks WHERE id = 3;").unwrap();

            db.execute(
                "BEGIN;
                 INSERT INTO tasks VALUES (99, 'rolled_back', 0, 'pending');
                 ROLLBACK;",
            )
            .unwrap();
        }

        // Session 2: reopen, verify persisted rows and absence of deleted/rolled-back rows,
        // verify FSM guard rejects retries < 3, then perform valid guarded transition.
        {
            let mut db =
                Database::open_with_clock(&dir.path, Box::new(MockClock { now: 2000 })).unwrap();
            let QueryResult::Rows {
                mut rows,
                fsm_columns,
            } = db
                .execute("SELECT id, note, retries, state FROM tasks;")
                .unwrap()
                .pop()
                .unwrap()
            else {
                panic!("expected Rows from SELECT");
            };

            assert_eq!(fsm_columns, vec![None, None, None, Some(tasks_fsm_ref())]);
            rows.sort_by_key(|row| match row[0] {
                Value::Integer(id) => id,
                _ => unreachable!(),
            });
            assert_eq!(
                rows,
                vec![
                    vec![
                        Value::Integer(1),
                        Value::String(grown_note.clone()),
                        Value::Integer(1),
                        Value::Fsm(1),
                    ],
                    vec![
                        Value::Integer(2),
                        Value::String(big_note.clone()),
                        Value::Integer(0),
                        Value::Fsm(0),
                    ],
                ]
            );

            assert!(matches!(
                db.execute("UPDATE tasks SET state = 'failed' WHERE id = 1;"),
                Err(DbError::GuardFailed(_))
            ));

            db.execute("UPDATE tasks SET retries = 3, state = 'failed' WHERE id = 1;")
                .unwrap();
        }

        // Session 3: reopen and confirm the updated FSM state persisted.
        {
            let mut db = Database::open(&dir.path).unwrap();
            let result = db
                .execute("SELECT id, retries, state FROM tasks WHERE id = 1;")
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(
                result,
                QueryResult::Rows {
                    rows: vec![vec![Value::Integer(1), Value::Integer(3), Value::Fsm(2)]],
                    fsm_columns: vec![None, None, Some(tasks_fsm_ref())],
                }
            );
        }
    }
}
