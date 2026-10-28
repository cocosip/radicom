//! File and IO primitives (docs/05-file-io.md).

pub mod byte_source;
pub mod dataset_builder;
pub mod file;
pub mod file_ref;
pub mod pool;
pub mod reader;
pub mod trace;
pub mod writer;

pub use file_ref::{DirectoryRef, FileRef, PathFile, TempFileProvider};
pub use pool::{BufferPool, PooledBuffer};
