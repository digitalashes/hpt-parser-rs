//! The row group being built: the only code that touches column writers.

use crate::config::{Column, Type, Value};
use crate::{Error, Metadata, Record};
use ::parquet::column::page::{CompressedPage, PageWriteSpec, PageWriter};
use ::parquet::column::writer::{
    ColumnCloseResult, ColumnWriter, get_column_writer, get_typed_column_writer_mut,
};
use ::parquet::data_type::{
    BoolType, ByteArray, ByteArrayType, DataType, DoubleType, Int32Type, Int64Type,
};
use ::parquet::errors::ParquetError;
use ::parquet::file::properties::WriterPropertiesPtr;
use ::parquet::file::writer::{SerializedFileWriter, SerializedPageWriter, TrackedWrite};
use ::parquet::schema::types::ColumnDescPtr;
use bytes::Bytes;
use std::borrow::Cow;
use std::io::{self, Write};
use std::num::NonZero;
use std::ops::Range;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

/// Column workers at most.
const WORKERS: usize = 6;

/// Records with what their rows need: the metadata they were read with and the values of
/// the columns with plugins, made for the whole batch.
pub(super) struct Batch {
    pub(super) records: Vec<Record>,
    pub(super) metadata: Arc<Metadata>,
    /// Each column's values when it has plugins, by column.
    pub(super) plugin: Vec<Option<Vec<Value<'static>>>>,
}

/// The row group being built. The columns are split over worker threads: each makes its
/// columns' values for every row, then encodes and compresses them into pages kept in
/// memory; at [`Group::flush`] the pages go to the file as one row group.
pub(super) struct Group {
    workers: Vec<Worker>,
    /// To the thread that drops each batch once every worker is done with it.
    free: SyncSender<Arc<Batch>>,
    rows: usize,
}

/// A worker thread's channels.
struct Worker {
    jobs: SyncSender<Job>,
    done: Receiver<Done>,
}

enum Job {
    /// Make and encode the rows of these records.
    Rows(Arc<Batch>, Range<usize>),
    /// Close the column chunks and start new ones.
    Close,
}

/// Each of the worker's column chunks, by the column's place in a row, with its pages and
/// metadata, or the first error since the last close.
type Done = Result<Vec<(usize, Bytes, ColumnCloseResult)>, ParquetError>;

const WORKER: &str = "a worker lives as long as its group and does not panic";

/// A column chunk on its worker: the column, its values waiting to be encoded, and the
/// writer that encodes them into its pages.
struct Chunk {
    /// The column's place in a row.
    index: usize,
    column: Column,
    /// [`Column::per_charge`].
    per_charge: bool,
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
    /// The values and the last one pushed.
    Bytes(Vec<ByteArray>, Option<ByteArray>),
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
    /// `descrs`: the schema's leaf columns, one per column. Starts the workers and the
    /// thread that drops the batches.
    pub(super) fn new(
        columns: &[Column],
        descrs: &[ColumnDescPtr],
        properties: &WriterPropertiesPtr,
    ) -> Result<Self, Error> {
        // The cores left by the caller, which parses, and the thread that drops the batches
        // (the one that hands them out mostly waits); two at least, or one thread would make
        // the values and encode them one after the other.
        let cores = thread::available_parallelism().map_or(1, NonZero::get).saturating_sub(2);
        let k = cores.clamp(2, WORKERS).min(columns.len()).max(1);
        let (free, freed) = mpsc::sync_channel(2 * k);
        thread::Builder::new()
            .name("batch drop".into())
            .spawn(move || freed.into_iter().for_each(drop))?;
        // Heaviest first, each column to the worker with the least weight so far. On a wide
        // file a column made once per charge took half as long as one made for every record,
        // and a `json` column (the codes) as long as four others made the same way.
        // ponytail: weights by kind, measure a column's cost if another one lags.
        let weight = |c: &Column| match (c.per_charge(), c.kind) {
            (true, Type::Json) => 2,
            (true, _) => 1,
            (false, Type::Json) => 8,
            (false, _) => 2,
        };
        let mut order: Vec<_> = columns.iter().zip(descrs).enumerate().collect();
        order.sort_by_key(|(_, (c, _))| std::cmp::Reverse(weight(c)));
        let mut shares: Vec<(u32, Vec<Chunk>)> = (0..k).map(|_| (0, Vec::new())).collect();
        for (index, (column, descr)) in order {
            let pages = Pages::default();
            let writer = column_writer(&pages, descr, properties);
            let buffer = Buffer::new(column.kind);
            let share = shares.iter_mut().min_by_key(|(w, _)| *w).expect("one worker at least");
            share.0 += weight(column);
            let (column, per_charge) = (column.clone(), column.per_charge());
            let descr = descr.clone();
            share.1.push(Chunk { index, column, per_charge, buffer, descr, pages, writer });
        }
        let workers = (shares.into_iter())
            .map(|(_, chunks)| {
                let (jobs, job) = mpsc::sync_channel(2);
                let (finished, done) = mpsc::channel();
                let (properties, free) = (properties.clone(), free.clone());
                thread::Builder::new()
                    .name("parquet columns".into())
                    .spawn(move || worker(chunks, &properties, &job, &finished, &free))?;
                Ok(Worker { jobs, done })
            })
            .collect::<io::Result<_>>()?;
        Ok(Group { workers, free, rows: 0 })
    }

