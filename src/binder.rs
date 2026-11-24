use std::collections::{HashMap, HashSet};

use crate::{
    catalog::{Catalog, Column, TableSchema},
    executor::Executor,
    expr::{BinaryOp, Expr, MockClock, eval_expr},
    parser::{AstExpr, ColumnDef, ColumnTypeAst, SelectItem, Statement, TransitionDef},
    types::{ColumnType, DbError, Fsm, Value},
};

pub(crate) enum BoundStatement {
    CreateTable(TableSchema),
    Begin,
    Commit,
    Rollback,
    Execute(Executor),
}

pub(crate) fn bind<C: Catalog>(stmt: Statement, catalog: &C) -> Result<BoundStatement, DbError> {
    match stmt {
        Statement::CreateTable { name, columns } => bind_create_table(name, columns),
        Statement::Begin => Ok(BoundStatement::Begin),
        Statement::Commit => Ok(BoundStatement::Commit),
        Statement::Rollback => Ok(BoundStatement::Rollback),
        Statement::Select {
            items,
            from,
            filter,
            limit,
        } => bind_select(items, from, filter, limit, catalog),
        Statement::Insert { table, values } => bind_insert(table, values, catalog),
        Statement::Update {
            table,
            sets,
            filter,
        } => bind_update(table, sets, filter, catalog),
        Statement::Delete { table, filter } => bind_delete(table, filter, catalog),
    }
}

fn find_column<'a>(schema: &'a TableSchema, name: &str) -> Result<(usize, &'a Column), DbError> {
    schema
        .columns
        .iter()
        .enumerate()
        .find(|(_, col)| col.name == name)
        .ok_or_else(|| {
            DbError::BindError(format!(
                "column '{name}' not found in table '{}'",
                schema.name
            ))
        })
}

fn fsm_idx_for_col(schema: &TableSchema, col_idx: usize) -> Option<u32> {
    match schema.columns.get(col_idx)?.column_type {
        ColumnType::Fsm(fsm_idx) => Some(fsm_idx),
        _ => None,
    }
}

fn resolve_fsm_state(schema: &TableSchema, fsm_idx: u32, state: &str) -> Result<u16, DbError> {
    let fsm = schema.fsms.get(fsm_idx as usize).ok_or_else(|| {
        DbError::BindError(format!(
            "fsm index {fsm_idx} out of bounds in table '{}'",
            schema.name
        ))
    })?;
    let idx = fsm.states.iter().position(|s| s == state).ok_or_else(|| {
        DbError::BindError(format!(
            "unknown FSM state '{state}' in table '{}'",
            schema.name
        ))
    })?;
    u16::try_from(idx).map_err(|_| DbError::BindError("FSM state index overflow".to_string()))
}

fn contains_current_time(expr: &Expr) -> bool {
    match expr {
        Expr::CurrentTime => true,
        Expr::Literal(_) | Expr::Column(_) => false,
        Expr::IsNull(inner) | Expr::Not(inner) => contains_current_time(inner),
        Expr::Binary { left, right, .. } => {
            contains_current_time(left) || contains_current_time(right)
        }
    }
}

