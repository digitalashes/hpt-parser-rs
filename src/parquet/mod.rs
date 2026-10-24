//! Parquet output: each record a row of the config's columns, in one file or in chunks of
//! files, the file's [`Metadata`] as JSON in every footer under `hpt.metadata`.
//!
//! Column types: `string` is `BYTE_ARRAY (STRING)`, `json` `BYTE_ARRAY (JSON)`, `f64`
//! `DOUBLE`, `i64` `INT64`, `bool` `BOOLEAN`, `date` `INT32 (DATE)`, `list` a `LIST` of
//! strings. Every column is optional.

mod group;

use crate::config::{self, Call, Column, Config, Type, plugin_error};
use crate::csv;
use crate::json::{self, Spare, Value};
use crate::{Error, Item, Metadata, Parser, Record};
use ::parquet::basic::{
    BrotliLevel, Compression as Codec, ConvertedType, GzipLevel, LogicalType, Repetition,
    Type as Physical, ZstdLevel,
};
use ::parquet::file::metadata::KeyValue;
use ::parquet::file::properties::WriterProperties;
use ::parquet::file::writer::SerializedFileWriter;
use ::parquet::schema::types::{SchemaDescriptor, Type as Schema, TypePtr};
use group::{Batch, Group};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::{panic, thread};

/// Records read at a time: a batch goes to the thread that makes rows, where a plugin
/// function gets it in one call. A batch stays in the cache: 8192 took a 2.5 GB file with
/// plugins from 3.7 to 4.6 s and its peak memory from 39 to 57 MB (47 to 175 MB with every
/// field in the plugin JSON).
const BATCH: usize = 512;

/// A batch's JSON charge items with their positions.
type Items = Vec<(Value, u64)>;

/// The footer key that holds the file's metadata.
pub const METADATA_KEY: &str = "hpt.metadata";

/// The codec of every column, from [`parse_compression`]. It keeps the `parquet` crate's
/// own type, which changes with each of its major versions, out of this API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compression(Codec);

/// Where and how to write.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Output {
    /// The file, or the name the chunk files are made from.
    pub path: PathBuf,
    /// The codec of every column.
    pub compression: Compression,
    /// Rows per row group, 0 for one row group per file.
    pub row_group_size: usize,
    /// Records per file, 0 for one file at `path`. Otherwise the files are
    /// `<stem>-00001.<ext>`, `<stem>-00002.<ext>`, ... next to `path`.
    pub chunk_size: u64,
}

impl Output {
    /// `zstd:3`, row groups of 256000 rows, one file.
    #[must_use]
    #[expect(clippy::missing_panics_doc, reason = "3 is a zstd level")]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Output {
            path: path.into(),
            compression: Compression(Codec::ZSTD(
                ZstdLevel::try_new(3).expect("3 is a zstd level"),
            )),
            row_group_size: 256_000,
            chunk_size: 0,
        }
    }

    /// Takes what the config's `[output]` sets.
    ///
    /// # Errors
    ///
    /// A compression [`parse_compression`] rejects.
    pub fn apply(&mut self, config: &config::Output) -> Result<(), Error> {
        if let Some(c) = &config.compression {
            self.compression = parse_compression(c)?;
        }
        if let Some(n) = config.row_group_size {
            self.row_group_size = n;
        }
        if let Some(n) = config.chunk_size {
            self.chunk_size = n;
        }
        Ok(())
    }
}

