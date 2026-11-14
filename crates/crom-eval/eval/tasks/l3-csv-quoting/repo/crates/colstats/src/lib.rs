//! Column summaries.

use std::collections::BTreeSet;

use schema::{ColumnType, Table, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub name: String,
    pub ty: ColumnType,
    pub count: usize,
    pub nulls: usize,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub mean: Option<f64>,
    pub distinct: usize,
}

pub fn summarize(table: &Table) -> Vec<Summary> {
    table
        .columns
        .iter()
        .enumerate()
        .map(|(c, (name, ty))| {
            let values: Vec<&Value> = table.rows.iter().map(|r| &r[c]).collect();
            let nulls = values.iter().filter(|v| matches!(v, Value::Null)).count();
            let nums: Vec<f64> = values.iter().filter_map(|v| v.as_f64()).collect();
            let distinct: BTreeSet<String> = values
                .iter()
                .filter(|v| !matches!(v, Value::Null))
                .map(|v| format!("{v:?}"))
                .collect();
            Summary {
                name: name.clone(),
                ty: *ty,
                count: values.len(),
                nulls,
                min: nums.iter().copied().reduce(f64::min),
                max: nums.iter().copied().reduce(f64::max),
                mean: if nums.is_empty() {
                    None
                } else {
                    Some(nums.iter().sum::<f64>() / nums.len() as f64)
                },
                distinct: distinct.len(),
            }
        })
        .collect()
}

/// Load CSV text and summarize every column.
pub fn summarize_csv(input: &str) -> Result<Vec<Summary>, schema::TableError> {
    Ok(summarize(&schema::load(input)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_summary() {
        let s = summarize_csv("x,label\n1,a\n3,b\n,a\n").unwrap();
        assert_eq!(s[0].count, 3);
        assert_eq!(s[0].nulls, 1);
        assert_eq!(s[0].min, Some(1.0));
        assert_eq!(s[0].max, Some(3.0));
        assert_eq!(s[0].mean, Some(2.0));
        assert_eq!(s[1].distinct, 2);
        assert_eq!(s[1].ty, ColumnType::Text);
    }
}