fn bind_expr(expr: AstExpr, schema: Option<&TableSchema>) -> Result<Expr, DbError> {
    match expr {
        AstExpr::Literal(v) => Ok(Expr::Literal(v)),
        AstExpr::Ident(name) => match schema {
            Some(schema) => {
                let (idx, _) = find_column(schema, &name)?;
                Ok(Expr::Column(idx))
            }
            None => Err(DbError::BindError(format!(
                "column '{name}' cannot be referenced without FROM clause"
            ))),
        },
        AstExpr::CurrentTime => Ok(Expr::CurrentTime),
        AstExpr::IsNull(inner) => Ok(Expr::IsNull(Box::new(bind_expr(*inner, schema)?))),
        AstExpr::Not(inner) => Ok(Expr::Not(Box::new(bind_expr(*inner, schema)?))),
        AstExpr::Neg(inner) => {
            let bound = bind_expr(*inner, schema)?;
            match bound {
                Expr::Literal(Value::Integer(n)) => n
                    .checked_neg()
                    .map(|v| Expr::Literal(Value::Integer(v)))
                    .ok_or_else(|| DbError::BindError(format!("integer negation overflow: -{n}"))),
                Expr::Literal(Value::Float(f)) => Ok(Expr::Literal(Value::Float(-f))),
                other => {
                    let is_float_col = matches!(
                        other,
                        Expr::Column(idx)
                            if schema
                                .and_then(|s| s.columns.get(idx))
                                .is_some_and(|c| c.column_type == ColumnType::Float)
                    );
                    let zero = if is_float_col {
                        Value::Float(0.0)
                    } else {
                        Value::Integer(0)
                    };
                    Ok(Expr::Binary {
                        op: BinaryOp::Sub,
                        left: Box::new(Expr::Literal(zero)),
                        right: Box::new(other),
                    })
                }
            }
        }
        AstExpr::Binary { op, left, right } => {
            let mut bound_left = bind_expr(*left, schema)?;
            let mut bound_right = bind_expr(*right, schema)?;

            if matches!(op, BinaryOp::Eq | BinaryOp::Ne)
                && let Some(schema) = schema
            {
                if let (Expr::Column(col_idx), Expr::Literal(Value::String(state))) =
                    (&bound_left, &bound_right)
                    && let Some(fsm_idx) = fsm_idx_for_col(schema, *col_idx)
                {
                    let state_id = resolve_fsm_state(schema, fsm_idx, state)?;
                    bound_right = Expr::Literal(Value::Fsm(state_id));
                } else if let (Expr::Literal(Value::String(state)), Expr::Column(col_idx)) =
                    (&bound_left, &bound_right)
                    && let Some(fsm_idx) = fsm_idx_for_col(schema, *col_idx)
                {
                    let state_id = resolve_fsm_state(schema, fsm_idx, state)?;
                    bound_left = Expr::Literal(Value::Fsm(state_id));
                }
            }

            Ok(Expr::Binary {
                op,
                left: Box::new(bound_left),
                right: Box::new(bound_right),
            })
        }
    }
}