/// `none`, `snappy`, `gzip[:level]`, `lz4`, `zstd[:level]` or `brotli[:level]`. Without a
/// level gzip takes 6, zstd and brotli 1; `lz4` is Parquet's `LZ4_RAW`.
///
/// # Errors
///
/// Another name, a level for a codec without levels, or a level out of the codec's range.
pub fn parse_compression(s: &str) -> Result<Compression, Error> {
    let error =
        |message: String| Error::Config { message: format!("compression `{s}`: {message}") };
    let (name, level) = match s.split_once(':') {
        Some((name, level)) => {
            let level =
                level.parse::<i32>().map_err(|_| error("the level is not a number".into()))?;
            (name, Some(level))
        }
        None => (s, None),
    };
    let unsigned = |l: i32| u32::try_from(l).map_err(|_| error("the level is negative".into()));
    let level_error = |e: ::parquet::errors::ParquetError| error(e.to_string());
    Ok(Compression(match (name, level) {
        ("none", None) => Codec::UNCOMPRESSED,
        ("snappy", None) => Codec::SNAPPY,
        ("lz4", None) => Codec::LZ4_RAW,
        ("none" | "snappy" | "lz4", Some(_)) => return Err(error("takes no level".into())),
        ("gzip", None) => Codec::GZIP(GzipLevel::default()),
        ("gzip", Some(l)) => Codec::GZIP(GzipLevel::try_new(unsigned(l)?).map_err(level_error)?),
        ("zstd", None) => Codec::ZSTD(ZstdLevel::default()),
        ("zstd", Some(l)) => Codec::ZSTD(ZstdLevel::try_new(l).map_err(level_error)?),
        ("brotli", None) => Codec::BROTLI(BrotliLevel::default()),
        ("brotli", Some(l)) => {
            Codec::BROTLI(BrotliLevel::try_new(unsigned(l)?).map_err(level_error)?)
        }
        _ => {
            return Err(error(
                "expected none, snappy, gzip[:level], lz4, zstd[:level] or brotli[:level]".into(),
            ));
        }
    }))
}

/// A file once it is closed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Chunk {
    /// Where it was written.
    pub path: PathBuf,
    /// 1-based.
    pub index: u32,
    /// Rows in it.
    pub records: u64,
}

/// What [`write()`] wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Summary {
    /// Files written.
    pub chunks: u32,
    /// Records written, over all files.
    pub records: u64,
    /// A JSON file's metadata came after records: rows written before it have nulls (or
    /// stale values) in the columns that read `metadata.*`. The footers have what was known
    /// when each file closed; the last one is complete.
    pub late_metadata: bool,
}

/// A file being written.
struct Open {
    chunk: Chunk,
    writer: SerializedFileWriter<File>,
}

/// Writes each record as a row of `config`'s columns. With a `chunk_size` a file is closed
/// once it has that many records and the next record has come (so the last file closes
/// after the whole input is read), then `on_chunk` gets it. No records at all still give one
/// file with zero rows.
///
/// Records are read in batches of up to 512 (fewer where the metadata changes or the next
/// JSON charge item's records do not fit, more for an item with more), and other threads
/// make the rows of each batch, a share of the columns each, while the next one is read.
/// With a `[[filter]]` or a column with plugins, the filters drop records from a batch first
/// (dropped records count for nothing), then each plugin function is called once for the
/// batch. The `[[metadata]]` plugins run once after the last record, and the fields they add
/// are in the last file's footer.
///
/// # Errors
///
/// The parser's error, a plugin's, IO or Parquet errors. Files closed so far stay; the open
/// one is left without a footer.
#[expect(clippy::missing_panics_doc, reason = "the metadata always serializes")]
pub fn write<R: Read>(
    mut parser: Parser<R>,
    config: &Config,
    output: &Output,
    mut on_chunk: impl FnMut(&Chunk),
) -> Result<Summary, Error> {
    let schema = schema(&config.columns)?;
    let properties =
        Arc::new(WriterProperties::builder().set_compression(output.compression.0).build());
    let descr = SchemaDescriptor::new(schema.clone());
    let group = Group::new(&config.columns, descr.columns(), &properties)?;
    let mut files = Files {
        output,
        schema,
        properties,
        group,
        file: None,
        summary: Summary::default(),
        closed: Vec::new(),
    };
    #[cfg(feature = "plugins")]
    let mut plugins = crate::plugin::Plugins::load(config)?;
    #[cfg(feature = "plugins")]
    let call: &mut Call = &mut |function, input| plugins.call(function, input);
    #[cfg(not(feature = "plugins"))]
    if !config.plugins.is_empty() {
        let message = "the config has plugins, this build has no feature `plugins`".into();
        return Err(Error::Config { message });
    }
    // A config without plugins names no plugin function.
    #[cfg(not(feature = "plugins"))]
    let call: &mut Call = &mut |_, _| unreachable!("no plugins");
    let mut first = None;
    // Without a filter or a column with plugins (`[[metadata]]` alone runs at the end).
    if config.filters.is_empty() && !config.columns.iter().any(Column::has_plugins) {
        rows(&mut parser, &mut files, &mut first, &mut on_chunk, |files, records, metadata| {
            files.rows(Batch { records, metadata, plugin: Vec::new() })
        })?;
    } else {
        rows(&mut parser, &mut files, &mut first, &mut on_chunk, |files, mut batch, metadata| {
            config.filter(&mut batch, &metadata, call)?;
            // Columns with plugins for the whole batch, the others record by record on the
            // workers: every column a batch at a time cost nyp 6% (1.73 s became 1.83 s).
            let plugin = (config.columns.iter())
                .map(|c| c.has_plugins().then(|| c.values(&batch, &metadata, call)).transpose())
                .collect::<Result<_, _>>()?;
            files.rows(Batch { records: batch, metadata, plugin })
        })?;
    }
    let mut metadata = Cow::Borrowed(parser.metadata());
    for function in &config.metadata {
        #[derive(serde::Deserialize)]
        struct Answer {
            fields: serde_json::Map<String, serde_json::Value>,
        }
        let input = serde_json::to_vec(&*metadata).expect("strings, lists and maps of them");
        let Ok(Answer { fields }) = serde_json::from_str(call(function, &input)?.get()) else {
            return Err(plugin_error(function, "the answer has no `fields` object".into()));
        };
        for (key, value) in fields {
            let value = value.as_str().map_or_else(|| value.to_string(), String::from);
            metadata.to_mut().extra.insert(key, value);
        }
    }
    let mut summary = files.finish(&metadata)?;
    for c in files.closed.drain(..) {
        on_chunk(&c);
    }
    if let Some((record, seen)) = first {
        summary.late_metadata =
            (config.columns.iter()).any(|c| c.value(&record, &seen) != c.value(&record, &metadata));
    }
    Ok(summary)
}

