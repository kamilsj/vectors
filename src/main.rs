use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use vectors::{Database, ExecutionResult, Value};

const HELP: &str = "\
Shell commands:
  .help                 Show this help
  .tables               List tables
  .schema TABLE         Show a table's columns
  .indexes TABLE        Show a table's scalar indexes
  .save PATH            Save a database snapshot
  .open PATH            Replace the current database from a snapshot
  .read PATH            Execute SQL from a file
  .timer on|off         Show statement execution time
  .cancel               Discard the current multiline statement
  .quit                 Exit the shell

Terminate SQL statements with a semicolon.";

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [argument] if matches!(argument.as_str(), "--version" | "-V") => {
            println!("vectors {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        [argument] if matches!(argument.as_str(), "--help" | "-h") => {
            println!(
                "vectors {}\n\nUsage: vectors\n\nStarts the interactive SQL shell.\n\nOptions:\n  -h, --help       Show this help\n  -V, --version    Show version",
                env!("CARGO_PKG_VERSION")
            );
            return;
        }
        [] => {}
        _ => {
            eprintln!("error: unexpected argument; run 'vectors --help'");
            std::process::exit(2);
        }
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    if let Err(error) = run_shell(stdin.lock(), stdout.lock(), stderr.lock()) {
        eprintln!("error: {error}");
    }
}

