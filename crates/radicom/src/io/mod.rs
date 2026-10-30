//! File and IO primitives (docs/01-architecture.md §7).

pub mod byte_source;
pub mod dataset_builder;
pub mod file;
pub mod file_ref;
pub mod reader;
pub mod temp;
pub mod trace;
pub mod writer;

pub use file_ref::{DirectoryRef, PathFile, TempFileProvider};
pub use temp::create_unique_file;