    pub(super) fn len(&self) -> usize {
        self.rows
    }

    /// The rows of `batch.records[rows]`, made and encoded on the workers.
    pub(super) fn push(&mut self, batch: &Arc<Batch>, rows: Range<usize>) {
        self.rows += rows.len();
        for w in &self.workers {
            w.jobs.send(Job::Rows(batch.clone(), rows.clone())).expect(WORKER);
        }
    }

    /// Hands a batch to the thread that drops it once the workers are done with it.
    pub(super) fn free(&self, batch: Arc<Batch>) {
        // The dropping thread ends only after the group.
        _ = self.free.send(batch);
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
        for w in &self.workers {
            w.jobs.send(Job::Close).expect(WORKER);
        }
        let mut closed = Vec::new();
        for w in &self.workers {
            closed.extend(w.done.recv().expect(WORKER)?);
        }
        closed.sort_unstable_by_key(|(index, ..)| *index);
        let mut group = file.next_row_group()?;
        for (_, pages, close) in closed {
            group.append_column(&pages, close)?;
        }
        group.close()?;
        self.rows = 0;
        Ok(())
    }
}

/// A worker thread: makes its columns' values for each job's rows and encodes them every
/// `write_batch_size` rows; at `Close` it closes its column chunks and starts new ones. It
/// ends when the group is dropped.
fn worker(
    mut chunks: Vec<Chunk>,
    properties: &WriterPropertiesPtr,
    jobs: &Receiver<Job>,
    done: &Sender<Done>,
    free: &SyncSender<Arc<Batch>>,
) {
    let batch_size = properties.write_batch_size();
    let mut buffered = 0;
    // The first error since the last close; the rows after it are skipped.
    let mut failed = None;
    for job in jobs {
        match job {
            Job::Rows(batch, rows) => {
                for r in rows {
                    if failed.is_some() {
                        break;
                    }
                    let record = &batch.records[r];
                    // The row before, still in the buffers, has the same charge: a wide row's
                    // payers, or a JSON standard charge's.
                    let again = r > 0
                        && buffered > 0
                        && Arc::ptr_eq(&batch.records[r - 1].charge, &record.charge);
                    for c in &mut chunks {
                        match batch.plugin.get(c.index) {
                            // Lent as a record's text is.
                            Some(Some(values)) => c.buffer.push(match &values[r] {
                                Value::Text(s) => Value::Text(Cow::Borrowed(s)),
                                v => v.clone(),
                            }),
                            _ if again && c.per_charge => c.buffer.repeat(),
                            _ => c.buffer.push(c.column.value(record, &batch.metadata)),
                        }
                    }
                    buffered += 1;
                    if buffered == batch_size {
                        buffered = 0;
                        failed = encode(&mut chunks).err();
                    }
                }
                // The dropping thread ends only after the workers.
                _ = free.send(batch);
            }
            Job::Close => {
                buffered = 0;
                let closed = match failed.take() {
                    Some(e) => Err(e),
                    None => close(&mut chunks, properties),
                };
                if done.send(closed).is_err() {
                    return;
                }
            }
        }
    }
}