fn run_shell(
    mut input: impl BufRead,
    mut output: impl Write,
    mut errors: impl Write,
) -> io::Result<()> {
    let mut database = Database::new();
    let mut statement = String::new();
    let mut timer_enabled = false;

    writeln!(
        output,
        "vectors {} | in-memory SQL vector database",
        env!("CARGO_PKG_VERSION")
    )?;
    writeln!(output, "Type .help for help. End SQL with ';'.")?;

    loop {
        let prompt = if statement.trim().is_empty() {
            "vectors> "
        } else {
            "     ...> "
        };
        write!(output, "{prompt}")?;
        output.flush()?;

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            if !statement.trim().is_empty() {
                writeln!(errors, "error: incomplete SQL discarded")?;
            }
            break;
        }

        let trimmed = line.trim();
        match parse_meta_command(trimmed) {
            Ok(Some(command)) => {
                let allowed_while_pending = matches!(
                    command,
                    MetaCommand::Help | MetaCommand::Cancel | MetaCommand::Quit
                );
                if !statement.trim().is_empty() && !allowed_while_pending {
                    writeln!(
                        errors,
                        "error: finish the current SQL statement or use .cancel"
                    )?;
                    continue;
                }
                if handle_meta_command(
                    command,
                    &mut database,
                    &mut statement,
                    &mut timer_enabled,
                    &mut output,
                    &mut errors,
                )? {
                    break;
                }
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                writeln!(errors, "error: {error}")?;
                continue;
            }
        }

        if trimmed.is_empty() && statement.trim().is_empty() {
            continue;
        }
        statement.push_str(&line);
        if !input_is_complete(&statement) {
            continue;
        }

        execute_sql(
            &database,
            &statement,
            timer_enabled,
            &mut output,
            &mut errors,
        )?;
        statement.clear();
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum MetaCommand {
    Help,
    Tables,
    Schema(String),
    Indexes(String),
    Save(String),
    Open(String),
    Read(String),
    Timer(bool),
    Cancel,
    Quit,
}

fn parse_meta_command(line: &str) -> Result<Option<MetaCommand>, String> {
    if !line.starts_with('.') {
        return Ok(None);
    }
    let (name, argument) = line
        .find(char::is_whitespace)
        .map(|index| (&line[..index], line[index..].trim()))
        .unwrap_or((line, ""));
    let name = name.to_ascii_lowercase();
    let no_argument = || {
        if argument.is_empty() {
            Ok(())
        } else {
            Err(format!("{name} does not accept an argument"))
        }
    };
    let required_argument = || {
        if argument.is_empty() {
            Err(format!("{name} requires an argument"))
        } else {
            Ok(unquote(argument))
        }
    };

    let command = match name.as_str() {
        ".help" => {
            no_argument()?;
            MetaCommand::Help
        }
        ".tables" => {
            no_argument()?;
            MetaCommand::Tables
        }
        ".schema" => MetaCommand::Schema(required_argument()?),
        ".indexes" => MetaCommand::Indexes(required_argument()?),
        ".save" => MetaCommand::Save(required_argument()?),
        ".open" => MetaCommand::Open(required_argument()?),
        ".read" => MetaCommand::Read(required_argument()?),
        ".timer" => match argument.to_ascii_lowercase().as_str() {
            "on" => MetaCommand::Timer(true),
            "off" => MetaCommand::Timer(false),
            _ => return Err(".timer expects 'on' or 'off'".into()),
        },
        ".cancel" => {
            no_argument()?;
            MetaCommand::Cancel
        }
        ".quit" | ".exit" => {
            no_argument()?;
            MetaCommand::Quit
        }
        _ => return Err(format!("unknown command '{name}'; type .help for help")),
    };
    Ok(Some(command))
}

fn unquote(argument: &str) -> String {
    let bytes = argument.as_bytes();
    if bytes.len() >= 2
        && matches!(bytes[0], b'\'' | b'"')
        && bytes.last().copied() == Some(bytes[0])
    {
        argument[1..argument.len() - 1].to_string()
    } else {
        argument.to_string()
    }
}

fn handle_meta_command(
    command: MetaCommand,
    database: &mut Database,
    statement: &mut String,
    timer_enabled: &mut bool,
    output: &mut impl Write,
    errors: &mut impl Write,
) -> io::Result<bool> {
    match command {
        MetaCommand::Help => writeln!(output, "{HELP}")?,
        MetaCommand::Tables => match database.tables() {
            Ok(tables) if tables.is_empty() => writeln!(output, "(no tables)")?,
            Ok(tables) => {
                let rows = tables
                    .into_iter()
                    .map(|table| vec![table])
                    .collect::<Vec<_>>();
                write_table(output, &["table".into()], &rows)?;
                writeln!(output, "({} table(s))", rows.len())?;
            }
            Err(error) => writeln!(errors, "error: {error}")?,
        },
        MetaCommand::Schema(table) => match database.schema(&table) {
            Ok(columns) => {
                let rows = columns
                    .into_iter()
                    .map(|column| {
                        vec![
                            column.name,
                            column.data_type.to_string(),
                            yes_no(column.nullable),
                            yes_no(column.unique),
                        ]
                    })
                    .collect::<Vec<_>>();
                write_table(
                    output,
                    &[
                        "column".into(),
                        "type".into(),
                        "nullable".into(),
                        "unique".into(),
                    ],
                    &rows,
                )?;
            }
            Err(error) => writeln!(errors, "error: {error}")?,
        },
        MetaCommand::Indexes(table) => match database.indexes(&table) {
            Ok(indexes) if indexes.is_empty() => {
                writeln!(output, "(no scalar indexes on {table})")?
            }
            Ok(indexes) => {
                let rows = indexes
                    .into_iter()
                    .map(|index| vec![index.name, index.column])
                    .collect::<Vec<_>>();
                write_table(output, &["index".into(), "column".into()], &rows)?;
            }
            Err(error) => writeln!(errors, "error: {error}")?,
        },
        MetaCommand::Save(path) => match database.save(&path) {
            Ok(()) => writeln!(output, "saved {path}")?,
            Err(error) => writeln!(errors, "error: {error}")?,
        },
        MetaCommand::Open(path) => match Database::open(&path) {
            Ok(opened) => {
                *database = opened;
                writeln!(output, "opened {path}")?;
            }
            Err(error) => writeln!(errors, "error: {error}")?,
        },
        MetaCommand::Read(path) => match fs::read_to_string(&path) {
            Ok(sql) => execute_sql(database, &sql, *timer_enabled, output, errors)?,
            Err(error) => writeln!(errors, "error: cannot read {path}: {error}")?,
        },
        MetaCommand::Timer(enabled) => {
            *timer_enabled = enabled;
            writeln!(output, "timer {}", if enabled { "on" } else { "off" })?;
        }
        MetaCommand::Cancel => {
            if statement.trim().is_empty() {
                writeln!(output, "(no pending statement)")?;
            } else {
                statement.clear();
                writeln!(output, "statement canceled")?;
            }
        }
        MetaCommand::Quit => return Ok(true),
    }
    Ok(false)
}

fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.into()
}

