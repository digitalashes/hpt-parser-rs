//! The row group being built: the only code that touches column writers.

use crate::Error;
use crate::config::{Column, Type, Value};
use ::parquet::data_type::{
    BoolType, ByteArray, ByteArrayType, DataType, DoubleType, Int32Type, Int64Type,
};
use ::parquet::file::writer::{SerializedColumnWriter, SerializedFileWriter};
use std::io::Write;

/// Rows buffered per column until [`Group::flush`] writes them as one row group.
///
/// ponytail: memory is `row_group_size` rows of raw values. The upgrade is a `PageWriter`
/// per column over an in-memory buffer (`get_column_writer`), encoding and compressing
/// pages as rows come, appended with `SerializedRowGroupWriter::append_column` at flush;
/// `push` and `flush` stay as they are.
pub(super) struct Group {
    columns: Vec<Buffer>,
    rows: usize,
}

/// A column's values with their definition levels. Nulls have a level and no value.
struct Buffer {
    values: Values,
    def: Vec<i16>,
}

enum Values {
    Bytes(Vec<ByteArray>),
    F64(Vec<f64>),
    I64(Vec<i64>),
    Bool(Vec<bool>),
    Date(Vec<i32>),
    /// The elements of every row's list, one after another, and their repetition levels.
    List(Vec<ByteArray>, Vec<i16>),
}

impl Group {
    pub(super) fn new(columns: &[Column]) -> Self {
        let columns = (columns.iter())
            .map(|c| Buffer {
                values: match c.kind {
                    Type::String | Type::Json => Values::Bytes(Vec::new()),
                    Type::F64 => Values::F64(Vec::new()),
                    Type::I64 => Values::I64(Vec::new()),
                    Type::Bool => Values::Bool(Vec::new()),
                    Type::Date => Values::Date(Vec::new()),
                    Type::List => Values::List(Vec::new(), Vec::new()),
                },
                def: Vec::new(),
            })
            .collect();
        Group { columns, rows: 0 }
    }

    pub(super) fn len(&self) -> usize {
        self.rows
    }

    /// One row: a value per column, in the columns' order.
    pub(super) fn push(&mut self, row: impl Iterator<Item = Value>) {
        for (buffer, value) in self.columns.iter_mut().zip(row) {
            buffer.push(value);
        }
        self.rows += 1;
    }

    /// Writes the rows as a row group and clears the buffers, keeping their capacity.
    /// Nothing is written when there are no rows.
    pub(super) fn flush<W: Write + Send>(
        &mut self,
        file: &mut SerializedFileWriter<W>,
    ) -> Result<(), Error> {
        if self.rows == 0 {
            return Ok(());
        }
        let mut group = file.next_row_group()?;
        for b in &mut self.columns {
            let mut column = group.next_column()?.expect("the schema has a column per buffer");
            let def = &b.def;
            match &mut b.values {
                Values::Bytes(v) => write::<ByteArrayType>(&mut column, v, def, None)?,
                Values::List(v, rep) => write::<ByteArrayType>(&mut column, v, def, Some(rep))?,
                Values::F64(v) => write::<DoubleType>(&mut column, v, def, None)?,
                Values::I64(v) => write::<Int64Type>(&mut column, v, def, None)?,
                Values::Bool(v) => write::<BoolType>(&mut column, v, def, None)?,
                Values::Date(v) => write::<Int32Type>(&mut column, v, def, None)?,
            }
            column.close()?;
            b.def.clear();
        }
        group.close()?;
        self.rows = 0;
        Ok(())
    }
}

/// Writes one column's values and levels, then clears the values and the repetition levels.
fn write<T: DataType>(
    column: &mut SerializedColumnWriter<'_>,
    values: &mut Vec<T::T>,
    def: &[i16],
    rep: Option<&mut Vec<i16>>,
) -> Result<(), Error> {
    column.typed::<T>().write_batch(values, Some(def), rep.as_deref().map(Vec::as_slice))?;
    values.clear();
    if let Some(rep) = rep {
        rep.clear();
    }
    Ok(())
}

impl Buffer {
    fn push(&mut self, value: Value) {
        let def = match (&mut self.values, value) {
            // Levels of `optional group (LIST) { repeated group list { optional element } }`:
            // 0 null list, 1 empty list, 3 an element; repetition 1 for every element but
            // a row's first.
            (Values::List(v, rep), Value::List(items)) => {
                if items.is_empty() {
                    self.def.push(1);
                    rep.push(0);
                }
                for (i, s) in items.into_iter().enumerate() {
                    v.push(s.into_bytes().into());
                    self.def.push(3);
                    rep.push(i16::from(i > 0));
                }
                return;
            }
            (Values::List(_, rep), _) => {
                rep.push(0);
                0
            }
            (Values::Bytes(v), Value::Text(s)) => {
                v.push(s.into_bytes().into());
                1
            }
            (Values::F64(v), Value::F64(x)) => {
                v.push(x);
                1
            }
            (Values::I64(v), Value::I64(x)) => {
                v.push(x);
                1
            }
            (Values::Bool(v), Value::Bool(x)) => {
                v.push(x);
                1
            }
            (Values::Date(v), Value::Date(x)) => {
                v.push(x);
                1
            }
            // Null, or a value of another type, which the cast does not give.
            _ => 0,
        };
        self.def.push(def);
    }
}
