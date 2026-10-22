//! The row group being built: the only code that touches column writers.

use crate::Error;
use crate::config::{Column, Type, Value};
use ::parquet::column::page::{CompressedPage, PageWriteSpec, PageWriter};
use ::parquet::column::writer::{ColumnWriter, get_column_writer, get_typed_column_writer_mut};
use ::parquet::data_type::{
    BoolType, ByteArray, ByteArrayType, DataType, DoubleType, Int32Type, Int64Type,
};
use ::parquet::file::properties::WriterPropertiesPtr;
use ::parquet::file::writer::{SerializedFileWriter, SerializedPageWriter, TrackedWrite};
use ::parquet::schema::types::ColumnDescPtr;
use bytes::Bytes;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};

/// The row group being built. Rows are buffered per column for a batch of
/// `write_batch_size` rows, then encoded and compressed into pages kept in memory; at
/// [`Group::flush`] the pages go to the file as one row group.
pub(super) struct Group {
    columns: Vec<Chunk>,
    properties: WriterPropertiesPtr,
    rows: usize,
    batch: usize,
}

/// A column chunk: the batch not yet encoded, the writer that encodes it and its pages.
struct Chunk {
    buffer: Buffer,
    descr: ColumnDescPtr,
    pages: Pages,
    writer: ColumnWriter<'static>,
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

/// A column chunk's encoded pages, shared with the page writer inside its column writer
/// (which gives back nothing but metadata when it closes).
#[derive(Clone, Default)]
struct Pages(Arc<Mutex<Vec<u8>>>);

impl Write for Pages {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Lays pages out as the file writer does, at offsets from the chunk's start.
struct PageSink(TrackedWrite<Pages>);

impl PageWriter for PageSink {
    fn write_page(&mut self, page: CompressedPage) -> ::parquet::errors::Result<PageWriteSpec> {
        SerializedPageWriter::new(&mut self.0).write_page(page)
    }

    fn close(&mut self) -> ::parquet::errors::Result<()> {
        Ok(self.0.flush()?)
    }
}

/// A writer whose pages go to `pages`, at offsets counted from its own first page.
fn column_writer(
    pages: &Pages,
    descr: &ColumnDescPtr,
    properties: &WriterPropertiesPtr,
) -> ColumnWriter<'static> {
    let sink = Box::new(PageSink(TrackedWrite::new(pages.clone())));
    get_column_writer(descr.clone(), properties.clone(), sink)
}

impl Group {
    /// `descrs`: the schema's leaf columns, one per column.
    pub(super) fn new(
        columns: &[Column],
        descrs: &[ColumnDescPtr],
        properties: &WriterPropertiesPtr,
    ) -> Self {
        let columns = (columns.iter().zip(descrs))
            .map(|(c, descr)| {
                let pages = Pages::default();
                let writer = column_writer(&pages, descr, properties);
                let values = match c.kind {
                    Type::String | Type::Json => Values::Bytes(Vec::new()),
                    Type::F64 => Values::F64(Vec::new()),
                    Type::I64 => Values::I64(Vec::new()),
                    Type::Bool => Values::Bool(Vec::new()),
                    Type::Date => Values::Date(Vec::new()),
                    Type::List => Values::List(Vec::new(), Vec::new()),
                };
                Chunk {
                    buffer: Buffer { values, def: Vec::new() },
                    descr: descr.clone(),
                    pages,
                    writer,
                }
            })
            .collect();
        Group { columns, properties: properties.clone(), rows: 0, batch: 0 }
    }

    pub(super) fn len(&self) -> usize {
        self.rows
    }

    /// One row: a value per column, in the columns' order.
    pub(super) fn push(&mut self, row: impl Iterator<Item = Value>) -> Result<(), Error> {
        for (chunk, value) in self.columns.iter_mut().zip(row) {
            chunk.buffer.push(value);
        }
        self.rows += 1;
        self.batch += 1;
        if self.batch == self.properties.write_batch_size() {
            self.encode()?;
        }
        Ok(())
    }

    /// Encodes the buffered batch and clears the buffers, keeping their capacity.
    fn encode(&mut self) -> Result<(), Error> {
        for c in &mut self.columns {
            let def = &c.buffer.def;
            let w = &mut c.writer;
            match &mut c.buffer.values {
                Values::Bytes(v) => write::<ByteArrayType>(w, v, def, None)?,
                Values::List(v, rep) => write::<ByteArrayType>(w, v, def, Some(rep))?,
                Values::F64(v) => write::<DoubleType>(w, v, def, None)?,
                Values::I64(v) => write::<Int64Type>(w, v, def, None)?,
                Values::Bool(v) => write::<BoolType>(w, v, def, None)?,
                Values::Date(v) => write::<Int32Type>(w, v, def, None)?,
            }
            c.buffer.def.clear();
        }
        self.batch = 0;
        Ok(())
    }

    /// Writes the rows as a row group and starts the next one. Nothing is written when
    /// there are no rows.
    pub(super) fn flush<W: Write + Send>(
        &mut self,
        file: &mut SerializedFileWriter<W>,
    ) -> Result<(), Error> {
        if self.rows == 0 {
            return Ok(());
        }
        self.encode()?;
        let mut group = file.next_row_group()?;
        for c in &mut self.columns {
            let writer = column_writer(&c.pages, &c.descr, &self.properties);
            let close = std::mem::replace(&mut c.writer, writer).close()?;
            let pages =
                std::mem::take(&mut *c.pages.0.lock().unwrap_or_else(PoisonError::into_inner));
            group.append_column(&Bytes::from(pages), close)?;
        }
        group.close()?;
        self.rows = 0;
        Ok(())
    }
}

/// Encodes one column's values and levels, then clears the values and the repetition levels.
fn write<T: DataType>(
    writer: &mut ColumnWriter<'static>,
    values: &mut Vec<T::T>,
    def: &[i16],
    rep: Option<&mut Vec<i16>>,
) -> Result<(), Error> {
    get_typed_column_writer_mut::<T>(writer).write_batch(
        values,
        Some(def),
        rep.as_deref().map(Vec::as_slice),
    )?;
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
