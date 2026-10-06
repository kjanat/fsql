use std::io::{self, Write};

use crate::eval::render;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Table,
    Csv,
    Tsv,
    Json,
    Lines,
}

impl Format {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "csv" => Some(Self::Csv),
            "tsv" => Some(Self::Tsv),
            "json" | "jsonl" | "ndjson" => Some(Self::Json),
            "lines" | "plain" => Some(Self::Lines),
            _ => None,
        }
    }
}

pub struct ResultSet {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

pub fn write(out: &mut dyn Write, set: &ResultSet, format: Format) -> io::Result<()> {
    match format {
        Format::Table => write_table(out, set),
        Format::Csv => write_delimited(out, set, ',', true),
        Format::Tsv => write_delimited(out, set, '\t', false),
        Format::Json => write_json(out, set),
        Format::Lines => write_lines(out, set),
    }
}

/// Write a single record in a format that does not need whole-result layout.
pub fn write_row(
    out: &mut dyn Write,
    headers: &[String],
    row: &[Value],
    format: Format,
) -> io::Result<()> {
    if !matches!(format, Format::Json | Format::Lines) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "streaming output requires json or lines",
        ));
    }
    write(
        out,
        &ResultSet {
            headers: headers.to_vec(),
            rows: vec![row.to_vec()],
        },
        format,
    )
}

fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        other => render(other),
    }
}

fn numeric(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::Float(_))
}

fn write_table(out: &mut dyn Write, set: &ResultSet) -> io::Result<()> {
    let cells: Vec<Vec<String>> = set
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect())
        .collect();
    let mut widths: Vec<usize> = set.headers.iter().map(|h| h.chars().count()).collect();
    for row in &cells {
        for (index, text) in row.iter().enumerate() {
            let width = text.chars().count();
            if let Some(slot) = widths.get_mut(index) {
                *slot = (*slot).max(width);
            }
        }
    }
    let right: Vec<bool> = (0..set.headers.len())
        .map(|index| {
            let mut any = false;
            for row in &set.rows {
                match row.get(index) {
                    Some(Value::Null) | None => {}
                    Some(value) if numeric(value) => any = true,
                    Some(_) => return false,
                }
            }
            any
        })
        .collect();
    let line = |out: &mut dyn Write, parts: &[String]| -> io::Result<()> {
        let mut first = true;
        for (index, part) in parts.iter().enumerate() {
            if !first {
                out.write_all(b"  ")?;
            }
            first = false;
            let width = widths.get(index).copied().unwrap_or(0);
            let pad = width.saturating_sub(part.chars().count());
            if right.get(index).copied().unwrap_or(false) {
                write!(out, "{}{part}", " ".repeat(pad))?;
            } else if index + 1 == parts.len() {
                write!(out, "{part}")?;
            } else {
                write!(out, "{part}{}", " ".repeat(pad))?;
            }
        }
        out.write_all(b"\n")
    };
    line(out, &set.headers)?;
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    line(out, &rule)?;
    for row in &cells {
        line(out, row)?;
    }
    Ok(())
}

fn write_delimited(out: &mut dyn Write, set: &ResultSet, sep: char, quote: bool) -> io::Result<()> {
    let escape = |text: &str| -> String {
        if quote && (text.contains(sep) || text.contains('"') || text.contains('\n')) {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else if !quote {
            text.replace('\t', "\\t").replace('\n', "\\n")
        } else {
            text.to_owned()
        }
    };
    let header: Vec<String> = set.headers.iter().map(|h| escape(h)).collect();
    writeln!(out, "{}", header.join(&sep.to_string()))?;
    for row in &set.rows {
        let cells: Vec<String> = row.iter().map(|v| escape(&cell(v))).collect();
        writeln!(out, "{}", cells.join(&sep.to_string()))?;
    }
    Ok(())
}

pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Float(_) => "null".to_owned(),
        Value::Text(_) | Value::Blob(_) | Value::Timestamp(_) => json_string(&render(value)),
    }
}

fn write_json(out: &mut dyn Write, set: &ResultSet) -> io::Result<()> {
    for row in &set.rows {
        let fields: Vec<String> = set
            .headers
            .iter()
            .zip(row)
            .map(|(header, value)| format!("{}:{}", json_string(header), json_value(value)))
            .collect();
        writeln!(out, "{{{}}}", fields.join(","))?;
    }
    Ok(())
}

fn write_lines(out: &mut dyn Write, set: &ResultSet) -> io::Result<()> {
    for row in &set.rows {
        let cells: Vec<String> = row.iter().map(cell).collect();
        writeln!(out, "{}", cells.join("\t"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> ResultSet {
        ResultSet {
            headers: vec!["path".to_owned(), "size".to_owned(), "ext".to_owned()],
            rows: vec![
                vec![
                    Value::Text("/a/b.txt".to_owned()),
                    Value::Int(1234),
                    Value::Text("txt".to_owned()),
                ],
                vec![Value::Text("/a/c".to_owned()), Value::Int(5), Value::Null],
            ],
        }
    }

    fn render_as(format: Format) -> String {
        let mut buffer = Vec::new();
        write(&mut buffer, &set(), format).expect("write");
        String::from_utf8(buffer).expect("utf8")
    }

    #[test]
    fn table_aligns_numbers_right_and_text_left() {
        assert_eq!(
            render_as(Format::Table),
            "path      size  ext\n--------  ----  ---\n/a/b.txt  1234  txt\n/a/c         5  \n"
        );
    }

    #[test]
    fn csv_quotes_only_when_needed() {
        let mut custom = set();
        custom.rows[1][0] = Value::Text("/a/has,comma".to_owned());
        let mut buffer = Vec::new();
        write(&mut buffer, &custom, Format::Csv).expect("write");
        assert_eq!(
            String::from_utf8(buffer).expect("utf8"),
            "path,size,ext\n/a/b.txt,1234,txt\n\"/a/has,comma\",5,\n"
        );
    }

    #[test]
    fn json_is_one_object_per_line() {
        assert_eq!(
            render_as(Format::Json),
            "{\"path\":\"/a/b.txt\",\"size\":1234,\"ext\":\"txt\"}\n{\"path\":\"/a/c\",\"size\":5,\"ext\":null}\n"
        );
    }

    #[test]
    fn json_escapes_control_characters() {
        assert_eq!(json_string("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
    }
}