fn execute_sql(
    database: &Database,
    sql: &str,
    timer_enabled: bool,
    output: &mut impl Write,
    errors: &mut impl Write,
) -> io::Result<()> {
    let started = Instant::now();
    match database.execute(sql) {
        Ok(results) => {
            for result in results {
                print_result(output, result)?;
            }
        }
        Err(error) => writeln!(errors, "error: {error}")?,
    }
    if timer_enabled {
        writeln!(output, "Time: {}", format_duration(started.elapsed()))?;
    }
    Ok(())
}

fn format_duration(duration: Duration) -> String {
    if duration >= Duration::from_secs(1) {
        format!("{:.3} s", duration.as_secs_f64())
    } else {
        format!("{:.3} ms", duration.as_secs_f64() * 1_000.0)
    }
}

fn print_result(output: &mut impl Write, result: ExecutionResult) -> io::Result<()> {
    match result {
        ExecutionResult::Command { tag, rows_affected } => {
            writeln!(output, "{tag} ({rows_affected} row(s))")
        }
        ExecutionResult::Query(result) => {
            if result.columns.is_empty() {
                return writeln!(output, "(empty result)");
            }
            let row_count = result.row_count();
            let rows_examined = result.rows_examined;
            let columns = result
                .columns
                .iter()
                .map(|column| printable_text(column))
                .collect::<Vec<_>>();
            let rows = result
                .rows
                .iter()
                .map(|row| row.iter().map(printable_value).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            write_table(output, &columns, &rows)?;
            writeln!(output, "({row_count} row(s), {rows_examined} examined)")
        }
    }
}

fn printable_value(value: &Value) -> String {
    match value {
        Value::Text(value) => printable_text(value),
        value => value.to_string(),
    }
}

fn printable_text(value: &str) -> String {
    let mut printable = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => printable.push_str("\\n"),
            '\r' => printable.push_str("\\r"),
            '\t' => printable.push_str("\\t"),
            character if character.is_control() => printable.extend(character.escape_default()),
            character => printable.push(character),
        }
    }
    printable
}

fn write_table(
    output: &mut impl Write,
    columns: &[String],
    rows: &[Vec<String>],
) -> io::Result<()> {
    let mut widths = columns
        .iter()
        .map(|column| display_width(column))
        .collect::<Vec<_>>();
    for row in rows {
        for (index, value) in row.iter().enumerate().take(widths.len()) {
            widths[index] = widths[index].max(display_width(value));
        }
    }
    write_table_row(output, columns, &widths)?;
    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join("-+-");
    writeln!(output, "{separator}")?;
    for row in rows {
        write_table_row(output, row, &widths)?;
    }
    Ok(())
}

fn write_table_row(output: &mut impl Write, cells: &[String], widths: &[usize]) -> io::Result<()> {
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            write!(output, " | ")?;
        }
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        write!(output, "{cell}{}", " ".repeat(width - display_width(cell)))?;
    }
    writeln!(output)
}

fn display_width(value: &str) -> usize {
    value.chars().count()
}

#[derive(Clone, Copy)]
enum InputState {
    Normal,
    SingleQuoted,
    DoubleQuoted,
    LineComment,
    BlockComment,
}