fn bind_create_table(name: String, col_defs: Vec<ColumnDef>) -> Result<BoundStatement, DbError> {
    let mut seen_cols = HashSet::with_capacity(col_defs.len());
    let mut columns = Vec::with_capacity(col_defs.len());
    let mut fsms = Vec::new();
    let mut pending_transitions: Vec<(usize, Vec<TransitionDef>)> = Vec::new();

    for col_def in col_defs {
        if !seen_cols.insert(col_def.name.clone()) {
            return Err(DbError::InvalidSchema(format!(
                "duplicate column name: {}",
                col_def.name
            )));
        }

        let column_type = match col_def.col_type {
            ColumnTypeAst::Integer => ColumnType::Integer,
            ColumnTypeAst::Float => ColumnType::Float,
            ColumnTypeAst::Bool => ColumnType::Bool,
            ColumnTypeAst::String => ColumnType::String,
            ColumnTypeAst::Fsm(fsm_def) => {
                if fsm_def.states.is_empty() {
                    return Err(DbError::InvalidSchema(format!(
                        "fsm column '{}' has no states",
                        col_def.name
                    )));
                }

                let mut state_map = HashMap::with_capacity(fsm_def.states.len());
                for (idx, state_name) in fsm_def.states.iter().enumerate() {
                    let state_id = u16::try_from(idx).map_err(|_| {
                        DbError::InvalidSchema(format!(
                            "too many states in fsm column '{}'",
                            col_def.name
                        ))
                    })?;
                    if state_map.insert(state_name.as_str(), state_id).is_some() {
                        return Err(DbError::InvalidSchema(format!(
                            "duplicate state name '{}' in fsm column '{}'",
                            state_name, col_def.name
                        )));
                    }
                }

                let initial = *state_map.get(fsm_def.initial.as_str()).ok_or_else(|| {
                    DbError::InvalidSchema(format!(
                        "unknown initial state '{}' in fsm column '{}'",
                        fsm_def.initial, col_def.name
                    ))
                })?;

                let fsm_idx = fsms.len();
                let fsm_idx_u32 = u32::try_from(fsm_idx).map_err(|_| {
                    DbError::InvalidSchema("too many fsm columns in table".to_string())
                })?;

                fsms.push(Fsm {
                    states: fsm_def.states,
                    initial,
                    allowed: HashMap::new(),
                });
                pending_transitions.push((fsm_idx, fsm_def.transitions));
                ColumnType::Fsm(fsm_idx_u32)
            }
        };

        columns.push(Column {
            name: col_def.name,
            column_type,
        });
    }

    let mut schema = TableSchema {
        name,
        columns,
        fsms,
    };

    for (fsm_idx, transitions) in pending_transitions {
        let mut allowed = HashMap::with_capacity(transitions.len());
        let find_state = |state_name: &str| -> Result<u16, DbError> {
            schema.fsms[fsm_idx]
                .states
                .iter()
                .position(|s| s == state_name)
                .map(|idx| idx as u16)
                .ok_or_else(|| {
                    DbError::InvalidSchema(format!(
                        "unknown state '{}' in transition for fsm {}",
                        state_name, fsm_idx
                    ))
                })
        };

        for TransitionDef { from, to, guard } in transitions {
            let from_id = find_state(&from)?;
            let to_id = find_state(&to)?;
            if allowed.contains_key(&(from_id, to_id)) {
                return Err(DbError::InvalidSchema(format!(
                    "duplicate transition ('{from}' -> '{to}') in fsm {fsm_idx}"
                )));
            }
            let bound_guard = match guard {
                Some(g) => bind_expr(g, Some(&schema))?,
                None => Expr::Literal(Value::Bool(true)),
            };
            allowed.insert((from_id, to_id), bound_guard);
        }

        schema.fsms[fsm_idx].allowed = allowed;
    }

    schema.validate()?;
    Ok(BoundStatement::CreateTable(schema))
}

fn bind_select<C: Catalog>(
    items: Vec<SelectItem>,
    from: Option<String>,
    filter: Option<AstExpr>,
    limit: Option<usize>,
    catalog: &C,
) -> Result<BoundStatement, DbError> {
    let (mut plan, cols) = match from {
        None => {
            let mut plan = Executor::SingleRow { done: false };
            if let Some(f) = filter {
                plan = Executor::Filter {
                    input: Box::new(plan),
                    predicate: bind_expr(f, None)?,
                };
            }
            let cols = items
                .into_iter()
                .map(|item| match item {
                    SelectItem::Wildcard => Err(DbError::BindError(
                        "wildcard '*' requires a FROM clause".to_string(),
                    )),
                    SelectItem::Expr(e) => bind_expr(e, None),
                })
                .collect::<Result<Vec<_>, _>>()?;
            (plan, cols)
        }
        Some(table) => {
            let schema = catalog
                .get_table(&table)?
                .ok_or_else(|| DbError::TableNotFound(table.clone()))?;
            let mut plan = Executor::SeqScan { table, rows: None };
            if let Some(f) = filter {
                plan = Executor::Filter {
                    input: Box::new(plan),
                    predicate: bind_expr(f, Some(schema))?,
                };
            }
            let mut cols = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    SelectItem::Wildcard => {
                        cols.extend((0..schema.columns.len()).map(Expr::Column));
                    }
                    SelectItem::Expr(e) => cols.push(bind_expr(e, Some(schema))?),
                }
            }
            (plan, cols)
        }
    };

    plan = Executor::Project {
        input: Box::new(plan),
        cols,
    };

    if let Some(remaining) = limit {
        plan = Executor::Limit {
            input: Box::new(plan),
            remaining,
        };
    }

    Ok(BoundStatement::Execute(plan))
}

