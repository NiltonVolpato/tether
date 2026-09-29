//! A message whose root is a string, standing in for a flatc-generated table.

use api::{Pack, Table, flatbuffers};

pub struct Text;

impl Table for Text {
    type View<'a> = &'a str;

    fn verify(buf: &[u8]) -> Result<&str, flatbuffers::InvalidFlatbuffer> {
        flatbuffers::root::<&str>(buf)
    }
}

/// The owned form, like flatc's object API `...T` types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextT(pub String);

impl TextT {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl Pack for TextT {
    fn to_bytes(&self) -> Vec<u8> {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let root = fbb.create_string(&self.0);
        fbb.finish(root, None);
        fbb.finished_data().to_vec()
    }
}
