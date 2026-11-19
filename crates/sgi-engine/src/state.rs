//! Saved state as a stream of named chunks.

use crate::engine::Fault;

/// A named piece of saved state: a small blob, or one piece of a large
/// store (`index` says which). A sink gets them in the order `save` makes
/// them; `load` may be given them in any order but `meta` first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub name: String,
    pub index: u64,
    pub bytes: Vec<u8>,
}

/// Where `save` puts its chunks: a migration stream, a file, or `Vec<Chunk>`.
pub trait StateSink {
    fn put(&mut self, chunk: Chunk) -> Result<(), Fault>;
}

/// Where `load` takes chunks from. `None` ends the state.
pub trait StateSource {
    fn next(&mut self) -> Result<Option<Chunk>, Fault>;
}

impl StateSink for Vec<Chunk> {
    fn put(&mut self, chunk: Chunk) -> Result<(), Fault> {
        self.push(chunk);
        Ok(())
    }
}

/// A source over chunks held in memory.
pub struct VecSource(pub std::vec::IntoIter<Chunk>);

impl VecSource {
    pub fn new(chunks: Vec<Chunk>) -> VecSource {
        VecSource(chunks.into_iter())
    }
}

impl StateSource for VecSource {
    fn next(&mut self) -> Result<Option<Chunk>, Fault> {
        Ok(self.0.next())
    }
}