/// Reads the records in batches while a thread of its own makes the records of the batch
/// before (half to two thirds of the reading for CSV, a third for JSON) and another hands
/// them to `make`, which writes their rows. CSV rows' records are cut into batches of 512;
/// a JSON batch is of charge items, whose values come back here to be reused. A batch ends
/// early where the metadata changes, so that each row has the metadata its record was read
/// with. The files the writing thread closes come back here for `on_chunk`. A parser
/// error still has the records before it written first.
fn rows<'o, R: Read>(
    parser: &mut Parser<R>,
    files: &mut Files<'o>,
    first: &mut Option<(Record, Metadata)>,
    on_chunk: &mut impl FnMut(&Chunk),
    mut make: impl FnMut(&mut Files<'o>, Vec<Record>, Arc<Metadata>) -> Result<(), Error> + Send,
) -> Result<(), Error> {
    let (to_records, read) = mpsc::sync_channel::<(Vec<csv::Rows>, Items, Arc<Metadata>)>(1);
    let (to_rows, batches) = mpsc::sync_channel::<(Vec<Record>, Arc<Metadata>)>(1);
    let (to_caller, closed) = mpsc::channel();
    let (to_reuse, done) = mpsc::channel::<Result<Spare, Items>>();
    thread::scope(|s| {
        s.spawn(move || records(&read, &to_rows, &to_reuse));
        let writer = s.spawn(move || {
            for (records, metadata) in batches {
                if first.is_none()
                    && let Some(record) = records.first()
                {
                    *first = Some((record.clone(), Metadata::clone(&metadata)));
                }
                let written = make(files, records, metadata);
                // The caller is gone only when it panicked.
                for c in files.closed.drain(..) {
                    _ = to_caller.send(c);
                }
                written?;
            }
            Ok(())
        });
        let (mut batch, mut items) = (Vec::new(), Vec::new());
        // The records in `items`, CSV rows in `batch` counting as a whole batch. A JSON item's
        // records stay in one batch, the next one if they do not fit.
        let mut size = 0;
        // The metadata the records in `batch` were read with, and the parser's count of
        // changes to it then.
        let mut seen: Option<(u64, Arc<Metadata>)> = None;
        let read = loop {
            let next = parser.next_item();
            let records = match &next {
                Some(Ok(Item::Json(value, _))) => json::records(value),
                _ => BATCH,
            };
            let changes = parser.metadata_changes();
            let stale = seen.as_ref().is_some_and(|(n, _)| *n != changes);
            // Once `seen` is set, a batch has a record: each round adds one or ends.
            if let Some((_, metadata)) = &seen
                && (stale || size + records > BATCH || !matches!(next, Some(Ok(_))))
            {
                let full =
                    (std::mem::take(&mut batch), std::mem::take(&mut items), metadata.clone());
                size = 0;
                // Fails only when the writer stopped, on an error it returns below.
                if to_records.send(full).is_err() {
                    break Ok(());
                }
                for c in closed.try_iter() {
                    on_chunk(&c);
                }
                for back in done.try_iter() {
                    match back {
                        Ok(spare) => parser.reuse(spare),
                        Err(items) => {
                            for (value, _) in items {
                                parser.recycle(value);
                            }
                        }
                    }
                }
            }
            if seen.is_none() || stale {
                seen = Some((changes, Arc::new(parser.metadata().clone())));
            }
            match next {
                Some(Ok(Item::Rows(rows))) => {
                    batch.push(rows);
                    size += records;
                }
                Some(Ok(Item::Json(value, position))) => {
                    items.push((value, position));
                    size += records;
                }
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            }
        };
        drop(to_records);
        let written = writer.join().unwrap_or_else(|p| panic::resume_unwind(p));
        for c in closed.try_iter() {
            on_chunk(&c);
        }
        written.and(read)
    })
}

