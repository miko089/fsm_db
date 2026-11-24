use crate::{
    binder::{BoundStatement, bind},
    catalog::{Catalog, InMemoryCatalog},
    executor::{ExecContext, Executor},
    expr::{Clock, SystemClock},
    lexer::tokenize,
    parser::parse,
    storage::{InMemoryStorage, Storage},
    tx::Transaction,
    types::{DbError, Row},
};

#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    Ok,
    Rows(Vec<Row>),
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
                    self.catalog.create_table(schema)?;
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
                    results.push(QueryResult::Rows(rows));
                }
            }
        }

        Ok(results)
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

    #[test]
    fn test_example_sql_with_mock_clock() {
        let mut db = db_with_clock(1_700_000_000);
        let results = db.execute(EXAMPLE_SQL).unwrap();

        assert_eq!(
            results,
            vec![
                QueryResult::Ok, // CREATE TABLE tasks
                QueryResult::Ok, // BEGIN
                QueryResult::Ok, // COMMIT
                QueryResult::Ok, // BEGIN
                QueryResult::Ok, // ROLLBACK
                QueryResult::Rows(vec![vec![
                    Value::Integer(0),
                    Value::Bool(false),
                    Value::String("init".to_string()),
                    Value::Fsm(0),
                ]]), // INSERT
                QueryResult::Rows(vec![]), // SELECT retries, state WHERE retries >= 1
                QueryResult::Rows(vec![vec![
                    Value::Integer(0),
                    Value::Bool(false),
                    Value::String("init".to_string()),
                    Value::Fsm(0),
                ]]), // SELECT * FROM tasks
                QueryResult::Rows(vec![vec![
                    Value::String("current time".to_string()),
                    Value::Integer(1_700_000_000),
                    Value::Integer(16),
                ]]), // SELECT 'current time', NOW, 2 * 8
                QueryResult::Rows(vec![]), // UPDATE WHERE state = 'running'
                QueryResult::Rows(vec![]), // DELETE WHERE state = 'done'
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
            QueryResult::Rows(vec![vec![Value::Integer(1), Value::Fsm(1)]])
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
            QueryResult::Rows(vec![vec![Value::Integer(3), Value::Fsm(2)]])
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
        let QueryResult::Rows(mut rows_in_tx) = in_tx.into_iter().last().unwrap() else {
            panic!("expected Rows from SELECT inside tx");
        };
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
                QueryResult::Rows(vec![vec![
                    Value::Integer(0),
                    Value::String("initial".to_string()),
                    Value::Fsm(0),
                ]]),
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
            QueryResult::Rows(vec![vec![
                Value::Integer(0),
                Value::String("initial".to_string()),
                Value::Fsm(0),
            ]])
        );
    }

    #[test]
    fn test_select_without_from_with_mock_clock() {
        let mut db = db_with_clock(123_456);
        let results = db.execute("SELECT 'current time', NOW, 2 * 8;").unwrap();
        assert_eq!(
            results,
            vec![QueryResult::Rows(vec![vec![
                Value::String("current time".to_string()),
                Value::Integer(123_456),
                Value::Integer(16),
            ]])]
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
                QueryResult::Rows(vec![vec![
                    Value::Integer(42),
                    Value::String("ok".to_string()),
                ]]),
                QueryResult::Rows(vec![vec![
                    Value::Integer(42),
                    Value::String("ok".to_string()),
                ]]),
            ]
        );
    }
}
