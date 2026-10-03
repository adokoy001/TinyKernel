//! Bounded, typed shell records. This module is pure `core` logic: it never
//! accesses hardware, allocates, or executes a shell command.
//!
//! Parse every transform against the preceding stage's `output_schema` before
//! invoking a source. Applying a prepared transform checks its input schema
//! again. Every capacity and type failure is explicit; data is never truncated.

use core::cmp::Ordering;
use core::fmt::{self, Write};

pub const MAX_ROWS: usize = 64;
pub const MAX_COLUMNS: usize = 8;
pub const MAX_TEXT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind { Null, Bool, Int, UInt, Text }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: &'static str,
    pub kind: Kind,
}

impl Column {
    pub const fn new(name: &'static str, kind: Kind) -> Self { Self { name, kind } }
}

const EMPTY_COLUMN: Column = Column::new("", Kind::Null);

/// UTF-8 text bounded in bytes, rather than truncated in the middle of a code
/// point. Construction is the only way to populate this representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Text {
    bytes: [u8; MAX_TEXT],
    len: u8,
}

impl Text {
    pub const EMPTY: Self = Self { bytes: [0; MAX_TEXT], len: 0 };

    pub fn new(value: &str) -> Result<Self, Error> {
        if value.len() > MAX_TEXT { return Err(Error::TextTooLong); }
        let mut text = Self::EMPTY;
        text.bytes[..value.len()].copy_from_slice(value.as_bytes());
        text.len = value.len() as u8;
        Ok(text)
    }

    pub fn as_str(&self) -> &str {
        // Only new() writes bytes, from a valid UTF-8 string, and len is private.
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap()
    }

    pub fn len(&self) -> usize { self.len as usize }
    pub fn is_empty(&self) -> bool { self.len == 0 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Text(Text),
}

impl Cell {
    pub fn text(value: &str) -> Result<Self, Error> { Ok(Self::Text(Text::new(value)?)) }

