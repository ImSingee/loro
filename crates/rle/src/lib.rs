//! Run length encoding library.
//!
//! There are many mergeable types. By merging them together we can get a more compact representation of the data.
//! For example, in many cases, `[0..5, 5..10]` can be merged into `0..10`.
//!
//! # RleVec
//!
//! RleVec<T> is a vector that can be compressed using run-length encoding.
//!
//! A T value may be merged with its neighbors. When we push new element, the new value
//! may be merged with the last element in the array. Each value has a length, so there
//! are two types of indexes:
//! 1. (merged) It refers to the index of the merged element.
//! 2. (atom) The index of substantial elements. It refers to the index of the atom element.
//!
//! By default, we use atom index in RleVec.
//! - len() returns the number of atom elements in the array.
//! - get(index) returns the atom element at the index.
//! - slice(from, to) returns a slice of atom elements from the index from to the index to.
//!
//!
#![allow(dead_code)]
#![deny(clippy::undocumented_unsafe_blocks)]
pub mod range_map;
mod rle_trait;
pub mod rle_tree;
mod rle_vec;
mod rle_vec_old;
pub use crate::rle_trait::{
    HasIndex, HasLength, Mergable, Rle, RleCollection, RlePush, Slice, Sliceable, ZeroElement,
};
pub use crate::rle_vec::{slice_vec_by, RleVec, RleVecWithLen};
pub use crate::rle_vec_old::{RleVecWithIndex, SearchResult, SliceIterator};
pub mod rle_impl;
pub use rle_tree::tree_trait::RleTreeTrait;
pub use rle_tree::RleTree;
mod small_set;
#[cfg(test)]
mod test;
