use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::{DbError, Row, Value};

#[derive(Debug, Clone)]
pub(crate) enum BinaryOp {
    Eq,  // =
    Ne,  // !=
    Lt,  // <
    Le,  // <=
    Gt,  // >
    Ge,  // >=
    And, // AND
    Or,  // OR
    Add, // +
    Sub, // -
    Mul, // *
    Div, // /
}

#[derive(Debug, Clone)]
pub(crate) enum Expr {
    Literal(Value),
    Column(usize),
    CurrentTime,
    // nothing equals `Null` => comparison of anything with `Null` is `false`, not `Null`
    IsNull(Box<Expr>),
    Not(Box<Expr>),
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
}

pub(crate) trait Clock {
    fn now(&self) -> i64;
}

pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

pub(crate) struct MockClock {
    pub(crate) now: i64,
}

impl Clock for MockClock {
    fn now(&self) -> i64 {
        self.now
    }
}

fn eval_ord<T: PartialOrd>(op: &BinaryOp, a: &T, b: &T) -> Option<bool> {
    match op {
        BinaryOp::Eq => Some(a == b),
        BinaryOp::Ne => Some(a != b),
        BinaryOp::Lt => Some(a < b),
        BinaryOp::Le => Some(a <= b),
        BinaryOp::Gt => Some(a > b),
        BinaryOp::Ge => Some(a >= b),
        _ => None,
    }
}

fn eval_arith(op: &BinaryOp, lhs: &Value, rhs: &Value) -> Option<Value> {
    match (lhs, rhs) {
        (Value::Null, _) | (_, Value::Null) => Some(Value::Null),
        (Value::Integer(a), Value::Integer(b)) => match op {
            BinaryOp::Add => a.checked_add(*b).map(Value::Integer),
            BinaryOp::Sub => a.checked_sub(*b).map(Value::Integer),
            BinaryOp::Mul => a.checked_mul(*b).map(Value::Integer),
            BinaryOp::Div => a.checked_div(*b).map(Value::Integer),
            _ => None,
        },
        (Value::Float(a), Value::Float(b)) => match op {
            BinaryOp::Add => Some(Value::Float(a + b)),
            BinaryOp::Sub => Some(Value::Float(a - b)),
            BinaryOp::Mul => Some(Value::Float(a * b)),
            BinaryOp::Div => (*b != 0.0).then(|| Value::Float(a / b)),
            _ => None,
        },
        _ => None,
    }
}

fn eval_eq<T: PartialEq>(op: &BinaryOp, a: &T, b: &T) -> Option<bool> {
    match op {
        BinaryOp::Eq => Some(a == b),
        BinaryOp::Ne => Some(a != b),
        _ => None,
    }
}

pub(crate) fn eval_expr(expr: &Expr, row: &Row, clock: &dyn Clock) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(val) => Ok(val.clone()),
        Expr::Column(idx) => row
            .get(*idx)
            .cloned()
            .ok_or_else(|| DbError::EvalError(expr.clone())),
        Expr::CurrentTime => Ok(Value::Integer(clock.now())),
        Expr::IsNull(inner) => {
            let val = eval_expr(inner, row, clock)?;
            Ok(Value::Bool(matches!(val, Value::Null)))
        }
        Expr::Not(inner) => match eval_expr(inner, row, clock)? {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            _ => Err(DbError::EvalError(expr.clone())),
        },
        Expr::Binary { op, left, right } => match op {
            BinaryOp::And => match eval_expr(left, row, clock)? {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => match eval_expr(right, row, clock)? {
                    Value::Bool(b) => Ok(Value::Bool(b)),
                    _ => Err(DbError::EvalError(expr.clone())),
                },
                _ => Err(DbError::EvalError(expr.clone())),
            },
            BinaryOp::Or => match eval_expr(left, row, clock)? {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => match eval_expr(right, row, clock)? {
                    Value::Bool(b) => Ok(Value::Bool(b)),
                    _ => Err(DbError::EvalError(expr.clone())),
                },
                _ => Err(DbError::EvalError(expr.clone())),
            },
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
                let lhs = eval_expr(left, row, clock)?;
                let rhs = eval_expr(right, row, clock)?;
                eval_arith(op, &lhs, &rhs).ok_or_else(|| DbError::EvalError(expr.clone()))
            }
            _ => {
                let lhs = eval_expr(left, row, clock)?;
                let rhs = eval_expr(right, row, clock)?;
                let res = match (&lhs, &rhs) {
                    (Value::Null, _) | (_, Value::Null) => Some(false),
                    (Value::Integer(a), Value::Integer(b)) => eval_ord(op, a, b),
                    (Value::Float(a), Value::Float(b)) => eval_ord(op, a, b),
                    (Value::String(a), Value::String(b)) => eval_ord(op, a, b),
                    (Value::Bool(a), Value::Bool(b)) => eval_eq(op, a, b),
                    (Value::Fsm(a), Value::Fsm(b)) => eval_eq(op, a, b),
                    _ => None,
                };
                res.map(Value::Bool)
                    .ok_or_else(|| DbError::EvalError(expr.clone()))
            }
        },
    }
}

