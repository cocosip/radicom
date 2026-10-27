//! File and IO primitives (docs/05-file-io.md).

pub mod byte_source;
pub mod dataset_builder;
pub mod file;
pub mod pool;
pub mod reader;
pub mod trace;
pub mod writer;

pub use pool::{BufferPool, PooledBuffer};
