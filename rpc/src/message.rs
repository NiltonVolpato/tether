use alloc::vec::Vec;
use core::marker::PhantomData;

use flatbuffers::InvalidFlatbuffer;

/// A flatbuffers table used as a message. Generated code implements it for
/// each request and response table, at `'static` (e.g. `WifiStatus<'static>`).
pub trait Table {
    /// The table reading a buffer, e.g. `WifiStatus<'a>`.
    type View<'a>;
    /// Verifies `buf` and returns its root table.
    fn verify(buf: &[u8]) -> Result<Self::View<'_>, InvalidFlatbuffer>;
}

/// An owned message (flatc's object API, e.g. `WifiStatusT`) that serializes
/// to a finished flatbuffer.
pub trait Pack {
    fn to_bytes(&self) -> Vec<u8>;
}

/// A received message, verified when it arrived.
pub struct Message<T: Table, B = Vec<u8>> {
    buf: B,
    _t: PhantomData<fn() -> T>,
}

impl<T: Table, B: AsRef<[u8]>> Message<T, B> {
    pub fn new(buf: B) -> Result<Self, InvalidFlatbuffer> {
        T::verify(buf.as_ref())?;
        Ok(Self { buf, _t: PhantomData })
    }

    pub fn get(&self) -> T::View<'_> {
        T::verify(self.buf.as_ref()).expect("verified in new()")
    }

    pub fn bytes(&self) -> &[u8] {
        self.buf.as_ref()
    }

    pub fn into_inner(self) -> B {
        self.buf
    }
}
