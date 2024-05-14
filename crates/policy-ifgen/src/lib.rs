#![cfg_attr(not(doctest), doc = include_str!("../README.md"))]
#![no_std]
#![warn(
    clippy::arithmetic_side_effects,
    clippy::wildcard_imports,
    missing_docs
)]

extern crate alloc;

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::fmt;

/// Macros used in code generated by `policy_ifgen_build``.
pub mod macros {
    pub use policy_ifgen_macro::{actions, effect, effects, value};
}

pub use policy_vm::{Id, KVPair, Struct, Value};
pub use runtime::{vm_action, vm_effect, ClientError, VmAction, VmEffect};
#[cfg(feature = "serde")]
pub use serde;

/// Struct fields
pub type Fields = Vec<KVPair>;
/// Map of struct fields
pub type FieldMap = BTreeMap<String, Value>;

/// An actor which can call policy actions.
pub trait Actor {
    /// Call an "untyped" policy action ([`VmAction`]).
    fn call_action(&mut self, action: VmAction<'_>) -> Result<(), ClientError>;
}

/// Possible errors from policy effect parsing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectsParseError {
    /// Effect has one or more extra fields.
    ExtraFields,
    /// Effect is missing an expected field.
    MissingField,
    /// Effect has unexpected field type.
    FieldTypeMismatch,
    /// Effect has unknown effect name.
    UnknownEffectName,
}

impl trouble::Error for EffectsParseError {}

impl fmt::Display for EffectsParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExtraFields => f.write_str("effect has one or more extra fields"),
            Self::MissingField => f.write_str("effect is missing an expected field"),
            Self::FieldTypeMismatch => f.write_str("effect has an unexpected field type"),
            Self::UnknownEffectName => f.write_str("effect has an unknown effect name"),
        }
    }
}

/// Possible errors when parsing a struct from a [`Value`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StructParseError {
    /// Struct has one or more extra fields.
    ExtraFields,
    /// Struct is missing an expected field.
    MissingField,
    /// Struct has unexpected field type.
    FieldTypeMismatch,
    /// Struct name does not match.
    NameMismatch,
    /// Value is not a struct.
    InvalidType,
}

impl trouble::Error for StructParseError {}

impl fmt::Display for StructParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExtraFields => f.write_str("struct has one or more extra fields"),
            Self::MissingField => f.write_str("struct is missing an expected field"),
            Self::FieldTypeMismatch => f.write_str("struct has an unexpected field type"),
            Self::NameMismatch => f.write_str("struct name does not match"),
            Self::InvalidType => f.write_str("value is not a struct"),
        }
    }
}