/// Makes the records of each batch read and sends them on: a JSON batch's together, its
/// values back to the caller, taken apart or not; CSV rows' cut into batches of `BATCH`.
fn records(
    read: &mpsc::Receiver<(Vec<csv::Rows>, Items, Arc<Metadata>)>,
    to_rows: &mpsc::SyncSender<(Vec<Record>, Arc<Metadata>)>,
    to_reuse: &mpsc::Sender<Result<Spare, Items>>,
) {
    // CSV records not sent yet, and the metadata they were read with.
    let (mut rest, mut seen) = (VecDeque::new(), None);
    // A batch read while the one before was made.
    let mut waiting = None;
    // Each send fails only when the writer stopped.
    while let Some((rows, items, metadata)) = waiting.take().or_else(|| read.recv().ok()) {
        if items.is_empty() {
            for r in &rows {
                r.records(&mut rest);
            }
            while rest.len() >= BATCH {
                if to_rows.send((rest.drain(..BATCH).collect(), Arc::clone(&metadata))).is_err() {
                    return;
                }
            }
            seen = Some(metadata);
            continue;
        }
        let mut pending = VecDeque::with_capacity(BATCH);
        for (value, position) in &items {
            json::item(value, *position, &mut pending);
        }
        // The values are taken apart here, while still in this thread's cache, unless the next
        // batch is read already: then this thread is the slower one (one record per item, say)
        // and the caller takes them apart.
        let back = if let Ok(batch) = read.try_recv() {
            waiting = Some(batch);
            Err(items)
        } else {
            let mut spare = Spare::default();
            for (value, _) in items {
                spare.keep(value);
            }
            Ok(spare)
        };
        _ = to_reuse.send(back);
        if to_rows.send((pending.into(), metadata)).is_err() {
            return;
        }
    }
    if let Some(metadata) = seen
        && !rest.is_empty()
    {
        _ = to_rows.send((rest.into(), metadata));
    }
}

/// The files closed so far and the one being written.
struct Files<'o> {
    output: &'o Output,
    schema: TypePtr,
    properties: Arc<WriterProperties>,
    group: Group,
    file: Option<Open>,
    summary: Summary,
    /// Files closed since the caller last took them.
    closed: Vec<Chunk>,
}