pub(crate) fn eval_predicate(expr: &Expr, row: &Row, clock: &dyn Clock) -> Result<bool, DbError> {
    match eval_expr(expr, row, clock)? {
        Value::Bool(b) => Ok(b),
        _ => Err(DbError::EvalError(expr.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_and_ge_and_is_not_null() {
        // col(0) >= 5 and col(1) is not null
        let expr = Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(Expr::Binary {
                op: BinaryOp::Ge,
                left: Box::new(Expr::Column(0)),
                right: Box::new(Expr::Literal(Value::Integer(5))),
            }),
            right: Box::new(Expr::Not(Box::new(Expr::IsNull(Box::new(Expr::Column(1)))))),
        };
        let clock = MockClock { now: 0 };

        // 5 >= 5 and "err" is not null -> true
        let row_ok = vec![Value::Integer(5), Value::String("err".to_string())];
        assert!(eval_predicate(&expr, &row_ok, &clock).unwrap());

        // 5 >= 5 and null is not null -> false
        let row_null = vec![Value::Integer(5), Value::Null];
        assert!(!eval_predicate(&expr, &row_null, &clock).unwrap());

        // 4 >= 5 and "err" is not null -> false
        let row_lt = vec![Value::Integer(4), Value::String("err".to_string())];
        assert!(!eval_predicate(&expr, &row_lt, &clock).unwrap());
    }

    #[test]
    fn current_time_with_mock_clock() {
        // col(0) < time.now
        let expr = Expr::Binary {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Column(0)),
            right: Box::new(Expr::CurrentTime),
        };
        let clock = MockClock { now: 1000 };

        // 900 < 1000 -> true
        let expired = vec![Value::Integer(900)];
        assert!(eval_predicate(&expr, &expired, &clock).unwrap());

        // 1100 < 1000 -> false
        let active = vec![Value::Integer(1100)];
        assert!(!eval_predicate(&expr, &active, &clock).unwrap());
    }

    #[test]
    fn basic_logic_and_null_comparison() {
        let clock = MockClock { now: 0 };
        let empty_row: Row = vec![];

        let default_guard = Expr::Literal(Value::Bool(true));
        assert!(eval_predicate(&default_guard, &empty_row, &clock).unwrap());

        let or_expr = Expr::Binary {
            op: BinaryOp::Or,
            left: Box::new(Expr::Literal(Value::Bool(false))),
            right: Box::new(Expr::Literal(Value::Bool(true))),
        };
        assert!(eval_predicate(&or_expr, &empty_row, &clock).unwrap());

        let null_eq = Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(Value::Null)),
            right: Box::new(Expr::Literal(Value::Integer(1))),
        };
        assert!(!eval_predicate(&null_eq, &empty_row, &clock).unwrap());
    }

    #[test]
    fn eval_errors() {
        let clock = MockClock { now: 0 };
        let row = vec![Value::Integer(1)];

        let out_of_bounds = Expr::Column(99);
        assert!(matches!(
            eval_predicate(&out_of_bounds, &row, &clock),
            Err(DbError::EvalError(_))
        ));

        let type_mismatch = Expr::Binary {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(Value::Integer(5))),
            right: Box::new(Expr::Literal(Value::String("abc".to_string()))),
        };
        assert!(matches!(
            eval_predicate(&type_mismatch, &row, &clock),
            Err(DbError::EvalError(_))
        ));

        let non_bool = Expr::Literal(Value::Integer(42));
        assert!(matches!(
            eval_predicate(&non_bool, &row, &clock),
            Err(DbError::EvalError(_))
        ));
    }

    #[test]
    fn basic_arithmetic() {
        let clock = MockClock { now: 0 };
        let row = vec![Value::Integer(10), Value::Integer(3), Value::Float(2.5)];

        let cases = [
            (
                BinaryOp::Add,
                Expr::Column(0),
                Expr::Column(1),
                Value::Integer(13),
            ),
            (
                BinaryOp::Sub,
                Expr::Column(0),
                Expr::Column(1),
                Value::Integer(7),
            ),
            (
                BinaryOp::Mul,
                Expr::Column(0),
                Expr::Column(1),
                Value::Integer(30),
            ),
            (
                BinaryOp::Div,
                Expr::Column(0),
                Expr::Column(1),
                Value::Integer(3),
            ),
            (
                BinaryOp::Mul,
                Expr::Column(2),
                Expr::Literal(Value::Float(4.0)),
                Value::Float(10.0),
            ),
        ];

        for (op, left, right, expected) in cases {
            let expr = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
            assert_eq!(eval_expr(&expr, &row, &clock).unwrap(), expected);
        }

        let div_by_zero = Expr::Binary {
            op: BinaryOp::Div,
            left: Box::new(Expr::Column(0)),
            right: Box::new(Expr::Literal(Value::Integer(0))),
        };
        assert!(matches!(
            eval_expr(&div_by_zero, &row, &clock),
            Err(DbError::EvalError(_))
        ));
    }
}
