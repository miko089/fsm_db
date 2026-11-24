use std::io::{self, BufRead, IsTerminal, Write};

use fsm_db::{Catalog, Database, FsmRef, QueryResult, Value};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    run_repl(stdin.lock(), &mut stdout, &mut stderr, interactive)
}

fn run_repl<R: BufRead, W: Write, E: Write>(
    mut input: R,
    out: &mut W,
    err: &mut E,
    interactive: bool,
) -> io::Result<()> {
    let mut db = Database::in_memory();
    let mut buffer = String::new();
    let mut line = String::new();

    loop {
        if interactive {
            let prompt = if buffer.is_empty() { "db> " } else { "..> " };
            out.write_all(prompt.as_bytes())?;
            out.flush()?;
        }

        line.clear();
        let bytes_read = input.read_line(&mut line)?;
        if bytes_read == 0 {
            if interactive {
                writeln!(out)?;
            }
            let remaining = buffer.trim();
            if !remaining.is_empty() {
                match db.execute(remaining) {
                    Ok(results) => print_results(&results, db.catalog(), out)?,
                    Err(e) => writeln!(err, "Error: {e}")?,
                }
            }
            break;
        }

        let trimmed = line.trim();
        if !is_in_string(&buffer) && matches!(trimmed, ".exit" | "\\q") {
            break;
        }

        if buffer.is_empty() && trimmed.is_empty() {
            continue;
        }

        buffer.push_str(&line);

        if let Some(sql) = take_complete_sql(&mut buffer) {
            match db.execute(&sql) {
                Ok(results) => print_results(&results, db.catalog(), out)?,
                Err(e) => writeln!(err, "Error: {e}")?,
            }
        }
    }

    Ok(())
}

fn is_in_string(sql: &str) -> bool {
    let mut in_string = false;
    for ch in sql.chars() {
        if ch == '\'' {
            in_string = !in_string;
        }
    }
    in_string
}

fn take_complete_sql(buffer: &mut String) -> Option<String> {
    let mut in_string = false;
    let mut last_semi_end = None;

    for (idx, ch) in buffer.char_indices() {
        match ch {
            '\'' => in_string = !in_string,
            ';' if !in_string => last_semi_end = Some(idx + ch.len_utf8()),
            _ => {}
        }
    }

    let end = last_semi_end?;
    let sql = buffer[..end].to_string();
    *buffer = buffer[end..].trim_start().to_string();
    Some(sql)
}

fn print_results<C: Catalog, W: Write>(
    results: &[QueryResult],
    catalog: &C,
    out: &mut W,
) -> io::Result<()> {
    for result in results {
        match result {
            QueryResult::Ok => writeln!(out, "OK")?,
            QueryResult::Rows { rows, fsm_columns } => {
                for row in rows {
                    let formatted = row
                        .iter()
                        .enumerate()
                        .map(|(col_idx, value)| {
                            let fsm_ref = fsm_columns.get(col_idx).and_then(Option::as_ref);
                            format_value(value, fsm_ref, catalog)
                        })
                        .collect::<Vec<_>>()
                        .join(" | ");
                    writeln!(out, "{formatted}")?;
                }
            }
        }
    }
    Ok(())
}

