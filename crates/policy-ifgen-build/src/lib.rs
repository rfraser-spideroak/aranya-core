//! Generate typed policy interfaces from policy source code.

#![warn(
    clippy::arithmetic_side_effects,
    clippy::wildcard_imports,
    missing_docs
)]

use std::{fs, path::Path};

use anyhow::{Context, Result};
use policy_lang::lang::parse_policy_document;

mod imp;
pub use imp::generate_code;

/// Read policy from `input` and write Rust interface to `output`.
pub fn generate(input: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<()> {
    generate_(input.as_ref(), output.as_ref())
}

fn generate_(input: &Path, output: &Path) -> Result<()> {
    let policy_source = fs::read_to_string(input).with_context(|| format!("reading {input:?}"))?;

    let policy_doc = parse_policy_document(&policy_source)?;
    let rust_code = generate_code(&policy_doc);

    fs::write(output, rust_code).with_context(|| format!("writing to {output:?}"))?;

    Ok(())
}
