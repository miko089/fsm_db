use std::vec::IntoIter;

use crate::{
    expr::{Clock, Expr, eval_expr, eval_predicate},
    storage::Storage,
    tx::Transaction,
    types::{DbError, Row, RowId},
};

pub(crate) enum Executor {
    SeqScan {
        table: String,
        rows: Option<IntoIter<(RowId, Row)>>,
    },
    Filter {
        input: Box<Executor>,
        predicate: Expr,
    },
    Project {
        input: Box<Executor>,
        cols: Vec<Expr>,
    },
    Insert {
        table: String,
        rows: IntoIter<Row>,
    },
    Update {
        table: String,
        input: Box<Executor>,
        sets: Vec<(usize, Expr)>,
        results: Option<IntoIter<(RowId, Row)>>,
    },
    Delete {
        table: String,
        input: Box<Executor>,
        results: Option<IntoIter<(RowId, Row)>>,
    },
    Limit {
        input: Box<Executor>,
        remaining: usize,
    },
}

pub(crate) struct ExecContext<'a, S: Storage> {
    pub(crate) tx: &'a mut Transaction,
    pub(crate) storage: &'a S,
    pub(crate) clock: &'a dyn Clock,
}

impl Executor {
    pub(crate) fn next<S: Storage>(
        &mut self,
        ctx: &mut ExecContext<S>,
    ) -> Result<Option<(RowId, Row)>, DbError> {
        match self {
            Executor::SeqScan { table, rows } => {
                if rows.is_none() {
                    *rows = Some(ctx.tx.scan(ctx.storage, table)?.into_iter());
                }
                Ok(rows.as_mut().and_then(|iter| iter.next()))
            }
            Executor::Filter { input, predicate } => {
                while let Some((row_id, row)) = input.next(ctx)? {
                    if eval_predicate(predicate, &row, ctx.clock)? {
                        return Ok(Some((row_id, row)));
                    }
                }
                Ok(None)
            }
            Executor::Project { input, cols } => {
                let Some((row_id, row)) = input.next(ctx)? else {
                    return Ok(None);
                };
                let projected = cols
                    .iter()
                    .map(|expr| eval_expr(expr, &row, ctx.clock))
                    .collect::<Result<Row, _>>()?;
                Ok(Some((row_id, projected)))
            }
            Executor::Insert { table, rows } => {
                let Some(row) = rows.next() else {
                    return Ok(None);
                };
                let row_id = ctx.tx.insert(table, row.clone());
                Ok(Some((row_id, row)))
            }
            Executor::Update {
                table,
                input,
                sets,
                results,
            } => {
                if results.is_none() {
                    let mut staged = Vec::new();
                    while let Some((row_id, row)) = input.next(ctx)? {
                        let mut new_row = row.clone();
                        for (col_idx, expr) in sets.iter() {
                            let val = eval_expr(expr, &row, ctx.clock)?;
                            let slot = new_row
                                .get_mut(*col_idx)
                                .ok_or_else(|| DbError::EvalError(expr.clone()))?;
                            *slot = val;
                        }
                        staged.push((row_id, new_row));
                    }
                    for (row_id, new_row) in &staged {
                        ctx.tx
                            .update(ctx.storage, table, *row_id, new_row.clone())?;
                    }
                    *results = Some(staged.into_iter());
                }
                Ok(results.as_mut().and_then(|iter| iter.next()))
            }
            Executor::Delete {
                table,
                input,
                results,
            } => {
                if results.is_none() {
                    let mut staged = Vec::new();
                    while let Some(item) = input.next(ctx)? {
                        staged.push(item);
                    }
                    let mut deleted = Vec::with_capacity(staged.len());
                    for (row_id, row) in staged {
                        ctx.tx.delete(ctx.storage, table, row_id)?;
                        deleted.push((row_id, row));
                    }
                    *results = Some(deleted.into_iter());
                }
                Ok(results.as_mut().and_then(|iter| iter.next()))
            }
            Executor::Limit { input, remaining } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                if let Some(row_pair) = input.next(ctx)? {
                    *remaining -= 1;
                    Ok(Some(row_pair))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        catalog::{Catalog, Column, InMemoryCatalog, TableSchema},
        expr::{BinaryOp, MockClock},
        storage::InMemoryStorage,
        types::{ColumnType, Fsm, Value},
    };

    const TABLE: &str = "tasks";
    const PENDING: u16 = 0;
    const RUNNING: u16 = 1;
    const FAILED: u16 = 2;
    const DONE: u16 = 3;

    fn setup() -> (InMemoryStorage, InMemoryCatalog, MockClock) {
        let storage = InMemoryStorage::new();
        let mut catalog = InMemoryCatalog::new();

        let always = Expr::Literal(Value::Bool(true));
        let a_ge_3 = Expr::Binary {
            left: Box::new(Expr::Column(0)),
            op: BinaryOp::Ge,
            right: Box::new(Expr::Literal(Value::Integer(3))),
        };

        let mut allowed = HashMap::new();
        allowed.insert((PENDING, RUNNING), always.clone());
        allowed.insert((RUNNING, DONE), always);
        allowed.insert((RUNNING, FAILED), a_ge_3);

        let schema = TableSchema {
            name: TABLE.to_string(),
            columns: vec![
                Column {
                    name: "a".to_string(),
                    column_type: ColumnType::Integer,
                },
                Column {
                    name: "b".to_string(),
                    column_type: ColumnType::Integer,
                },
                Column {
                    name: "state".to_string(),
                    column_type: ColumnType::Fsm(0),
                },
            ],
            fsms: vec![Fsm {
                states: vec![
                    "pending".into(),
                    "running".into(),
                    "failed".into(),
                    "done".into(),
                ],
                initial: PENDING,
                allowed,
            }],
        };

        catalog.create_table(schema).unwrap();
        (storage, catalog, MockClock { now: 1000 })
    }

    fn task_row(a: i64, b: i64, state: u16) -> Row {
        vec![Value::Integer(a), Value::Integer(b), Value::Fsm(state)]
    }

    fn collect_all<S: Storage>(
        exec: &mut Executor,
        ctx: &mut ExecContext<S>,
    ) -> Result<Vec<(RowId, Row)>, DbError> {
        let mut out = Vec::new();
        while let Some(item) = exec.next(ctx)? {
            out.push(item);
        }
        Ok(out)
    }

    #[test]
    fn test_project_filter_seq_scan_read_your_own_writes() {
        let (mut storage, _catalog, clock) = setup();
        let stored_match = storage.insert(TABLE, task_row(10, 1, RUNNING)).unwrap();
        let _stored_skip = storage.insert(TABLE, task_row(2, 1, RUNNING)).unwrap();

        let mut tx = Transaction::new();
        let tx_match = tx.insert(TABLE, task_row(7, 0, PENDING));
        let _tx_skip = tx.insert(TABLE, task_row(1, 0, PENDING));

        let mut plan = Executor::Project {
            input: Box::new(Executor::Filter {
                input: Box::new(Executor::SeqScan {
                    table: TABLE.to_string(),
                    rows: None,
                }),
                predicate: Expr::Binary {
                    op: BinaryOp::Ge,
                    left: Box::new(Expr::Column(0)),
                    right: Box::new(Expr::Literal(Value::Integer(5))),
                },
            }),
            cols: vec![
                Expr::Column(0),
                Expr::Binary {
                    op: BinaryOp::Gt,
                    left: Box::new(Expr::Column(1)),
                    right: Box::new(Expr::Literal(Value::Integer(0))),
                },
            ],
        };

        let mut ctx = ExecContext {
            tx: &mut tx,
            storage: &storage,
            clock: &clock,
        };
        let mut results = collect_all(&mut plan, &mut ctx).unwrap();
        results.sort_by_key(|(id, _)| *id);

        let mut expected = vec![
            (stored_match, vec![Value::Integer(10), Value::Bool(true)]),
            (tx_match, vec![Value::Integer(7), Value::Bool(false)]),
        ];
        expected.sort_by_key(|(id, _)| *id);

        assert_eq!(results, expected);
        assert!(results.iter().all(|(_, row)| row.len() == 2));
    }

    #[test]
    fn test_insert_and_no_steal() {
        let (mut storage, catalog, clock) = setup();
        let mut tx = Transaction::new();

        let r1 = task_row(1, 10, PENDING);
        let r2 = task_row(2, 20, PENDING);
        let r3 = task_row(3, 30, PENDING);

        let mut insert = Executor::Insert {
            table: TABLE.to_string(),
            rows: vec![r1.clone(), r2.clone(), r3.clone()].into_iter(),
        };

        let mut inserted = Vec::new();
        {
            let mut ctx = ExecContext {
                tx: &mut tx,
                storage: &storage,
                clock: &clock,
            };
            while let Some(item) = insert.next(&mut ctx).unwrap() {
                inserted.push(item);
            }
            assert_eq!(insert.next(&mut ctx).unwrap(), None);
        }

        assert_eq!(inserted.len(), 3);
        let inserted_rows: Vec<Row> = inserted.iter().map(|(_, r)| r.clone()).collect();
        assert_eq!(inserted_rows, vec![r1.clone(), r2.clone(), r3.clone()]);

        // inside the same transaction `SeqScan` sees all inserted rows, while `Storage` stays empty
        let mut scan = Executor::SeqScan {
            table: TABLE.to_string(),
            rows: None,
        };
        let mut ctx = ExecContext {
            tx: &mut tx,
            storage: &storage,
            clock: &clock,
        };
        let mut visible = collect_all(&mut scan, &mut ctx).unwrap();
        visible.sort_by_key(|(id, _)| *id);
        inserted.sort_by_key(|(id, _)| *id);
        assert_eq!(visible, inserted);
        assert!(storage.scan(TABLE).unwrap().is_empty());

        // after commit, rows appear in `Storage`
        tx.commit(&mut storage, &catalog, &clock).unwrap();
        let stored: Vec<Row> = storage
            .scan(TABLE)
            .unwrap()
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        assert_eq!(stored.len(), 3);
        assert!(stored.contains(&r1));
        assert!(stored.contains(&r2));
        assert!(stored.contains(&r3));
    }

    #[test]
    fn test_update_filter_seq_scan_set_semantics_and_fsm_guard() {
        let (mut storage, catalog, clock) = setup();
        // Filtered rows (state == RUNNING):
        // - invalid_id: after swap a = b, b = a -> a = 2 < 3, violates RUNNING -> FAILED guard
        // - valid_id:   after swap a = b, b = a -> a = 5 >= 3, satisfies RUNNING -> FAILED guard
        // Unfiltered row (state == PENDING): untouched by Update
        let invalid_id = storage.insert(TABLE, task_row(1, 2, RUNNING)).unwrap();
        let valid_id = storage.insert(TABLE, task_row(1, 5, RUNNING)).unwrap();
        let untouched_id = storage.insert(TABLE, task_row(9, 9, PENDING)).unwrap();

        let make_update_plan = || Executor::Update {
            table: TABLE.to_string(),
            input: Box::new(Executor::Filter {
                input: Box::new(Executor::SeqScan {
                    table: TABLE.to_string(),
                    rows: None,
                }),
                predicate: Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(Expr::Column(2)),
                    right: Box::new(Expr::Literal(Value::Fsm(RUNNING))),
                },
            }),
            sets: vec![
                (0, Expr::Column(1)),
                (1, Expr::Column(0)),
                (2, Expr::Literal(Value::Fsm(FAILED))),
            ],
            results: None,
        };

        let mut tx_fail = Transaction::new();
        {
            let mut plan = make_update_plan();
            let mut ctx = ExecContext {
                tx: &mut tx_fail,
                storage: &storage,
                clock: &clock,
            };
            let mut updated = collect_all(&mut plan, &mut ctx).unwrap();
            updated.sort_by_key(|(id, _)| *id);

            let mut expected = vec![
                (invalid_id, task_row(2, 1, FAILED)),
                (valid_id, task_row(5, 1, FAILED)),
            ];
            expected.sort_by_key(|(id, _)| *id);
            assert_eq!(updated, expected);
        }

        assert!(tx_fail.commit(&mut storage, &catalog, &clock).is_err());
        assert_eq!(
            storage.get(TABLE, invalid_id).unwrap(),
            Some(task_row(1, 2, RUNNING))
        );
        assert_eq!(
            storage.get(TABLE, valid_id).unwrap(),
            Some(task_row(1, 5, RUNNING))
        );
        assert_eq!(
            storage.get(TABLE, untouched_id).unwrap(),
            Some(task_row(9, 9, PENDING))
        );

        let mut tx_ok = Transaction::new();
        {
            let mut plan = Executor::Update {
                table: TABLE.to_string(),
                input: Box::new(Executor::Filter {
                    input: Box::new(Executor::SeqScan {
                        table: TABLE.to_string(),
                        rows: None,
                    }),
                    predicate: Expr::Binary {
                        op: BinaryOp::Eq,
                        left: Box::new(Expr::Column(1)),
                        right: Box::new(Expr::Literal(Value::Integer(5))),
                    },
                }),
                sets: vec![
                    (0, Expr::Column(1)),
                    (1, Expr::Column(0)),
                    (2, Expr::Literal(Value::Fsm(FAILED))),
                ],
                results: None,
            };
            let mut ctx = ExecContext {
                tx: &mut tx_ok,
                storage: &storage,
                clock: &clock,
            };
            let updated = collect_all(&mut plan, &mut ctx).unwrap();
            assert_eq!(updated, vec![(valid_id, task_row(5, 1, FAILED))]);
        }

        tx_ok.commit(&mut storage, &catalog, &clock).unwrap();
        assert_eq!(
            storage.get(TABLE, valid_id).unwrap(),
            Some(task_row(5, 1, FAILED))
        );
        assert_eq!(
            storage.get(TABLE, invalid_id).unwrap(),
            Some(task_row(1, 2, RUNNING))
        );
        assert_eq!(
            storage.get(TABLE, untouched_id).unwrap(),
            Some(task_row(9, 9, PENDING))
        );
    }

    #[test]
    fn test_delete_filter_seq_scan() {
        let (mut storage, catalog, clock) = setup();
        let stored_del_id = storage.insert(TABLE, task_row(10, 0, PENDING)).unwrap();
        let stored_keep_id = storage.insert(TABLE, task_row(1, 0, PENDING)).unwrap();

        let mut tx = Transaction::new();
        let tx_del_id = tx.insert(TABLE, task_row(20, 0, PENDING));

        let mut delete = Executor::Delete {
            table: TABLE.to_string(),
            input: Box::new(Executor::Filter {
                input: Box::new(Executor::SeqScan {
                    table: TABLE.to_string(),
                    rows: None,
                }),
                predicate: Expr::Binary {
                    op: BinaryOp::Ge,
                    left: Box::new(Expr::Column(0)),
                    right: Box::new(Expr::Literal(Value::Integer(10))),
                },
            }),
            results: None,
        };

        {
            let mut ctx = ExecContext {
                tx: &mut tx,
                storage: &storage,
                clock: &clock,
            };
            let mut deleted = collect_all(&mut delete, &mut ctx).unwrap();
            deleted.sort_by_key(|(id, _)| *id);

            let mut expected = vec![
                (stored_del_id, task_row(10, 0, PENDING)),
                (tx_del_id, task_row(20, 0, PENDING)),
            ];
            expected.sort_by_key(|(id, _)| *id);
            assert_eq!(deleted, expected);

            // subsequent `SeqScan` in the same transaction sees only the surviving row
            let mut scan = Executor::SeqScan {
                table: TABLE.to_string(),
                rows: None,
            };
            let remaining = collect_all(&mut scan, &mut ctx).unwrap();
            assert_eq!(remaining, vec![(stored_keep_id, task_row(1, 0, PENDING))]);
        }

        // before commit, both original rows are still in Storage
        assert_eq!(storage.scan(TABLE).unwrap().len(), 2);

        // after commit, only the non-deleted row remains in Storage
        tx.commit(&mut storage, &catalog, &clock).unwrap();
        assert_eq!(
            storage.scan(TABLE).unwrap(),
            vec![(stored_keep_id, task_row(1, 0, PENDING))]
        );
    }

    #[test]
    fn test_limit() {
        let (mut storage, _catalog, clock) = setup();
        storage.insert(TABLE, task_row(1, 0, PENDING)).unwrap();
        storage.insert(TABLE, task_row(2, 0, PENDING)).unwrap();
        storage.insert(TABLE, task_row(3, 0, PENDING)).unwrap();

        let mut tx = Transaction::new();
        let mut plan = Executor::Limit {
            input: Box::new(Executor::SeqScan {
                table: TABLE.to_string(),
                rows: None,
            }),
            remaining: 2,
        };

        let mut ctx = ExecContext {
            tx: &mut tx,
            storage: &storage,
            clock: &clock,
        };
        let results = collect_all(&mut plan, &mut ctx).unwrap();
        assert_eq!(results.len(), 2);
    }
}