fn bind_insert<C: Catalog>(
    table: String,
    values: Vec<AstExpr>,
    catalog: &C,
) -> Result<BoundStatement, DbError> {
    let schema = catalog
        .get_table(&table)?
        .ok_or_else(|| DbError::TableNotFound(table.clone()))?;

    if values.len() != schema.columns.len() {
        return Err(DbError::BindError(format!(
            "INSERT into '{}' expected {} values, got {}",
            table,
            schema.columns.len(),
            values.len()
        )));
    }

    let clock = MockClock { now: 0 };
    let empty_row = Vec::new();
    let mut row = Vec::with_capacity(values.len());
    for (col, ast_expr) in schema.columns.iter().zip(values) {
        let bound = bind_expr(ast_expr, None)?;
        if contains_current_time(&bound) {
            return Err(DbError::BindError(
                "NOW is not supported in INSERT VALUES".to_string(),
            ));
        }
        let mut val = eval_expr(&bound, &empty_row, &clock)?;
        if let ColumnType::Fsm(fsm_idx) = col.column_type {
            val = match val {
                Value::String(s) => Value::Fsm(resolve_fsm_state(schema, fsm_idx, &s)?),
                Value::Fsm(id)
                    if schema
                        .fsms
                        .get(fsm_idx as usize)
                        .is_some_and(|fsm| (id as usize) < fsm.states.len()) =>
                {
                    Value::Fsm(id)
                }
                other => {
                    return Err(DbError::BindError(format!(
                        "expected string FSM state for column '{}', got {other:?}",
                        col.name
                    )));
                }
            };
        }
        row.push(val);
    }

    Ok(BoundStatement::Execute(Executor::Insert {
        table,
        rows: vec![row].into_iter(),
    }))
}

fn bind_update<C: Catalog>(
    table: String,
    sets: Vec<(String, AstExpr)>,
    filter: Option<AstExpr>,
    catalog: &C,
) -> Result<BoundStatement, DbError> {
    let schema = catalog
        .get_table(&table)?
        .ok_or_else(|| DbError::TableNotFound(table.clone()))?;

    let mut input = Executor::SeqScan {
        table: table.clone(),
        rows: None,
    };
    if let Some(f) = filter {
        input = Executor::Filter {
            input: Box::new(input),
            predicate: bind_expr(f, Some(schema))?,
        };
    }

    let mut bound_sets = Vec::with_capacity(sets.len());
    for (col_name, rhs_ast) in sets {
        let (col_idx, col) = find_column(schema, &col_name)?;
        let rhs_expr = match (&col.column_type, rhs_ast) {
            (ColumnType::Fsm(fsm_idx), AstExpr::Literal(Value::String(state))) => {
                let state_id = resolve_fsm_state(schema, *fsm_idx, &state)?;
                Expr::Literal(Value::Fsm(state_id))
            }
            (_, ast) => bind_expr(ast, Some(schema))?,
        };
        bound_sets.push((col_idx, rhs_expr));
    }

    Ok(BoundStatement::Execute(Executor::Update {
        table,
        input: Box::new(input),
        sets: bound_sets,
        results: None,
    }))
}

