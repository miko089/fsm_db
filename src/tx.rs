use std::collections::{HashMap, HashSet};

use crate::{
    catalog::{Catalog, TableSchema},
    expr::{Clock, eval_expr},
    storage::Storage,
    types::{ColumnType, DbError, Row, RowId, Value},
};

// marker for rows that are inserted in transaction and are not available in storage
const PENDING_PAGE_ID: u32 = u32::MAX;

#[derive(Debug, Default)]
struct TableWriteSet {
    inserted: HashMap<RowId, Row>,
    updated: HashMap<RowId, Row>,
    deleted: HashSet<RowId>,
}

#[derive(Debug, Default)]
pub(crate) struct Transaction {
    writes: HashMap<String, TableWriteSet>,
    next_pending_id: u16,
}

impl Transaction {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn is_pending(row_id: RowId) -> bool {
        row_id.page_id == PENDING_PAGE_ID
    }

    pub(crate) fn insert(&mut self, table: &str, row: Row) -> RowId {
        let row_id = RowId {
            page_id: PENDING_PAGE_ID,
            row_id: self.next_pending_id,
        };
        self.next_pending_id += 1;

        self.writes
            .entry(table.to_string())
            .or_default()
            .inserted
            .insert(row_id, row);

        row_id
    }

    pub(crate) fn update<S: Storage>(
        &mut self,
        storage: &S,
        table: &str,
        row_id: RowId,
        row: Row,
    ) -> Result<(), DbError> {
        if Self::is_pending(row_id) {
            let existing = self
                .writes
                .get_mut(table)
                .and_then(|ws| ws.inserted.get_mut(&row_id))
                .ok_or(DbError::RowNotFound(row_id))?;
            *existing = row;
            return Ok(());
        }

        if self
            .writes
            .get(table)
            .is_some_and(|ws| ws.deleted.contains(&row_id))
        {
            return Err(DbError::RowNotFound(row_id));
        }

        let exists_in_writes = self
            .writes
            .get(table)
            .is_some_and(|ws| ws.updated.contains_key(&row_id));
        if !exists_in_writes && storage.get(table, row_id)?.is_none() {
            return Err(DbError::RowNotFound(row_id));
        }

        self.writes
            .entry(table.to_string())
            .or_default()
            .updated
            .insert(row_id, row);

        Ok(())
    }

    pub(crate) fn delete<S: Storage>(
        &mut self,
        storage: &S,
        table: &str,
        row_id: RowId,
    ) -> Result<(), DbError> {
        if Self::is_pending(row_id) {
            let removed = self
                .writes
                .get_mut(table)
                .and_then(|ws| ws.inserted.remove(&row_id))
                .is_some();
            if !removed {
                unreachable!()
            }
            return Ok(());
        }

        if self
            .writes
            .get(table)
            .is_some_and(|ws| ws.deleted.contains(&row_id))
        {
            return Err(DbError::RowNotFound(row_id));
        }

        if storage.get(table, row_id)?.is_none() {
            return Err(DbError::RowNotFound(row_id));
        }

        let ws = self.writes.entry(table.to_string()).or_default();
        ws.updated.remove(&row_id);
        ws.deleted.insert(row_id);

        Ok(())
    }

    pub(crate) fn get<S: Storage>(
        &self,
        storage: &S,
        table: &str,
        row_id: RowId,
    ) -> Result<Option<Row>, DbError> {
        if Self::is_pending(row_id) {
            return Ok(self
                .writes
                .get(table)
                .and_then(|ws| ws.inserted.get(&row_id))
                .cloned());
        }

        if let Some(ws) = self.writes.get(table) {
            if ws.deleted.contains(&row_id) {
                return Ok(None);
            }
            if let Some(row) = ws.updated.get(&row_id) {
                return Ok(Some(row.clone()));
            }
        }

        storage.get(table, row_id)
    }

    pub(crate) fn scan<S: Storage>(
        &self,
        storage: &S,
        table: &str,
    ) -> Result<Vec<(RowId, Row)>, DbError> {
        let mut rows = storage.scan(table)?;
        if let Some(ws) = self.writes.get(table) {
            rows.retain(|(id, _)| !ws.deleted.contains(id));
            for (id, row) in &mut rows {
                if let Some(updated_row) = ws.updated.get(id) {
                    *row = updated_row.clone();
                }
            }
            rows.extend(ws.inserted.iter().map(|(id, row)| (*id, row.clone())));
        }
        Ok(rows)
    }

    pub(crate) fn rollback(&mut self) {
        self.writes.clear();
        self.next_pending_id = 0;
    }

