//! Column type inference and typed rows.

use std::fmt;

use csvread::CsvError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Float,
    Bool,
    Text,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Float(f64),
    Bool(bool),
    Text(String),
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Integer(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub columns: Vec<(String, ColumnType)>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    Csv(CsvError),
    Empty,
    /// A data row (1-based, header excluded) has the wrong number of fields.
    Ragged {
        row: usize,
        expected: usize,
        found: usize,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::Csv(e) => write!(f, "{e}"),
            TableError::Empty => write!(f, "no header row"),
            TableError::Ragged {
                row,
                expected,
                found,
            } => write!(f, "row {row}: expected {expected} fields, found {found}"),
        }
    }
}

impl std::error::Error for TableError {}

fn is_bool(s: &str) -> bool {
    matches!(s, "true" | "false")
}

fn infer(values: &[&str]) -> ColumnType {
    let present: Vec<&&str> = values.iter().filter(|v| !v.is_empty()).collect();
    if present.is_empty() {
        return ColumnType::Text;
    }
    if present.iter().all(|v| v.parse::<i64>().is_ok()) {
        ColumnType::Integer
    } else if present.iter().all(|v| v.parse::<f64>().is_ok()) {
        ColumnType::Float
    } else if present.iter().all(|v| is_bool(v)) {
        ColumnType::Bool
    } else {
        ColumnType::Text
    }
}

fn convert(raw: &str, ty: ColumnType) -> Value {
    if raw.is_empty() {
        return Value::Null;
    }
    match ty {
        ColumnType::Integer => Value::Integer(raw.parse().expect("inferred integer")),
        ColumnType::Float => Value::Float(raw.parse().expect("inferred float")),
        ColumnType::Bool => Value::Bool(raw == "true"),
        ColumnType::Text => Value::Text(raw.to_string()),
    }
}

/// Parse CSV with a header row into a typed table.
pub fn load(input: &str) -> Result<Table, TableError> {
    let mut records = csvread::parse(input).map_err(TableError::Csv)?.into_iter();
    let header = records.next().ok_or(TableError::Empty)?;
    let data: Vec<Vec<String>> = records.collect();
    for (i, r) in data.iter().enumerate() {
        if r.len() != header.len() {
            return Err(TableError::Ragged {
                row: i + 1,
                expected: header.len(),
                found: r.len(),
            });
        }
    }
    let mut columns = Vec::with_capacity(header.len());
    for (c, name) in header.iter().enumerate() {
        let col: Vec<&str> = data.iter().map(|r| r[c].as_str()).collect();
        columns.push((name.clone(), infer(&col)));
    }
    let rows = data
        .iter()
        .map(|r| {
            r.iter()
                .zip(&columns)
                .map(|(raw, (_, ty))| convert(raw, *ty))
                .collect()
        })
        .collect();
    Ok(Table { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_types() {
        let t = load("id,price,ok,name\n1,2.5,true,a\n2,3,false,b\n3,,true,\n").unwrap();
        let types: Vec<ColumnType> = t.columns.iter().map(|(_, t)| *t).collect();
        assert_eq!(
            types,
            [
                ColumnType::Integer,
                ColumnType::Float,
                ColumnType::Bool,
                ColumnType::Text
            ]
        );
        assert_eq!(t.rows[2][1], Value::Null);
    }

    #[test]
    fn ragged_rows_rejected() {
        assert_eq!(
            load("a,b\n1,2\n3\n"),
            Err(TableError::Ragged {
                row: 2,
                expected: 2,
                found: 1
            })
        );
    }
}