/// Encodes each chunk's buffered values and levels and empties its buffer.
fn encode(chunks: &mut [Chunk]) -> Result<(), ParquetError> {
    for c in chunks {
        let def = &c.buffer.def;
        let w = &mut c.writer;
        match &mut c.buffer.values {
            Values::Bytes(v, _) => write::<ByteArrayType>(w, v, def, None)?,
            Values::List(v, rep) => write::<ByteArrayType>(w, v, def, Some(rep))?,
            Values::F64(v) => write::<DoubleType>(w, v, def, None)?,
            Values::I64(v) => write::<Int64Type>(w, v, def, None)?,
            Values::Bool(v) => write::<BoolType>(w, v, def, None)?,
            Values::Date(v) => write::<Int32Type>(w, v, def, None)?,
        }
        c.buffer.def.clear();
    }
    Ok(())
}

/// Encodes what is left, then closes each column chunk and starts a new one.
fn close(chunks: &mut [Chunk], properties: &WriterPropertiesPtr) -> Done {
    encode(chunks)?;
    (chunks.iter_mut())
        .map(|c| {
            let writer = column_writer(&c.pages, &c.descr, properties);
            let close = std::mem::replace(&mut c.writer, writer).close()?;
            let pages =
                std::mem::take(&mut *c.pages.0.lock().unwrap_or_else(PoisonError::into_inner));
            Ok((c.index, Bytes::from(pages), close))
        })
        .collect()
}

/// Encodes one column's values and levels, then clears the values and the repetition levels.
fn write<T: DataType>(
    writer: &mut ColumnWriter<'static>,
    values: &mut Vec<T::T>,
    def: &[i16],
    rep: Option<&mut Vec<i16>>,
) -> Result<(), ParquetError> {
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
    fn new(kind: Type) -> Self {
        let values = match kind {
            Type::String | Type::Json => Values::Bytes(Vec::new(), None),
            Type::F64 => Values::F64(Vec::new()),
            Type::I64 => Values::I64(Vec::new()),
            Type::Bool => Values::Bool(Vec::new()),
            Type::Date => Values::Date(Vec::new()),
            Type::List => Values::List(Vec::new(), Vec::new()),
        };
        Buffer { values, def: Vec::new() }
    }

    fn push(&mut self, value: Value<'_>) {
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
            (Values::Bytes(v, last), Value::Text(s)) => {
                // A value like the one before (a charge's fields over its payers, the
                // file's metadata on every row) shares its bytes instead of a copy.
                if last.as_ref().is_none_or(|l| l.data() != s.as_bytes()) {
                    *last = Some(s.into_owned().into_bytes().into());
                }
                v.extend(last.clone());
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

    /// Pushes the last row again: its levels, and its value unless it is null, or a list's
    /// elements from where its repetition level is 0.
    fn repeat(&mut self) {
        const ROW: &str = "a row to repeat is buffered";
        let (from, values) = match &self.values {
            Values::List(_, rep) => {
                let from = rep.iter().rposition(|&r| r == 0).expect(ROW);
                (from, self.def[from..].iter().filter(|&&d| d == 3).count())
            }
            _ => (
                self.def.len().checked_sub(1).expect(ROW),
                usize::from(self.def.last() == Some(&1)),
            ),
        };
        self.def.extend_from_within(from..);
        match &mut self.values {
            Values::Bytes(v, _) => again(v, values),
            Values::List(v, rep) => {
                again(v, values);
                rep.extend_from_within(from..);
            }
            Values::F64(v) => again(v, values),
            Values::I64(v) => again(v, values),
            Values::Bool(v) => again(v, values),
            Values::Date(v) => again(v, values),
        }
    }
}

/// Pushes the last `n` values again.
fn again<T: Clone>(v: &mut Vec<T>, n: usize) {
    v.extend_from_within(v.len() - n..);
}
