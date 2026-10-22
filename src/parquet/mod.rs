//! Parquet output: each record a row of the config's columns, in one file or in chunks of
//! files, the file's [`Metadata`] as JSON in every footer under `hpt.metadata`.
//!
//! Column types: `string` is `BYTE_ARRAY (STRING)`, `json` `BYTE_ARRAY (JSON)`, `f64`
//! `DOUBLE`, `i64` `INT64`, `bool` `BOOLEAN`, `date` `INT32 (DATE)`, `list` a `LIST` of
//! strings. Every column is optional.

mod group;

use crate::config::{self, Call, Column, Config, Type, Value, plugin_error};
use crate::{Error, Metadata, Parser};
use ::parquet::basic::{
    BrotliLevel, Compression, ConvertedType, GzipLevel, LogicalType, Repetition, Type as Physical,
    ZstdLevel,
};
use ::parquet::file::metadata::KeyValue;
use ::parquet::file::properties::WriterProperties;
use ::parquet::file::writer::SerializedFileWriter;
use ::parquet::schema::types::{SchemaDescriptor, Type as Schema, TypePtr};
use group::Group;
use std::borrow::Cow;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Records a plugin function gets in one call. A batch stays in the cache: 8192 took a
/// 2.5 GB file with plugins from 3.7 to 4.6 s and its peak memory from 39 to 57 MB (47 to
/// 175 MB with every field in the plugin JSON).
const BATCH: usize = 512;

/// The footer key that holds the file's metadata.
pub const METADATA_KEY: &str = "hpt.metadata";

/// Where and how to write.
#[derive(Clone, Debug)]
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
            compression: Compression::ZSTD(ZstdLevel::try_new(3).expect("3 is a zstd level")),
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
    Ok(match (name, level) {
        ("none", None) => Compression::UNCOMPRESSED,
        ("snappy", None) => Compression::SNAPPY,
        ("lz4", None) => Compression::LZ4_RAW,
        ("none" | "snappy" | "lz4", Some(_)) => return Err(error("takes no level".into())),
        ("gzip", None) => Compression::GZIP(GzipLevel::default()),
        ("gzip", Some(l)) => {
            Compression::GZIP(GzipLevel::try_new(unsigned(l)?).map_err(level_error)?)
        }
        ("zstd", None) => Compression::ZSTD(ZstdLevel::default()),
        ("zstd", Some(l)) => Compression::ZSTD(ZstdLevel::try_new(l).map_err(level_error)?),
        ("brotli", None) => Compression::BROTLI(BrotliLevel::default()),
        ("brotli", Some(l)) => {
            Compression::BROTLI(BrotliLevel::try_new(unsigned(l)?).map_err(level_error)?)
        }
        _ => {
            return Err(error(
                "expected none, snappy, gzip[:level], lz4, zstd[:level] or brotli[:level]".into(),
            ));
        }
    })
}

/// A file once it is closed.
#[derive(Clone, Debug, PartialEq, Eq)]
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
/// With a `[[filter]]` or a column with plugins, records are read in batches of 512: the
/// filters drop records from a batch (dropped records count for nothing), then each plugin
/// function is called once for the batch. The `[[metadata]]` plugins run once after the last
/// record, and the fields they add are in the last file's footer.
///
/// # Errors
///
/// The parser's error, a plugin's, IO or Parquet errors. Files closed so far stay; the open
/// one is left without a footer.
#[expect(clippy::missing_panics_doc, reason = "each column has a value per record")]
pub fn write<R: Read>(
    mut parser: Parser<R>,
    config: &Config,
    output: &Output,
    on_chunk: impl FnMut(&Chunk),
) -> Result<Summary, Error> {
    let schema = schema(&config.columns)?;
    let properties =
        Arc::new(WriterProperties::builder().set_compression(output.compression).build());
    let descr = SchemaDescriptor::new(schema.clone());
    let group = Group::new(&config.columns, descr.columns(), &properties);
    let mut files = Files {
        output,
        schema,
        properties,
        group,
        file: None,
        summary: Summary::default(),
        on_chunk,
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
    // Without a filter or a column with plugins (`[[metadata]]` alone runs at the end),
    // record by record: even a batch of one costs chi 6% (8.2 s, 8.7 s).
    if config.filters.is_empty() && !config.columns.iter().any(Column::has_plugins) {
        while let Some(record) = parser.next() {
            let record = record?;
            let metadata = parser.metadata();
            files.row(config.columns.iter().map(|c| c.value(&record, metadata)), metadata)?;
            if first.is_none() {
                first = Some((record, metadata.clone()));
            }
        }
    } else {
        let mut batch = Vec::with_capacity(BATCH);
        loop {
            batch.clear();
            for record in parser.by_ref().take(BATCH) {
                batch.push(record?);
            }
            let Some(record) = batch.first() else { break };
            let metadata = parser.metadata();
            if first.is_none() {
                first = Some((record.clone(), metadata.clone()));
            }
            config.filter(&mut batch, metadata, call)?;
            // Columns with plugins for the whole batch, the others record by record: every
            // column a batch at a time cost nyp 6% (1.73 s became 1.83 s).
            let mut batched = Vec::with_capacity(config.columns.len());
            for c in &config.columns {
                let values = c.has_plugins().then(|| c.values(&batch, metadata, call));
                batched.push(values.transpose()?.map(Vec::into_iter));
            }
            for record in &batch {
                let values = config.columns.iter().zip(&mut batched).map(|(c, b)| match b {
                    Some(values) => values.next().expect("a value per record"),
                    None => c.value(record, metadata),
                });
                files.row(values, metadata)?;
            }
        }
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
    if let Some((record, seen)) = first {
        summary.late_metadata =
            (config.columns.iter()).any(|c| c.value(&record, &seen) != c.value(&record, &metadata));
    }
    Ok(summary)
}

/// The files closed so far and the one being written.
struct Files<'o, F> {
    output: &'o Output,
    schema: TypePtr,
    properties: Arc<WriterProperties>,
    group: Group,
    file: Option<Open>,
    summary: Summary,
    on_chunk: F,
}

impl<F: FnMut(&Chunk)> Files<'_, F> {
    /// Adds a row, first closing the open file if it is full.
    fn row<'a>(
        &mut self,
        values: impl Iterator<Item = Value<'a>>,
        metadata: &Metadata,
    ) -> Result<(), Error> {
        let chunk_size = self.output.chunk_size;
        if let Some(f) = self.file.take_if(|f| chunk_size > 0 && f.chunk.records == chunk_size) {
            self.close(f, metadata)?;
        }
        let f = match self.file.take() {
            Some(f) => f,
            None => self.open()?,
        };
        let f = self.file.insert(f);
        self.group.push(values)?;
        f.chunk.records += 1;
        if self.group.len() == self.output.row_group_size {
            self.group.flush(&mut f.writer)?;
        }
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
        (self.on_chunk)(&f.chunk);
        Ok(())
    }

    /// Closes the open file, or writes one with no rows when there was no record.
    fn finish(mut self, metadata: &Metadata) -> Result<Summary, Error> {
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