fn input_is_complete(input: &str) -> bool {
    let mut state = InputState::Normal;
    let mut complete = false;
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        match state {
            InputState::Normal => match character {
                character if character.is_whitespace() => {}
                '-' if characters.peek() == Some(&'-') => {
                    characters.next();
                    state = InputState::LineComment;
                }
                '/' if characters.peek() == Some(&'*') => {
                    characters.next();
                    state = InputState::BlockComment;
                }
                '\'' => {
                    complete = false;
                    state = InputState::SingleQuoted;
                }
                '"' => {
                    complete = false;
                    state = InputState::DoubleQuoted;
                }
                ';' => complete = true,
                _ => complete = false,
            },
            InputState::SingleQuoted => {
                if character == '\'' {
                    if characters.peek() == Some(&'\'') {
                        characters.next();
                    } else {
                        state = InputState::Normal;
                    }
                }
            }
            InputState::DoubleQuoted => {
                if character == '"' {
                    if characters.peek() == Some(&'"') {
                        characters.next();
                    } else {
                        state = InputState::Normal;
                    }
                }
            }
            InputState::LineComment => {
                if character == '\n' {
                    state = InputState::Normal;
                }
            }
            InputState::BlockComment => {
                if character == '*' && characters.peek() == Some(&'/') {
                    characters.next();
                    state = InputState::Normal;
                }
            }
        }
    }
    complete && matches!(state, InputState::Normal | InputState::LineComment)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn parses_shell_commands_and_quoted_paths() {
        assert_eq!(parse_meta_command("select 1"), Ok(None));
        assert_eq!(parse_meta_command(".tables"), Ok(Some(MetaCommand::Tables)));
        assert_eq!(
            parse_meta_command(".save \"my database.vdb\""),
            Ok(Some(MetaCommand::Save("my database.vdb".into())))
        );
        assert_eq!(
            parse_meta_command(".timer ON"),
            Ok(Some(MetaCommand::Timer(true)))
        );
        assert!(parse_meta_command(".schema").is_err());
        assert!(parse_meta_command(".unknown").is_err());
    }

    #[test]
    fn detects_complete_sql_outside_literals_and_comments() {
        assert!(input_is_complete("SELECT 1;"));
        assert!(input_is_complete("SELECT 1; -- trailing comment"));
        assert!(input_is_complete("SELECT ';'; /* trailing comment */"));
        assert!(!input_is_complete("SELECT ';'"));
        assert!(!input_is_complete("SELECT 1; SELECT 2"));
        assert!(!input_is_complete("SELECT 1; /* unfinished"));
    }

    #[test]
    fn shell_lists_metadata_and_aligns_query_results() {
        let input = Cursor::new(
            ".help\n\
             CREATE TABLE entries (\n\
                 id INTEGER PRIMARY KEY,\n\
                 title TEXT\n\
             );\n\
             INSERT INTO entries VALUES (1, 'short'), (2, 'longer title');\n\
             .tables\n\
             .schema entries\n\
             SELECT id, title FROM entries ORDER BY id;\n\
             .quit\n",
        );
        let mut output = Vec::new();
        let mut errors = Vec::new();
        run_shell(input, &mut output, &mut errors).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(concat!(
            "vectors ",
            env!("CARGO_PKG_VERSION"),
            " | in-memory SQL vector database"
        )));
        assert!(output.contains("     ...> "));
        assert!(output.contains(".indexes TABLE"));
        assert!(output.contains("table  \n-------\nentries"));
        assert!(output.contains("column | type    | nullable | unique"));
        assert!(output.contains("id | title       \n---+-------------"));
        assert!(output.contains("1  | short       \n2  | longer title"));
        assert!(errors.is_empty(), "{}", String::from_utf8_lossy(&errors));
    }

    #[test]
    fn escapes_control_characters_in_table_cells() {
        assert_eq!(
            printable_text("line one\nline two\tend"),
            "line one\\nline two\\tend"
        );
    }
}
