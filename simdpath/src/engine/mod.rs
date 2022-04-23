//! Base traits for different implementations of JSONPath execution engines.
//!
//! Defines the [`Runner`] trait that provides different ways of retrieving
//! query results from input bytes. Result types are defined in the [result]
//! module.

pub mod result;

use align::{
    alignment::{self, Alignment},
    AlignedBytes,
};
use cfg_if::cfg_if;
use len_trait::Len;
use result::CountResult;

/// Input into a query engine.
pub struct Input {
    bytes: AlignedBytes<alignment::Page>,
}

impl std::ops::Deref for Input {
    type Target = AlignedBytes<alignment::Page>;

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl Input {
    /// Transmute a buffer into an input.
    ///
    /// The buffer must know its length, may be extended by auxillary UTF8 characters
    /// and will be interpreted as a slice of bytes at the end.
    pub fn new<T: Extend<char> + Len + AsRef<[u8]>>(src: T) -> Self {
        cfg_if! {
            if #[cfg(feature = "simd")] {
                let mut contents = src;
                let rem = contents.len() % alignment::TwoSimdBlocks::size();
                let pad = if rem == 0 {
                    0
                } else {
                    alignment::TwoSimdBlocks::size() - rem
                };

                let extension = std::iter::repeat('\0').take(pad + alignment::TwoSimdBlocks::size());
                contents.extend(extension);

                debug_assert_eq!(contents.len() % alignment::TwoSimdBlocks::size(), 0);

                Self {
                    bytes: AlignedBytes::<alignment::Page>::from(contents.as_ref()),
                }
            }
            else {
                Self {
                    bytes: AlignedBytes::<alignment::Page>::from(src.as_ref()),
                }
            }
        }
    }
}

/// Trait for an engine that can run its query on a given input.
pub trait Runner {
    /// Count the number of values satisfying the query on given [`Input`].
    fn count(&self, input: &Input) -> CountResult;
}