impl Files<'_> {
    /// Adds a batch's records as rows, first closing the open file where it is full, and
    /// writes each row group once it is full. The batch's metadata goes to a closed file's
    /// footer.
    fn rows(&mut self, batch: Batch) -> Result<(), Error> {
        let (chunk_size, group_size) = (self.output.chunk_size, self.output.row_group_size);
        let batch = Arc::new(batch);
        let mut start = 0;
        while start < batch.records.len() {
            if let Some(f) = self.file.take_if(|f| chunk_size > 0 && f.chunk.records == chunk_size)
            {
                self.close(f, &batch.metadata)?;
            }
            let f = match self.file.take() {
                Some(f) => f,
                None => self.open()?,
            };
            let f = self.file.insert(f);
            // Up to where the file or the row group is full.
            let mut end = batch.records.len();
            if chunk_size > 0 {
                let room = usize::try_from(chunk_size - f.chunk.records).unwrap_or(usize::MAX);
                end = end.min(start.saturating_add(room));
            }
            if group_size > 0 {
                end = end.min(start + group_size - self.group.len());
            }
            self.group.push(&batch, start..end);
            f.chunk.records += (end - start) as u64;
            if self.group.len() == group_size {
                self.group.flush(&mut f.writer)?;
            }
            start = end;
        }
        self.group.free(batch);
        Ok(())
    }

    fn open(&self) -> Result<Open, Error> {
        let index = self.summary.chunks + 1;
        let path = chunk_path(&self.output.path, self.output.chunk_size, index);
        let file = File::create(&path)?;
        let writer = SerializedFileWriter::new(file, self.schema.clone(), self.properties.clone())?;
        Ok(Open { chunk: Chunk { path, index, records: 0 }, writer })
    }

    fn close(&mut self, mut f: Open, metadata: &Metadata) -> Result<(), Error> {
        self.group.flush(&mut f.writer)?;
        f.writer.append_key_value_metadata(KeyValue::new(METADATA_KEY.into(), footer(metadata)));
        f.writer.close()?;
        self.summary.chunks += 1;
        self.summary.records += f.chunk.records;
        self.closed.push(f.chunk);
        Ok(())
    }

    /// Closes the open file, or writes one with no rows when there was no record.
    fn finish(&mut self, metadata: &Metadata) -> Result<Summary, Error> {
        let f = match self.file.take() {
            Some(f) => f,
            None => self.open()?,
        };
        self.close(f, metadata)?;
        Ok(self.summary)
    }
}

fn chunk_path(path: &Path, chunk_size: u64, index: u32) -> PathBuf {
    if chunk_size == 0 {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(ext) => format!("{stem}-{index:05}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{index:05}"),
    };
    path.with_file_name(name)
}

fn schema(columns: &[Column]) -> Result<TypePtr, Error> {
    let fields = (columns.iter()).map(|c| field(c).map(Arc::new)).collect::<Result<_, _>>()?;
    Ok(Arc::new(Schema::group_type_builder("hpt").with_fields(fields).build()?))
}

fn field(c: &Column) -> ::parquet::errors::Result<Schema> {
    let primitive = |name, physical, logical| {
        Schema::primitive_type_builder(name, physical)
            .with_repetition(Repetition::OPTIONAL)
            .with_logical_type(logical)
            .build()
    };
    let name = c.name.as_str();
    match c.kind {
        Type::String => primitive(name, Physical::BYTE_ARRAY, Some(LogicalType::String)),
        Type::Json => primitive(name, Physical::BYTE_ARRAY, Some(LogicalType::Json)),
        Type::F64 => primitive(name, Physical::DOUBLE, None),
        Type::I64 => primitive(name, Physical::INT64, None),
        Type::Bool => primitive(name, Physical::BOOLEAN, None),
        Type::Date => primitive(name, Physical::INT32, Some(LogicalType::Date)),
        Type::List => {
            let element = primitive("element", Physical::BYTE_ARRAY, Some(LogicalType::String))?;
            let list = Schema::group_type_builder("list")
                .with_repetition(Repetition::REPEATED)
                .with_fields(vec![Arc::new(element)])
                .build()?;
            Schema::group_type_builder(name)
                .with_repetition(Repetition::OPTIONAL)
                .with_converted_type(ConvertedType::LIST)
                .with_logical_type(Some(LogicalType::List))
                .with_fields(vec![Arc::new(list)])
                .build()
        }
    }
}

/// The metadata as JSON, named as the `metadata.*` fields.
fn footer(m: &Metadata) -> String {
    serde_json::to_string(m).expect("strings, lists and maps of them always serialize")
}
