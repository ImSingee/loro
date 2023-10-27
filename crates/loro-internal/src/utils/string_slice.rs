use std::{fmt::Debug, ops::Deref};

use append_only_bytes::BytesSlice;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    container::richtext::richtext_state::{unicode_to_utf8_index, utf16_to_utf8_index},
    delta::DeltaValue,
};

use super::utf16::{count_unicode_chars, count_utf16_chars};

#[derive(Clone)]
pub struct StringSlice {
    bytes: Variant,
}

impl PartialEq for StringSlice {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for StringSlice {}

#[derive(Clone, PartialEq, Eq)]
enum Variant {
    BytesSlice(BytesSlice),
    Owned(String),
}

impl From<String> for StringSlice {
    fn from(s: String) -> Self {
        Self {
            bytes: Variant::Owned(s),
        }
    }
}

impl From<BytesSlice> for StringSlice {
    fn from(s: BytesSlice) -> Self {
        Self::new(s)
    }
}

impl From<&str> for StringSlice {
    fn from(s: &str) -> Self {
        Self {
            bytes: Variant::Owned(s.to_string()),
        }
    }
}

impl Debug for StringSlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StringSlice")
            .field("bytes", &self.as_str())
            .finish()
    }
}

impl StringSlice {
    pub fn new(s: BytesSlice) -> Self {
        std::str::from_utf8(&s).unwrap();
        Self {
            bytes: Variant::BytesSlice(s),
        }
    }

    pub fn as_str(&self) -> &str {
        match &self.bytes {
            // SAFETY: `bytes` is always valid utf8
            Variant::BytesSlice(s) => unsafe { std::str::from_utf8_unchecked(s) },
            Variant::Owned(s) => &s,
        }
    }

    pub fn to_string(&self) -> String {
        self.as_str().to_string()
    }

    pub fn len_bytes(&self) -> usize {
        match &self.bytes {
            Variant::BytesSlice(s) => s.len(),
            Variant::Owned(s) => s.len(),
        }
    }

    fn bytes(&self) -> &[u8] {
        match &self.bytes {
            Variant::BytesSlice(s) => s.deref(),
            Variant::Owned(s) => s.as_bytes(),
        }
    }

    pub fn len_unicode(&self) -> usize {
        count_unicode_chars(self.bytes())
    }

    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }
}

impl<'de> Deserialize<'de> for StringSlice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(s.into())
    }
}

impl Serialize for StringSlice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl DeltaValue for StringSlice {
    fn value_extend(&mut self, other: Self) -> Result<(), Self> {
        match (&mut self.bytes, &other.bytes) {
            (Variant::BytesSlice(s), Variant::BytesSlice(o)) => match s.try_merge(o) {
                Ok(_) => Ok(()),
                Err(_) => Err(other),
            },
            (Variant::Owned(s), _) => {
                s.push_str(other.as_str());
                Ok(())
            }
            _ => Err(other),
        }
    }

    fn take(&mut self, length: usize) -> Self {
        let length = if cfg!(feature = "wasm") {
            utf16_to_utf8_index(self.as_str(), length).unwrap()
        } else {
            unicode_to_utf8_index(self.as_str(), length).unwrap()
        };

        match &mut self.bytes {
            Variant::BytesSlice(s) => {
                let mut other = s.slice_clone(length..);
                s.slice_(..length);
                std::mem::swap(s, &mut other);
                Self {
                    bytes: Variant::BytesSlice(other),
                }
            }
            Variant::Owned(s) => {
                let mut other = s.split_off(length);
                std::mem::swap(s, &mut other);
                Self {
                    bytes: Variant::Owned(other),
                }
            }
        }
    }

    /// Unicode length of the string
    /// Utf16 length when in WASM
    fn length(&self) -> usize {
        if cfg!(feature = "wasm") {
            count_utf16_chars(self.bytes())
        } else {
            count_unicode_chars(self.bytes())
        }
    }
}