fn format_value<C: Catalog>(value: &Value, fsm_ref: Option<&FsmRef>, catalog: &C) -> String {
    match value {
        Value::Integer(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Fsm(id) => fsm_ref
            .and_then(|r| {
                catalog
                    .get_table(&r.table)
                    .ok()
                    .flatten()
                    .and_then(|schema| schema.fsm_state_name(r.fsm_idx, *id))
            })
            .map(str::to_string)
            .unwrap_or_else(|| format!("fsm({id})")),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Null => "NULL".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run_script(script: &str, interactive: bool) -> (String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        run_repl(
            Cursor::new(script.as_bytes()),
            &mut out,
            &mut err,
            interactive,
        )
        .unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn multiline_fsm_and_strings_with_semicolons() {
        let script = "\
CREATE TABLE jobs (
    id INTEGER,
    note STRING,
    state FSM (
        STATES ('new', 'done'),
        INITIAL 'new',
        TRANSITIONS ('new' -> 'done')
    )
);
INSERT INTO jobs VALUES (1, 'a;b''c', 'new');
SELECT id, note, state FROM jobs;
";
        let (out, err) = run_script(script, false);
        assert_eq!(err, "");
        assert_eq!(out, "OK\n1 | a;b'c | new\n1 | a;b'c | new\n");
    }

    #[test]
    fn exit_commands_stop_immediately() {
        let (out1, err1) = run_script("SELECT 1;\n.exit\nSELECT 2;\n", false);
        assert_eq!(err1, "");
        assert_eq!(out1, "1\n");

        let (out2, err2) = run_script("SELECT 1;\n\\q\nSELECT 2;\n", false);
        assert_eq!(err2, "");
        assert_eq!(out2, "1\n");
    }

    #[test]
    fn errors_go_to_stderr_and_repl_continues() {
        let script = "SELECT * FROM missing_table;\nSELECT 42, true, NULL;\n";
        let (out, err) = run_script(script, false);
        assert!(err.starts_with("Error: "));
        assert!(err.contains("missing_table"));
        assert_eq!(out, "42 | true | NULL\n");
    }

    #[test]
    fn interactive_prompts_and_piped_mode() {
        let script = "\nSELECT\n1;\n";
        let (interactive_out, interactive_err) = run_script(script, true);
        assert_eq!(interactive_err, "");
        assert_eq!(interactive_out, "db> db> ..> 1\ndb> \n");

        let (piped_out, piped_err) = run_script(script, false);
        assert_eq!(piped_err, "");
        assert_eq!(piped_out, "1\n");
    }

    #[test]
    fn example_sql_pipe_succeeds() {
        let example = include_str!("../docs/example.sql");
        let (out, err) = run_script(example, false);
        assert_eq!(err, "");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 8);
        assert_eq!(&lines[0..5], &["OK", "OK", "OK", "OK", "OK"]);
        assert_eq!(lines[5], "0 | false | init | pending");
        assert_eq!(lines[6], "0 | false | init | pending");
        assert!(lines[7].starts_with("current time | "));
        assert!(lines[7].ends_with(" | 16"));
    }

    #[test]
    fn multiple_fsm_columns_reordered_repeated_and_dml_print_state_names() {
        let script = "\
CREATE TABLE orders (
    id INTEGER,
    payment FSM (
        STATES ('unpaid', 'paid', 'refunded'),
        INITIAL 'unpaid',
        TRANSITIONS ('unpaid' -> 'paid', 'paid' -> 'refunded')
    ),
    shipping FSM (
        STATES ('pending', 'shipped', 'delivered'),
        INITIAL 'pending',
        TRANSITIONS ('pending' -> 'shipped', 'shipped' -> 'delivered')
    )
);
INSERT INTO orders VALUES (10, 'unpaid', 'pending');
UPDATE orders SET payment = 'paid', shipping = 'shipped' WHERE id = 10;
SELECT shipping, id, payment, payment = 'paid' FROM orders;
SELECT payment, payment FROM orders;
DELETE FROM orders WHERE shipping = 'shipped';
";
        let (out, err) = run_script(script, false);
        assert_eq!(err, "");
        assert_eq!(
            out,
            "\
OK
10 | unpaid | pending
10 | paid | shipped
shipped | 10 | paid | true
paid | paid
10 | paid | shipped
"
        );
    }

    #[test]
    fn format_value_falls_back_to_fsm_id_when_catalog_lookup_fails() {
        let db = Database::in_memory();
        let catalog = db.catalog();

        assert_eq!(format_value(&Value::Fsm(3), None, catalog), "fsm(3)");

        let missing_table = FsmRef {
            table: "missing".to_string(),
            fsm_idx: 0,
        };
        assert_eq!(
            format_value(&Value::Fsm(2), Some(&missing_table), catalog),
            "fsm(2)"
        );
    }
}