    pub(crate) fn commit<S: Storage, C: Catalog>(
        &mut self,
        storage: &mut S,
        catalog: &C,
        clock: &dyn Clock,
    ) -> Result<(), DbError> {
        for (table_name, ws) in &self.writes {
            let schema = catalog
                .get_table(table_name)?
                .ok_or_else(|| DbError::TableNotFound(table_name.clone()))?;

            Self::validate_table_writes(storage, schema, ws, clock)?;
        }

        for (table_name, ws) in self.writes.drain() {
            for row_id in ws.deleted {
                storage.delete(&table_name, row_id)?;
            }
            for (row_id, row) in ws.updated {
                storage.update(&table_name, row_id, row)?;
            }
            for (_, row) in ws.inserted {
                storage.insert(&table_name, row)?;
            }
        }
        self.next_pending_id = 0;

        Ok(())
    }

    fn validate_table_writes<S: Storage>(
        storage: &S,
        schema: &TableSchema,
        ws: &TableWriteSet,
        clock: &dyn Clock,
    ) -> Result<(), DbError> {
        for row in ws.inserted.values() {
            if row.len() != schema.columns.len() {
                return Err(DbError::InvalidSchema(format!(
                    "row length {} does not match table {} column count {}",
                    row.len(),
                    schema.name,
                    schema.columns.len()
                )));
            }
            for (col_idx, col) in schema.columns.iter().enumerate() {
                if let ColumnType::Fsm(fsm_idx) = col.column_type {
                    let Value::Fsm(state) = row[col_idx] else {
                        return Err(DbError::InvalidSchema(format!(
                            "expected fsm value for column {}",
                            col.name
                        )));
                    };
                    let fsm = &schema.fsms[fsm_idx as usize];
                    if state != fsm.initial {
                        return Err(DbError::InvalidSchema(format!(
                            "inserted row has non-initial fsm state {} (expected {})",
                            state, fsm.initial
                        )));
                    }
                }
            }
        }

        for (row_id, new_row) in &ws.updated {
            if new_row.len() != schema.columns.len() {
                return Err(DbError::InvalidSchema(format!(
                    "row length {} does not match table {} column count {}",
                    new_row.len(),
                    schema.name,
                    schema.columns.len()
                )));
            }
            let old_row = storage
                .get(&schema.name, *row_id)?
                .ok_or(DbError::RowNotFound(*row_id))?;

            for (col_idx, col) in schema.columns.iter().enumerate() {
                if let ColumnType::Fsm(fsm_idx) = col.column_type {
                    let Value::Fsm(old_state) = old_row[col_idx] else {
                        return Err(DbError::InvalidSchema(format!(
                            "expected fsm value for column {}",
                            col.name
                        )));
                    };
                    let Value::Fsm(new_state) = new_row[col_idx] else {
                        return Err(DbError::InvalidSchema(format!(
                            "expected fsm value for column {}",
                            col.name
                        )));
                    };

                    if old_state != new_state {
                        let fsm = &schema.fsms[fsm_idx as usize];
                        let guard = fsm.allowed.get(&(old_state, new_state)).ok_or_else(|| {
                            DbError::InvalidSchema(format!(
                                "disallowed fsm transition ({} -> {}) on table {}",
                                old_state, new_state, schema.name
                            ))
                        })?;
                        match eval_expr(guard, new_row, clock)? {
                            Value::Bool(true) => {}
                            _ => return Err(DbError::EvalError(guard.clone())),
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Column, InMemoryCatalog, TableSchema};
    use crate::expr::{BinaryOp, Expr, MockClock};
    use crate::storage::InMemoryStorage;
    use crate::types::{ColumnType, Fsm, Value};

    const TABLE: &str = "tasks";
    const PENDING: u16 = 0;
    const RUNNING: u16 = 1;
    const FAILED: u16 = 2;
    const DONE: u16 = 3;

    fn setup() -> (InMemoryStorage, InMemoryCatalog, MockClock) {
        let storage = InMemoryStorage::new();
        let mut catalog = InMemoryCatalog::new();

        let always = Expr::Literal(Value::Bool(true));
        let retries_ge_3 = Expr::Binary {
            left: Box::new(Expr::Column(0)),
            op: BinaryOp::Ge,
            right: Box::new(Expr::Literal(Value::Integer(3))),
        };

        let mut allowed = HashMap::new();
        allowed.insert((PENDING, RUNNING), always.clone());
        allowed.insert((RUNNING, DONE), always);
        allowed.insert((RUNNING, FAILED), retries_ge_3);

        let schema = TableSchema {
            name: TABLE.to_string(),
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

    fn task_row(retries: i64, state: u16) -> Row {
        vec![Value::Integer(retries), Value::Fsm(state)]
    }

    #[test]
    fn test_read_your_own_writes_and_no_steal() {
        let (mut storage, catalog, clock) = setup();
        let mut tx = Transaction::new();

        let pending_id = tx.insert(TABLE, task_row(0, PENDING));
        assert_eq!(tx.scan(&storage, TABLE).unwrap().len(), 1);
        assert_eq!(storage.scan(TABLE).unwrap().len(), 0);

        tx.update(&storage, TABLE, pending_id, task_row(2, PENDING))
            .unwrap();
        assert_eq!(
            tx.get(&storage, TABLE, pending_id).unwrap(),
            Some(task_row(2, PENDING))
        );

        tx.commit(&mut storage, &catalog, &clock).unwrap();
        let committed = storage.scan(TABLE).unwrap();
        assert_eq!(committed.len(), 1);
        assert_eq!(committed[0].1, task_row(2, PENDING));

        let real_id = committed[0].0;
        let mut tx2 = Transaction::new();
        tx2.delete(&storage, TABLE, real_id).unwrap();
        assert!(tx2.get(&storage, TABLE, real_id).unwrap().is_none());
        assert_eq!(tx2.scan(&storage, TABLE).unwrap().len(), 0);
        assert_eq!(storage.scan(TABLE).unwrap().len(), 1);

        tx2.commit(&mut storage, &catalog, &clock).unwrap();
        assert_eq!(storage.scan(TABLE).unwrap().len(), 0);
    }

    #[test]
    fn test_commit_checks_only_final_state_not_intermediate() {
        let (mut storage, catalog, clock) = setup();
        let row_id = storage.insert(TABLE, task_row(0, PENDING)).unwrap();

        let mut tx = Transaction::new();
        tx.update(&storage, TABLE, row_id, task_row(0, DONE))
            .unwrap();
        tx.update(&storage, TABLE, row_id, task_row(1, RUNNING))
            .unwrap();

        tx.commit(&mut storage, &catalog, &clock).unwrap();
        assert_eq!(
            storage.get(TABLE, row_id).unwrap(),
            Some(task_row(1, RUNNING))
        );
    }

    #[test]
    fn test_commit_invalid_transition_is_atomic() {
        let (mut storage, catalog, clock) = setup();
        let id1 = storage.insert(TABLE, task_row(0, PENDING)).unwrap();
        let id2 = storage.insert(TABLE, task_row(0, PENDING)).unwrap();

        let mut tx = Transaction::new();
        tx.update(&storage, TABLE, id1, task_row(0, RUNNING))
            .unwrap();
        tx.update(&storage, TABLE, id2, task_row(0, DONE)).unwrap();

        assert!(tx.commit(&mut storage, &catalog, &clock).is_err());

        assert_eq!(storage.get(TABLE, id1).unwrap(), Some(task_row(0, PENDING)));
        assert_eq!(storage.get(TABLE, id2).unwrap(), Some(task_row(0, PENDING)));
    }

    #[test]
    fn test_commit_fsm_guard_fail_and_pass() {
        let (mut storage, catalog, clock) = setup();
        let row_id = storage.insert(TABLE, task_row(1, RUNNING)).unwrap();

        let mut tx_fail = Transaction::new();
        tx_fail
            .update(&storage, TABLE, row_id, task_row(2, FAILED))
            .unwrap();
        assert!(tx_fail.commit(&mut storage, &catalog, &clock).is_err());
        assert_eq!(
            storage.get(TABLE, row_id).unwrap(),
            Some(task_row(1, RUNNING))
        );

        let mut tx_ok = Transaction::new();
        tx_ok
            .update(&storage, TABLE, row_id, task_row(3, FAILED))
            .unwrap();
        tx_ok.commit(&mut storage, &catalog, &clock).unwrap();
        assert_eq!(
            storage.get(TABLE, row_id).unwrap(),
            Some(task_row(3, FAILED))
        );
    }

    #[test]
    fn test_commit_insert_disallowed_non_initial_state() {
        let (mut storage, catalog, clock) = setup();
        let mut tx = Transaction::new();

        tx.insert(TABLE, task_row(0, DONE));
        assert!(tx.commit(&mut storage, &catalog, &clock).is_err());
        assert_eq!(storage.scan(TABLE).unwrap().len(), 0);
    }

    #[test]
    fn test_rollback_discards_all_changes() {
        let (mut storage, catalog, clock) = setup();
        let row_id = storage.insert(TABLE, task_row(0, PENDING)).unwrap();

        let mut tx = Transaction::new();
        tx.insert(TABLE, task_row(0, PENDING));
        tx.update(&storage, TABLE, row_id, task_row(5, RUNNING))
            .unwrap();
        tx.rollback();

        assert_eq!(tx.scan(&storage, TABLE).unwrap().len(), 1);
        assert_eq!(
            tx.get(&storage, TABLE, row_id).unwrap(),
            Some(task_row(0, PENDING))
        );

        tx.commit(&mut storage, &catalog, &clock).unwrap();
        assert_eq!(storage.scan(TABLE).unwrap().len(), 1);
        assert_eq!(
            storage.get(TABLE, row_id).unwrap(),
            Some(task_row(0, PENDING))
        );
    }
}