    pub const fn kind(self) -> Kind {
        match self {
            Self::Null => Kind::Null,
            Self::Bool(_) => Kind::Bool,
            Self::Int(_) => Kind::Int,
            Self::UInt(_) => Kind::UInt,
            Self::Text(_) => Kind::Text,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    TooManyColumns,
    TooManyRows,
    TextTooLong,
    InvalidSchema,
    WrongRowWidth,
    TypeMismatch,
    UnknownColumn,
    DuplicateColumn,
    InvalidOperator,
    InvalidValue,
    InvalidArguments,
    UnknownTransform,
    TakeOutOfRange,
    SchemaChanged,
    Write,
}

impl Error {
    pub const fn description(self) -> &'static str {
        match self {
            Self::TooManyColumns => "at most 8 columns are supported",
            Self::TooManyRows => "at most 64 records are supported",
            Self::TextTooLong => "text exceeds 64 UTF-8 bytes",
            Self::InvalidSchema => "column names must be nonempty and unique",
            Self::WrongRowWidth => "record width does not match the schema",
            Self::TypeMismatch => "value type does not match the column",
            Self::UnknownColumn => "unknown column",
            Self::DuplicateColumn => "column selected more than once",
            Self::InvalidOperator => "expected ==, !=, <, <=, >, or >=",
            Self::InvalidValue => "invalid value for the column type",
            Self::InvalidArguments => "invalid transform arguments",
            Self::UnknownTransform => "unknown record transform",
            Self::TakeOutOfRange => "take count must be between 0 and 64",
            Self::SchemaChanged => "prepared transform has a different input schema",
            Self::Write => "output writer failed",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.description())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schema {
    columns: [Column; MAX_COLUMNS],
    len: u8,
}

impl Schema {
    pub const fn empty() -> Self { Self { columns: [EMPTY_COLUMN; MAX_COLUMNS], len: 0 } }

    pub fn new(columns: &[Column]) -> Result<Self, Error> {
        if columns.len() > MAX_COLUMNS { return Err(Error::TooManyColumns); }
        for (index, column) in columns.iter().enumerate() {
            if column.name.is_empty()
                || columns[..index].iter().any(|previous| previous.name == column.name) {
                return Err(Error::InvalidSchema);
            }
        }
        let mut result = Self::empty();
        result.columns[..columns.len()].copy_from_slice(columns);
        result.len = columns.len() as u8;
        Ok(result)
    }

    pub fn columns(&self) -> &[Column] { &self.columns[..self.len as usize] }
    pub fn len(&self) -> usize { self.len as usize }
    pub fn is_empty(&self) -> bool { self.len == 0 }
}

/// Place the table in persistent storage in the kernel. Rows are transformed
/// in place; no operation copies this entire table onto a small task stack.
pub struct Table {
    schema: Schema,
    rows: [[Cell; MAX_COLUMNS]; MAX_ROWS],
    len: u8,
}

impl Table {
    pub const fn new() -> Self {
        Self { schema: Schema::empty(), rows: [[Cell::Null; MAX_COLUMNS]; MAX_ROWS], len: 0 }
    }

    /// Schema validation precedes mutation, so a rejected reset preserves data.
    pub fn reset(&mut self, columns: &[Column]) -> Result<(), Error> {
        let schema = Schema::new(columns)?;
        self.schema = schema;
        self.len = 0;
        Ok(())
    }

    /// Erase retained record payloads when crossing a privilege boundary.
    /// Unlike reset(), this clears every byte of the backing row storage.
    pub fn clear(&mut self) {
        // Cell has an explicit u8 discriminant, and Null is discriminant 0.
        // All-zero bytes therefore represent a valid Null for every cell;
        // its inactive payload contains no references or required invariants.
        unsafe { core::ptr::write_bytes(self.rows.as_mut_ptr(), 0, MAX_ROWS); }
        self.schema = Schema::empty();
        self.len = 0;
    }

    /// Null is an explicit missing value accepted by any declared kind; every
    /// non-null value must have exactly that kind (Int and UInt stay distinct).
    pub fn push_row(&mut self, cells: &[Cell]) -> Result<(), Error> {
        if cells.len() != self.schema.len() { return Err(Error::WrongRowWidth); }
        if self.len() == MAX_ROWS { return Err(Error::TooManyRows); }
        for (cell, column) in cells.iter().zip(self.schema()) {
            if *cell != Cell::Null && cell.kind() != column.kind {
                return Err(Error::TypeMismatch);
            }
        }
        let row = &mut self.rows[self.len as usize];
        row[..cells.len()].copy_from_slice(cells);
        row[cells.len()..].fill(Cell::Null);
        self.len += 1;
        Ok(())
    }

    pub fn len(&self) -> usize { self.len as usize }
    pub fn is_empty(&self) -> bool { self.len == 0 }
    pub fn schema(&self) -> &[Column] { self.schema.columns() }

    pub fn row(&self, index: usize) -> Option<&[Cell]> {
        if index < self.len() { Some(&self.rows[index][..self.schema.len()]) } else { None }
    }

    pub fn get(&self, row: usize, column: usize) -> Option<&Cell> {
        self.row(row)?.get(column)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operator { Eq, Ne, Lt, Le, Gt, Ge }

impl Operator {
    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "==" => Ok(Self::Eq), "!=" => Ok(Self::Ne),
            "<" => Ok(Self::Lt), "<=" => Ok(Self::Le),
            ">" => Ok(Self::Gt), ">=" => Ok(Self::Ge),
            _ => Err(Error::InvalidOperator),
        }
    }

    fn matches(self, left: Cell, right: Cell) -> bool {
        // Missing values can be tested for equality; they never satisfy an
        // ordered predicate (sorting separately gives them a fixed position).
        if left == Cell::Null || right == Cell::Null {
            return match self {
                Self::Eq => left == right,
                Self::Ne => left != right,
                _ => false,
            };
        }
        let comparison = compare_cells(left, right);
        match self {
            Self::Eq => comparison == Ordering::Equal,
            Self::Ne => comparison != Ordering::Equal,
            Self::Lt => comparison == Ordering::Less,
            Self::Le => comparison != Ordering::Greater,
            Self::Gt => comparison == Ordering::Greater,
            Self::Ge => comparison != Ordering::Less,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Where { column: u8, operator: Operator, value: Cell },
    Select { indices: [u8; MAX_COLUMNS], len: u8 },
    Sort { column: u8, descending: bool },
    Take { len: u8 },
    Count,
}

/// A transform prepared entirely from words and schema, before source effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transform {
    input: Schema,
    output: Schema,
    action: Action,
}

impl Transform {
    pub fn parse(columns: &[Column], words: &[&str]) -> Result<Self, Error> {
        let input = Schema::new(columns)?;
        let mut output = input;
        let command = words.first().copied().ok_or(Error::InvalidArguments)?;
        let action = match command {
            "where" => {
                if words.len() != 4 { return Err(Error::InvalidArguments); }
                let column = find_column(columns, words[1])?;
                let operator = Operator::parse(words[2])?;
                let value = parse_value(columns[column].kind, words[3])?;
                if value == Cell::Null && !matches!(operator, Operator::Eq | Operator::Ne) {
                    return Err(Error::InvalidValue);
                }
                Action::Where { column: column as u8, operator, value }
            }
            "select" => {
                if words.len() < 2 { return Err(Error::InvalidArguments); }
                if words.len() - 1 > MAX_COLUMNS { return Err(Error::TooManyColumns); }
                let mut indices = [0u8; MAX_COLUMNS];
                let mut selected = [EMPTY_COLUMN; MAX_COLUMNS];
                for (index, name) in words[1..].iter().enumerate() {
                    let source = find_column(columns, name)? as u8;
                    if indices[..index].contains(&source) { return Err(Error::DuplicateColumn); }
                    indices[index] = source;
                    selected[index] = columns[source as usize];
                }
                output = Schema::new(&selected[..words.len() - 1])?;
                Action::Select { indices, len: (words.len() - 1) as u8 }
            }
            "sort" => {
                if words.len() != 2 && words.len() != 3 { return Err(Error::InvalidArguments); }
                let column = find_column(columns, words[1])?;
                let descending = words.len() == 3;
                if descending && words[2] != "--desc" { return Err(Error::InvalidArguments); }
                Action::Sort { column: column as u8, descending }
            }
            "take" => {
                if words.len() != 2 { return Err(Error::InvalidArguments); }
                // Parsing usize avoids arithmetic overflow for hostile tokens.
                let len = words[1].parse::<usize>().map_err(|_| Error::TakeOutOfRange)?;
                if len > MAX_ROWS { return Err(Error::TakeOutOfRange); }
                Action::Take { len: len as u8 }
            }
            "count" => {
                if words.len() != 1 { return Err(Error::InvalidArguments); }
                output = Schema::new(&[Column::new("count", Kind::UInt)])?;
                Action::Count
            }
            _ => return Err(Error::UnknownTransform),
        };
        Ok(Self { input, output, action })
    }

    pub fn output_schema(&self) -> &[Column] { self.output.columns() }

    /// After the schema check this cannot fail or leave a partial transform.
    pub fn apply(&self, table: &mut Table) -> Result<(), Error> {
        if table.schema != self.input { return Err(Error::SchemaChanged); }
        match self.action {
            Action::Where { column, operator, value } => {
                let mut destination = 0;
                for source in 0..table.len() {
                    if operator.matches(table.rows[source][column as usize], value) {
                        if destination != source { table.rows[destination] = table.rows[source]; }
                        destination += 1;
                    }
                }
                table.len = destination as u8;
            }
            Action::Select { indices, len } => {
                for index in 0..table.len() {
                    // A single bounded row is the largest temporary, 640 bytes
                    // on the x86_64 target; reversed column orders remain safe.
                    let previous = table.rows[index];
                    for column in 0..len as usize {
                        table.rows[index][column] = previous[indices[column] as usize];
                    }
                    table.rows[index][len as usize..].fill(Cell::Null);
                }
            }
            Action::Sort { column, descending } => {
                // Insertion sort uses one row, is bounded by 64^2 comparisons,
                // and never swaps equal keys, preserving their source order.
                for index in 1..table.len() {
                    let row = table.rows[index];
                    let mut destination = index;
                    while destination > 0 {
                        let comparison = compare_cells(
                            table.rows[destination - 1][column as usize], row[column as usize]);
                        let move_row = if descending { comparison == Ordering::Less }
                                       else { comparison == Ordering::Greater };
                        if !move_row { break; }
                        table.rows[destination] = table.rows[destination - 1];
                        destination -= 1;
                    }
                    table.rows[destination] = row;
                }
            }
            Action::Take { len } => table.len = table.len.min(len),
            Action::Count => {
                let count = table.len as u64;
                table.rows[0].fill(Cell::Null);
                table.rows[0][0] = Cell::UInt(count);
                table.len = 1;
            }
        }
        table.schema = self.output;
        Ok(())
    }
}

fn find_column(columns: &[Column], name: &str) -> Result<usize, Error> {
    columns.iter().position(|column| column.name == name).ok_or(Error::UnknownColumn)
}

fn parse_value(kind: Kind, value: &str) -> Result<Cell, Error> {
    // In a text column every token is text, including "null". Numeric and bool
    // columns accept the null keyword as an explicit missing-value predicate.
    if kind != Kind::Text && value == "null" { return Ok(Cell::Null); }
    match kind {
        Kind::Null => Err(Error::TypeMismatch),
        Kind::Bool => match value {
            "true" => Ok(Cell::Bool(true)), "false" => Ok(Cell::Bool(false)),
            _ => Err(Error::TypeMismatch),
        },
        Kind::Int | Kind::UInt => {
            if let Ok(number) = value.parse::<i64>() { return Ok(Cell::Int(number)); }
            if let Ok(number) = value.parse::<u64>() { return Ok(Cell::UInt(number)); }
            Err(Error::TypeMismatch)
        }
        Kind::Text => Cell::text(value),
    }
}

fn compare_cells(left: Cell, right: Cell) -> Ordering {
    match (left, right) {
        (Cell::Null, Cell::Null) => Ordering::Equal,
        (Cell::Null, _) => Ordering::Less,
        (_, Cell::Null) => Ordering::Greater,
        (Cell::Bool(left), Cell::Bool(right)) => left.cmp(&right),
        (Cell::Int(left), Cell::Int(right)) => left.cmp(&right),
        (Cell::UInt(left), Cell::UInt(right)) => left.cmp(&right),
        (Cell::Int(left), Cell::UInt(right)) => {
            if left < 0 { Ordering::Less } else { (left as u64).cmp(&right) }
        }
        (Cell::UInt(left), Cell::Int(right)) => {
            if right < 0 { Ordering::Greater } else { left.cmp(&(right as u64)) }
        }
        (Cell::Text(left), Cell::Text(right)) => left.as_str().cmp(right.as_str()),
        // Private callers reach this only with validated schema/literals. An
        // explicit total order keeps the helper deterministic even in tests.
        (left, right) => kind_order(left.kind()).cmp(&kind_order(right.kind())),
    }
}

fn kind_order(kind: Kind) -> u8 {
    match kind { Kind::Null => 0, Kind::Bool => 1, Kind::Int => 2, Kind::UInt => 3, Kind::Text => 4 }
}

/// Compact JSON array of typed objects followed by a newline. Text and column
/// names escape quotes, backslashes and all JSON control characters.
pub fn write_json<W: Write + ?Sized>(table: &Table, out: &mut W) -> Result<(), Error> {
    write_json_inner(table, out).map_err(|_| Error::Write)
}

fn write_json_inner<W: Write + ?Sized>(table: &Table, out: &mut W) -> fmt::Result {
    out.write_char('[')?;
    for row in 0..table.len() {
        if row != 0 { out.write_char(',')?; }
        out.write_char('{')?;
        for (index, column) in table.schema().iter().enumerate() {
            if index != 0 { out.write_char(',')?; }
            write_json_string(column.name, out)?;
            out.write_char(':')?;
            match table.rows[row][index] {
                Cell::Text(text) => write_json_string(text.as_str(), out)?,
                cell => write_scalar(cell, out)?,
            }
        }
        out.write_char('}')?;
    }
    out.write_str("]\n")
}

fn write_json_string<W: Write + ?Sized>(text: &str, out: &mut W) -> fmt::Result {
    out.write_char('"')?;
    for character in text.chars() {
        match character {
            '"' => out.write_str("\\\"")?, '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?, '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?, '\u{8}' => out.write_str("\\b")?,
            '\u{c}' => out.write_str("\\f")?,
            character if character < '\u{20}' => write!(out, "\\u{:04x}", character as u32)?,
            character => out.write_char(character)?,
        }
    }
    out.write_char('"')
}

fn write_scalar<W: Write + ?Sized>(cell: Cell, out: &mut W) -> fmt::Result {
    match cell {
        Cell::Null => out.write_str("null"),
        Cell::Bool(value) => out.write_str(if value { "true" } else { "false" }),
        Cell::Int(value) => write!(out, "{}", value),
        Cell::UInt(value) => write!(out, "{}", value),
        Cell::Text(value) => write_escaped(value.as_str(), out),
    }
}

/// Human-readable aligned table, with a header even when there are no records.
/// Control characters are escaped instead of changing terminal state.
pub fn write_table<W: Write + ?Sized>(table: &Table, out: &mut W) -> Result<(), Error> {
    write_table_inner(table, out).map_err(|_| Error::Write)
}

fn write_table_inner<W: Write + ?Sized>(table: &Table, out: &mut W) -> fmt::Result {
    let mut widths = [0usize; MAX_COLUMNS];
    for (index, column) in table.schema().iter().enumerate() {
        widths[index] = escaped_width(column.name);
        for row in 0..table.len() {
            widths[index] = widths[index].max(cell_width(table.rows[row][index]));
        }
    }
    for (index, column) in table.schema().iter().enumerate() {
        write_escaped(column.name, out)?;
        if index + 1 != table.schema.len() {
            padding(out, widths[index] - escaped_width(column.name) + 2)?;
        }
    }
    out.write_char('\n')?;
    for row in 0..table.len() {
        for index in 0..table.schema.len() {
            write_scalar(table.rows[row][index], out)?;
            if index + 1 != table.schema.len() {
                padding(out, widths[index] - cell_width(table.rows[row][index]) + 2)?;
            }
        }
        out.write_char('\n')?;
    }
    Ok(())
}

/// A tab-separated text view suitable for saving to a bounded file writer.
/// Tabs, newlines, carriage returns, backslashes and controls inside cells are
/// escaped; no row or column delimiters can be injected from text values.
pub fn write_tsv<W: Write + ?Sized>(table: &Table, out: &mut W) -> Result<(), Error> {
    write_tsv_inner(table, out).map_err(|_| Error::Write)
}

fn write_tsv_inner<W: Write + ?Sized>(table: &Table, out: &mut W) -> fmt::Result {
    for (index, column) in table.schema().iter().enumerate() {
        if index != 0 { out.write_char('\t')?; }
        write_escaped(column.name, out)?;
    }
    out.write_char('\n')?;
    for row in 0..table.len() {
        for index in 0..table.schema.len() {
            if index != 0 { out.write_char('\t')?; }
            write_scalar(table.rows[row][index], out)?;
        }
        out.write_char('\n')?;
    }
    Ok(())
}

fn write_escaped<W: Write + ?Sized>(text: &str, out: &mut W) -> fmt::Result {
    for character in text.chars() {
        match character {
            '\\' => out.write_str("\\\\")?, '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?, '\t' => out.write_str("\\t")?,
            character if character.is_control() =>
                write!(out, "\\u{:04x}", character as u32)?,
            character => out.write_char(character)?,
        }
    }
    Ok(())
}

fn escaped_width(text: &str) -> usize {
    text.chars().map(|character| match character {
        '\\' | '\n' | '\r' | '\t' => 2,
        character if character.is_control() => 6,
        _ => 1,
    }).sum()
}

fn cell_width(cell: Cell) -> usize {
    match cell {
        Cell::Null => 4,
        Cell::Bool(value) => if value { 4 } else { 5 },
        Cell::Int(value) => digits(value.unsigned_abs()) + if value < 0 { 1 } else { 0 },
        Cell::UInt(value) => digits(value),
        Cell::Text(value) => escaped_width(value.as_str()),
    }
}

fn digits(mut value: u64) -> usize {
    let mut count = 1;
    while value >= 10 { value /= 10; count += 1; }
    count
}

fn padding<W: Write + ?Sized>(out: &mut W, count: usize) -> fmt::Result {
    for _ in 0..count { out.write_char(' ')?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NUMBER: Column = Column::new("number", Kind::UInt);
    const NAME: Column = Column::new("name", Kind::Text);

    fn numbered() -> Table {
        let mut table = Table::new();
        table.reset(&[NUMBER, NAME]).unwrap();
        for (number, name) in [(10, "ten"), (2, "two-a"), (2, "two-b"), (u64::MAX, "max")] {
            table.push_row(&[Cell::UInt(number), Cell::text(name).unwrap()]).unwrap();
        }
        table
    }

    #[test]
    fn numeric_sort_is_stable_in_both_directions() {
        let mut table = numbered();
        Transform::parse(table.schema(), &["sort", "number"]).unwrap().apply(&mut table).unwrap();
        assert_eq!(table.get(0, 0), Some(&Cell::UInt(2)));
        assert_eq!(table.get(1, 1), Some(&Cell::text("two-b").unwrap()));
        assert_eq!(table.get(2, 0), Some(&Cell::UInt(10)));
        Transform::parse(table.schema(), &["sort", "number", "--desc"]).unwrap().apply(&mut table).unwrap();
        assert_eq!(table.get(0, 0), Some(&Cell::UInt(u64::MAX)));
        assert_eq!(table.get(2, 1), Some(&Cell::text("two-a").unwrap()));
        assert_eq!(table.get(3, 1), Some(&Cell::text("two-b").unwrap()));
    }

    #[test]
    fn signed_unsigned_comparison_never_overflows() {
        assert_eq!(compare_cells(Cell::Int(-1), Cell::UInt(u64::MAX)), Ordering::Less);
        assert_eq!(compare_cells(Cell::UInt(u64::MAX), Cell::Int(i64::MAX)), Ordering::Greater);
        assert_eq!(compare_cells(Cell::Int(i64::MIN), Cell::UInt(0)), Ordering::Less);
        assert_eq!(compare_cells(Cell::UInt(7), Cell::Int(7)), Ordering::Equal);
        let mut table = numbered();
        Transform::parse(table.schema(), &["where", "number", ">", "-1"]).unwrap().apply(&mut table).unwrap();
        assert_eq!(table.len(), 4);
        Transform::parse(table.schema(), &["where", "number", ">", "9223372036854775807"])
            .unwrap().apply(&mut table).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.get(0, 0), Some(&Cell::UInt(u64::MAX)));
        let mut signed = Table::new();
        signed.reset(&[Column::new("value", Kind::Int)]).unwrap();
        signed.push_row(&[Cell::Int(i64::MIN)]).unwrap();
        signed.push_row(&[Cell::Int(i64::MAX)]).unwrap();
        Transform::parse(signed.schema(), &["where", "value", "<", "18446744073709551615"])
            .unwrap().apply(&mut signed).unwrap();
        assert_eq!(signed.len(), 2);
    }

    #[test]
    fn validation_rejects_types_and_arguments_before_execution() {
        let table = numbered();
        assert_eq!(Transform::parse(table.schema(), &["where", "number", "==", "true"]), Err(Error::TypeMismatch));
        assert_eq!(Transform::parse(table.schema(), &["where", "number", "=", "2"]), Err(Error::InvalidOperator));
        assert_eq!(Transform::parse(table.schema(), &["where", "missing", "==", "2"]), Err(Error::UnknownColumn));
        assert_eq!(Transform::parse(table.schema(), &["select", "name", "name"]), Err(Error::DuplicateColumn));
        assert_eq!(Transform::parse(table.schema(), &["sort", "number", "--bogus"]), Err(Error::InvalidArguments));
        assert_eq!(Transform::parse(table.schema(), &["count", "extra"]), Err(Error::InvalidArguments));
        for count in ["65", "-1", "184467440737095516160"] {
            assert_eq!(Transform::parse(table.schema(), &["take", count]), Err(Error::TakeOutOfRange));
        }
    }

    #[test]
    fn schema_projection_prevalidation_and_empty_count() {
        let mut table = numbered();
        let select = Transform::parse(table.schema(), &["select", "name", "number"]).unwrap();
        let filter = Transform::parse(select.output_schema(), &["where", "number", ">", "18446744073709551615"]).unwrap();
        let count = Transform::parse(filter.output_schema(), &["count"]).unwrap();
        assert_eq!(Transform::parse(select.output_schema(), &["where", "missing", "==", "1"]), Err(Error::UnknownColumn));
        select.apply(&mut table).unwrap();
        assert_eq!(table.get(0, 0), Some(&Cell::text("ten").unwrap()));
        filter.apply(&mut table).unwrap();
        assert!(table.is_empty());
        assert_eq!(table.schema(), &[NAME, NUMBER]);
        count.apply(&mut table).unwrap();
        assert_eq!(table.schema(), &[Column::new("count", Kind::UInt)]);
        assert_eq!(table.row(0), Some(&[Cell::UInt(0)][..]));
        assert_eq!(select.apply(&mut table), Err(Error::SchemaChanged));
    }

    #[test]
    fn capacity_and_utf8_are_explicit_and_atomic() {
        let mut table = Table::new();
        table.reset(&[NUMBER]).unwrap();
        assert_eq!(table.push_row(&[Cell::Int(1)]), Err(Error::TypeMismatch));
        assert_eq!(table.push_row(&[]), Err(Error::WrongRowWidth));
        assert_eq!(table.len(), 0);
        for number in 0..MAX_ROWS { table.push_row(&[Cell::UInt(number as u64)]).unwrap(); }
        assert_eq!(table.push_row(&[Cell::UInt(99)]), Err(Error::TooManyRows));
        assert_eq!(table.len(), MAX_ROWS);
        assert_eq!(table.reset(&[NUMBER, NUMBER]), Err(Error::InvalidSchema));
        assert_eq!(table.len(), MAX_ROWS);
        let text = "é".repeat(32);
        assert_eq!(Text::new(&text).unwrap().as_str(), text);
        assert_eq!(Text::new(&(text + "é")), Err(Error::TextTooLong));
        assert_eq!(table.reset(&[Column::new("", Kind::Int)]), Err(Error::InvalidSchema));
        assert_eq!(table.reset(&[NUMBER; 9]), Err(Error::TooManyColumns));
    }

    #[test]
    fn null_is_missing_and_typed_boolean_filter_works() {
        let mut table = Table::new();
        table.reset(&[Column::new("ready", Kind::Bool)]).unwrap();
        table.push_row(&[Cell::Bool(true)]).unwrap();
        table.push_row(&[Cell::Null]).unwrap();
        table.push_row(&[Cell::Bool(false)]).unwrap();
        Transform::parse(table.schema(), &["sort", "ready"]).unwrap().apply(&mut table).unwrap();
        assert_eq!(table.get(0, 0), Some(&Cell::Null));
        Transform::parse(table.schema(), &["where", "ready", "!=", "null"]).unwrap().apply(&mut table).unwrap();
        assert_eq!(table.len(), 2);
        Transform::parse(table.schema(), &["where", "ready", "==", "true"]).unwrap().apply(&mut table).unwrap();
        assert_eq!(table.row(0), Some(&[Cell::Bool(true)][..]));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn json_keeps_types_and_escapes_all_control_characters() {
        let mut table = Table::new();
        table.reset(&[Column::new("a\"", Kind::Text), Column::new("min", Kind::Int),
            Column::new("max", Kind::UInt), Column::new("flag", Kind::Bool), Column::new("empty", Kind::Text)]).unwrap();
        table.push_row(&[Cell::text("\"\\\n\r\t\u{8}\u{c}\u{1}沖縄").unwrap(),
            Cell::Int(i64::MIN), Cell::UInt(u64::MAX), Cell::Bool(false), Cell::Null]).unwrap();
        let mut output = String::new();
        write_json(&table, &mut output).unwrap();
        assert_eq!(output, "[{\"a\\\"\":\"\\\"\\\\\\n\\r\\t\\b\\f\\u0001沖縄\",\"min\":-9223372036854775808,\"max\":18446744073709551615,\"flag\":false,\"empty\":null}]\n");
        table.reset(&[NAME]).unwrap();
        output.clear();
        write_json(&table, &mut output).unwrap();
        assert_eq!(output, "[]\n");
    }

    #[test]
    fn table_and_tsv_escape_cell_delimiters_and_terminal_controls() {
        let mut table = Table::new();
        table.reset(&[NAME, NUMBER]).unwrap();
        table.push_row(&[Cell::text("a\tb\n\\\u{1b}").unwrap(), Cell::UInt(2)]).unwrap();
        let mut output = String::new();
        write_tsv(&table, &mut output).unwrap();
        assert_eq!(output, "name\tnumber\na\\tb\\n\\\\\\u001b\t2\n");
        output.clear();
        write_table(&table, &mut output).unwrap();
        assert_eq!(output.lines().count(), 2);
        assert!(output.contains("a\\tb\\n\\\\\\u001b  2"));
        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn bounded_output_reports_writer_failure_for_every_format() {
        struct Limited { remaining: usize }
        impl Write for Limited {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                if text.len() > self.remaining { return Err(fmt::Error); }
                self.remaining -= text.len();
                Ok(())
            }
        }
        let table = numbered();
        assert_eq!(write_json(&table, &mut Limited { remaining: 10 }), Err(Error::Write));
        assert_eq!(write_table(&table, &mut Limited { remaining: 10 }), Err(Error::Write));
        assert_eq!(write_tsv(&table, &mut Limited { remaining: 10 }), Err(Error::Write));
    }

    #[test]
    fn take_zero_and_signed_extremes_render_without_panics() {
        let mut table = Table::new();
        table.reset(&[Column::new("value", Kind::Int)]).unwrap();
        table.push_row(&[Cell::Int(i64::MIN)]).unwrap();
        table.push_row(&[Cell::Int(10)]).unwrap();
        table.push_row(&[Cell::Int(-2)]).unwrap();
        Transform::parse(table.schema(), &["sort", "value"]).unwrap().apply(&mut table).unwrap();
        assert_eq!(table.get(1, 0), Some(&Cell::Int(-2)));
        let mut output = String::new();
        write_table(&table, &mut output).unwrap();
        assert!(output.contains("-9223372036854775808"));
        Transform::parse(table.schema(), &["take", "0"]).unwrap().apply(&mut table).unwrap();
        assert!(table.is_empty());
    }

    #[test]
    fn every_predicate_operator_preserves_typed_filter_semantics() {
        for (operator, expected) in [("==", 2), ("!=", 2), ("<", 0), ("<=", 2), (">", 2), (">=", 4)] {
            let mut table = numbered();
            Transform::parse(table.schema(), &["where", "number", operator, "2"])
                .unwrap().apply(&mut table).unwrap();
            assert_eq!(table.len(), expected, "operator {}", operator);
        }
        let mut table = numbered();
        table.push_row(&[Cell::Null, Cell::text("missing").unwrap()]).unwrap();
        Transform::parse(table.schema(), &["where", "number", "<", "3"])
            .unwrap().apply(&mut table).unwrap();
        assert_eq!(table.len(), 2, "missing values do not satisfy ordered predicates");
        Transform::parse(table.schema(), &["where", "name", "==", "two-b"])
            .unwrap().apply(&mut table).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.get(0, 1), Some(&Cell::text("two-b").unwrap()));
    }

    #[test]
    fn full_width_projection_reverses_without_overwriting_columns() {
        let columns = [Column::new("a", Kind::UInt), Column::new("b", Kind::UInt),
            Column::new("c", Kind::UInt), Column::new("d", Kind::UInt),
            Column::new("e", Kind::UInt), Column::new("f", Kind::UInt),
            Column::new("g", Kind::UInt), Column::new("h", Kind::UInt)];
        let mut table = Table::new();
        table.reset(&columns).unwrap();
        table.push_row(&[Cell::UInt(0), Cell::UInt(1), Cell::UInt(2), Cell::UInt(3),
            Cell::UInt(4), Cell::UInt(5), Cell::UInt(6), Cell::UInt(7)]).unwrap();
        Transform::parse(table.schema(), &["select", "h", "g", "f", "e", "d", "c", "b", "a"])
            .unwrap().apply(&mut table).unwrap();
        for index in 0..MAX_COLUMNS {
            assert_eq!(table.get(0, index), Some(&Cell::UInt((7 - index) as u64)));
        }
        assert_eq!(table.schema()[0].name, "h");
        assert_eq!(table.schema()[7].name, "a");
    }

    #[test]
    fn privilege_clear_erases_all_retained_cell_storage() {
        let mut table = numbered();
        table.clear();
        assert!(table.is_empty());
        assert!(table.schema().is_empty());
        let bytes = unsafe { core::slice::from_raw_parts(table.rows.as_ptr().cast::<u8>(),
            core::mem::size_of_val(&table.rows)) };
        assert!(bytes.iter().all(|byte| *byte == 0));
    }
}