fn bind_delete<C: Catalog>(
    table: String,
    filter: Option<AstExpr>,
    catalog: &C,
) -> Result<BoundStatement, DbError> {
    let schema = catalog
        .get_table(&table)?
        .ok_or_else(|| DbError::TableNotFound(table.clone()))?;

    let mut input = Executor::SeqScan {
        table: table.clone(),
        rows: None,
    };
    if let Some(f) = filter {
        input = Executor::Filter {
            input: Box::new(input),
            predicate: bind_expr(f, Some(schema))?,
        };
    }

    Ok(BoundStatement::Execute(Executor::Delete {
        table,
        input: Box::new(input),
        results: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{catalog::InMemoryCatalog, lexer::tokenize, parser::parse, types::DbError};

    fn parse_one(sql: &str) -> Statement {
        let tokens = tokenize(sql).unwrap();
        let mut stmts = parse(&tokens).unwrap();
        assert_eq!(stmts.len(), 1);
        stmts.remove(0)
    }

    fn bind_sql<C: Catalog>(sql: &str, catalog: &C) -> Result<BoundStatement, DbError> {
        bind(parse_one(sql), catalog)
    }

    fn setup_tasks_catalog() -> InMemoryCatalog {
        let mut catalog = InMemoryCatalog::new();
        let sql = "CREATE TABLE tasks (
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
        let BoundStatement::CreateTable(schema) = bind_sql(sql, &catalog).unwrap() else {
            panic!("expected CreateTable");
        };
        catalog.create_table(schema).unwrap();
        catalog
    }

    #[test]
    fn test_create_table_with_fsm_and_guard() {
        let catalog = InMemoryCatalog::new();
        let sql = "CREATE TABLE tasks (
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
        let BoundStatement::CreateTable(schema) = bind_sql(sql, &catalog).unwrap() else {
            panic!("expected CreateTable");
        };

        assert_eq!(schema.name, "tasks");
        assert_eq!(schema.columns.len(), 4);
        assert_eq!(schema.columns[0].name, "retries");
        assert_eq!(schema.columns[0].column_type, ColumnType::Integer);
        assert_eq!(schema.columns[1].name, "done");
        assert_eq!(schema.columns[1].column_type, ColumnType::Bool);
        assert_eq!(schema.columns[2].name, "title");
        assert_eq!(schema.columns[2].column_type, ColumnType::String);
        assert_eq!(schema.columns[3].name, "state");
        assert_eq!(schema.columns[3].column_type, ColumnType::Fsm(0));

        assert_eq!(schema.fsms.len(), 1);
        let fsm = &schema.fsms[0];
        assert_eq!(fsm.states, vec!["pending", "running", "failed", "done"]);
        assert_eq!(fsm.initial, 0);
        assert_eq!(fsm.allowed.len(), 3);

        assert!(matches!(
            fsm.allowed.get(&(0, 1)),
            Some(Expr::Literal(Value::Bool(true)))
        ));
        assert!(matches!(
            fsm.allowed.get(&(1, 3)),
            Some(Expr::Literal(Value::Bool(true)))
        ));
        match fsm.allowed.get(&(1, 2)) {
            Some(Expr::Binary {
                op: BinaryOp::Ge,
                left,
                right,
            }) => {
                assert!(matches!(**left, Expr::Column(0)));
                assert!(matches!(**right, Expr::Literal(Value::Integer(3))));
            }
            other => panic!("unexpected guard for running -> failed: {other:?}"),
        }
    }

    #[test]
    fn test_create_table_guard_referencing_fsm_state_and_later_column() {
        let catalog = InMemoryCatalog::new();
        let sql = "CREATE TABLE jobs (
            state FSM (
                STATES ('idle', 'active'),
                INITIAL 'idle',
                TRANSITIONS (
                    'idle' -> 'active' WHEN state = 'idle' AND priority > 0
                )
            ),
            priority INTEGER
        );";
        let BoundStatement::CreateTable(schema) = bind_sql(sql, &catalog).unwrap() else {
            panic!("expected CreateTable");
        };
        assert_eq!(schema.columns[0].column_type, ColumnType::Fsm(0));
        match schema.fsms[0].allowed.get(&(0, 1)) {
            Some(Expr::Binary {
                op: BinaryOp::And,
                left,
                right,
            }) => {
                match left.as_ref() {
                    Expr::Binary {
                        op: BinaryOp::Eq,
                        left: l,
                        right: r,
                    } => {
                        assert!(matches!(**l, Expr::Column(0)));
                        assert!(matches!(**r, Expr::Literal(Value::Fsm(0))));
                    }
                    other => panic!("unexpected left guard: {other:?}"),
                }
                match right.as_ref() {
                    Expr::Binary {
                        op: BinaryOp::Gt,
                        left: l,
                        right: r,
                    } => {
                        assert!(matches!(**l, Expr::Column(1)));
                        assert!(matches!(**r, Expr::Literal(Value::Integer(0))));
                    }
                    other => panic!("unexpected right guard: {other:?}"),
                }
            }
            other => panic!("unexpected guard: {other:?}"),
        }
    }

    #[test]
    fn test_create_table_errors() {
        let catalog = InMemoryCatalog::new();

        let dup_col = "CREATE TABLE t (a INTEGER, a STRING);";
        assert!(matches!(
            bind_sql(dup_col, &catalog),
            Err(DbError::InvalidSchema(_))
        ));

        let dup_state = "CREATE TABLE t (
            s FSM (STATES ('a', 'a'), INITIAL 'a', TRANSITIONS ())
        );";
        assert!(matches!(
            bind_sql(dup_state, &catalog),
            Err(DbError::InvalidSchema(_))
        ));

        let bad_initial = "CREATE TABLE t (
            s FSM (STATES ('a', 'b'), INITIAL 'c', TRANSITIONS ())
        );";
        assert!(matches!(
            bind_sql(bad_initial, &catalog),
            Err(DbError::InvalidSchema(_))
        ));

        let bad_from = "CREATE TABLE t (
            s FSM (STATES ('a', 'b'), INITIAL 'a', TRANSITIONS ('c' -> 'b'))
        );";
        assert!(matches!(
            bind_sql(bad_from, &catalog),
            Err(DbError::InvalidSchema(_))
        ));

        let bad_to = "CREATE TABLE t (
            s FSM (STATES ('a', 'b'), INITIAL 'a', TRANSITIONS ('a' -> 'c'))
        );";
        assert!(matches!(
            bind_sql(bad_to, &catalog),
            Err(DbError::InvalidSchema(_))
        ));

        let dup_trans = "CREATE TABLE t (
            s FSM (STATES ('a', 'b'), INITIAL 'a', TRANSITIONS ('a' -> 'b', 'a' -> 'b'))
        );";
        assert!(matches!(
            bind_sql(dup_trans, &catalog),
            Err(DbError::InvalidSchema(_))
        ));

        let bad_guard_col = "CREATE TABLE t (
            s FSM (STATES ('a', 'b'), INITIAL 'a', TRANSITIONS ('a' -> 'b' WHEN missing > 1))
        );";
        assert!(matches!(
            bind_sql(bad_guard_col, &catalog),
            Err(DbError::BindError(_))
        ));
    }

    #[test]
    fn test_select_where_fsm_both_orders_wildcard_limit_and_no_from() {
        let catalog = setup_tasks_catalog();

        // state = 'running' and 'running' = state, with wildcard and limit
        for sql in [
            "SELECT * FROM tasks WHERE state = 'running' LIMIT 5;",
            "SELECT * FROM tasks WHERE 'running' = state LIMIT 5;",
        ] {
            let BoundStatement::Execute(Executor::Limit { input, remaining }) =
                bind_sql(sql, &catalog).unwrap()
            else {
                panic!("expected Limit");
            };
            assert_eq!(remaining, 5);
            let Executor::Project { input, cols } = *input else {
                panic!("expected Project");
            };
            assert_eq!(cols.len(), 4);
            for (idx, col) in cols.iter().enumerate() {
                assert!(matches!(col, Expr::Column(c) if *c == idx));
            }
            let Executor::Filter { input, predicate } = *input else {
                panic!("expected Filter");
            };
            assert!(matches!(
                *input,
                Executor::SeqScan { ref table, .. } if table == "tasks"
            ));
            match predicate {
                Expr::Binary {
                    op: BinaryOp::Eq,
                    left,
                    right,
                } => {
                    let is_col_fsm = matches!(
                        (*left.clone(), *right.clone()),
                        (Expr::Column(3), Expr::Literal(Value::Fsm(1)))
                            | (Expr::Literal(Value::Fsm(1)), Expr::Column(3))
                    );
                    assert!(is_col_fsm, "unexpected predicate: {left:?} = {right:?}");
                }
                other => panic!("unexpected predicate: {other:?}"),
            }
        }

        let BoundStatement::Execute(Executor::Project { input, .. }) =
            bind_sql("SELECT retries FROM tasks WHERE 'done' != state;", &catalog).unwrap()
        else {
            panic!("expected Project");
        };
        let Executor::Filter { predicate, .. } = *input else {
            panic!("expected Filter");
        };
        match predicate {
            Expr::Binary {
                op: BinaryOp::Ne,
                left,
                right,
            } => {
                assert!(matches!(*left, Expr::Literal(Value::Fsm(3))));
                assert!(matches!(*right, Expr::Column(3)));
            }
            other => panic!("unexpected predicate: {other:?}"),
        }

        let BoundStatement::Execute(Executor::Limit { input, remaining }) =
            bind_sql("SELECT -5, -1.5, NOW, 2 * 8 LIMIT 1;", &catalog).unwrap()
        else {
            panic!("expected Limit");
        };
        assert_eq!(remaining, 1);
        let Executor::Project { input, cols } = *input else {
            panic!("expected Project");
        };
        assert!(matches!(*input, Executor::SingleRow { done: false }));
        assert_eq!(cols.len(), 4);
        assert!(matches!(cols[0], Expr::Literal(Value::Integer(-5))));
        assert!(matches!(cols[1], Expr::Literal(Value::Float(f)) if (f + 1.5).abs() < 1e-9));
        assert!(matches!(cols[2], Expr::CurrentTime));
        assert!(matches!(
            cols[3],
            Expr::Binary {
                op: BinaryOp::Mul,
                ..
            }
        ));

        let BoundStatement::Execute(Executor::Project { cols, .. }) =
            bind_sql("SELECT -retries FROM tasks;", &catalog).unwrap()
        else {
            panic!("expected Project");
        };
        match &cols[0] {
            Expr::Binary {
                op: BinaryOp::Sub,
                left,
                right,
            } => {
                assert!(matches!(**left, Expr::Literal(Value::Integer(0))));
                assert!(matches!(**right, Expr::Column(0)));
            }
            other => panic!("expected 0 - col, got {other:?}"),
        }
    }

    #[test]
    fn test_insert_update_and_delete() {
        let catalog = setup_tasks_catalog();

        let BoundStatement::Execute(Executor::Insert { table, mut rows }) = bind_sql(
            "INSERT INTO tasks VALUES (-2 + 5, false, 'init', 'pending');",
            &catalog,
        )
        .unwrap() else {
            panic!("expected Insert");
        };
        assert_eq!(table, "tasks");
        assert_eq!(
            rows.next().unwrap(),
            vec![
                Value::Integer(3),
                Value::Bool(false),
                Value::String("init".to_string()),
                Value::Fsm(0),
            ]
        );
        assert!(rows.next().is_none());

        let BoundStatement::Execute(Executor::Update {
            table, input, sets, ..
        }) = bind_sql(
            "UPDATE tasks SET retries = retries + 1, state = 'failed' WHERE state = 'running';",
            &catalog,
        )
        .unwrap()
        else {
            panic!("expected Update");
        };
        assert_eq!(table, "tasks");
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].0, 0);
        match &sets[0].1 {
            Expr::Binary {
                op: BinaryOp::Add,
                left,
                right,
            } => {
                assert!(matches!(**left, Expr::Column(0)));
                assert!(matches!(**right, Expr::Literal(Value::Integer(1))));
            }
            other => panic!("unexpected SET rhs: {other:?}"),
        }
        assert_eq!(sets[1].0, 3);
        assert!(matches!(sets[1].1, Expr::Literal(Value::Fsm(2))));

        let Executor::Filter { input, predicate } = *input else {
            panic!("expected Filter");
        };
        assert!(matches!(*input, Executor::SeqScan { .. }));
        match predicate {
            Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } => {
                assert!(matches!(*left, Expr::Column(3)));
                assert!(matches!(*right, Expr::Literal(Value::Fsm(1))));
            }
            other => panic!("unexpected WHERE predicate: {other:?}"),
        }

        let BoundStatement::Execute(Executor::Delete { table, input, .. }) =
            bind_sql("DELETE FROM tasks WHERE state = 'done';", &catalog).unwrap()
        else {
            panic!("expected Delete");
        };
        assert_eq!(table, "tasks");
        let Executor::Filter { predicate, .. } = *input else {
            panic!("expected Filter");
        };
        match predicate {
            Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } => {
                assert!(matches!(*left, Expr::Column(3)));
                assert!(matches!(*right, Expr::Literal(Value::Fsm(3))));
            }
            other => panic!("unexpected WHERE predicate: {other:?}"),
        }
    }

    #[test]
    fn test_negative_cases() {
        let catalog = setup_tasks_catalog();

        for sql in [
            "SELECT * FROM missing;",
            "INSERT INTO missing VALUES (1);",
            "UPDATE missing SET a = 1;",
            "DELETE FROM missing;",
        ] {
            assert!(
                matches!(bind_sql(sql, &catalog), Err(DbError::TableNotFound(_))),
                "expected TableNotFound for {sql}"
            );
        }

        for sql in [
            "SELECT nope FROM tasks;",
            "SELECT retries FROM tasks WHERE nope = 1;",
            "UPDATE tasks SET nope = 1;",
            "UPDATE tasks SET retries = nope + 1;",
            "DELETE FROM tasks WHERE nope = 1;",
        ] {
            assert!(
                matches!(bind_sql(sql, &catalog), Err(DbError::BindError(_))),
                "expected BindError for {sql}"
            );
        }

        for sql in [
            "INSERT INTO tasks VALUES (0, false, 't', 'unknown');",
            "UPDATE tasks SET state = 'unknown';",
            "SELECT * FROM tasks WHERE state = 'unknown';",
            "SELECT * FROM tasks WHERE 'unknown' = state;",
            "DELETE FROM tasks WHERE state != 'unknown';",
        ] {
            assert!(
                matches!(bind_sql(sql, &catalog), Err(DbError::BindError(_))),
                "expected BindError for {sql}"
            );
        }

        assert!(matches!(
            bind_sql("INSERT INTO tasks VALUES (0, false);", &catalog),
            Err(DbError::BindError(_))
        ));

        assert!(matches!(
            bind_sql("SELECT *;", &catalog),
            Err(DbError::BindError(_))
        ));
        assert!(matches!(
            bind_sql("SELECT retries;", &catalog),
            Err(DbError::BindError(_))
        ));
    }

    #[test]
    fn test_bind_example_sql() {
        let sql = include_str!("../docs/example.sql");
        let tokens = tokenize(sql).unwrap();
        let stmts = parse(&tokens).unwrap();
        assert_eq!(stmts.len(), 10);

        let mut catalog = InMemoryCatalog::new();
        let mut bound_kinds = Vec::new();

        for stmt in stmts {
            let bound = bind(stmt, &catalog).unwrap();
            match bound {
                BoundStatement::CreateTable(schema) => {
                    assert_eq!(schema.name, "tasks");
                    catalog.create_table(schema).unwrap();
                    bound_kinds.push("create");
                }
                BoundStatement::Begin => bound_kinds.push("begin"),
                BoundStatement::Commit => bound_kinds.push("commit"),
                BoundStatement::Rollback => bound_kinds.push("rollback"),
                BoundStatement::Execute(exec) => match exec {
                    Executor::Insert { .. } => bound_kinds.push("insert"),
                    Executor::Limit { .. } => bound_kinds.push("limit"),
                    Executor::Project { .. } => bound_kinds.push("project"),
                    Executor::Update { .. } => bound_kinds.push("update"),
                    Executor::Delete { .. } => bound_kinds.push("delete"),
                    _ => panic!("unexpected top-level executor"),
                },
            }
        }

        assert_eq!(
            bound_kinds,
            vec![
                "create", "begin", "commit", "rollback", "insert", "limit", "limit", "project",
                "update", "delete"
            ]
        );
    }
}
