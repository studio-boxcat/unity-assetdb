//! Unity asset GUID → name index baker.
//!
//! Walks `<project>/Assets/`, parses `.meta` and asset YAML, writes a
//! compact bincode database (`store::AssetDb`) for tools that need to
//! resolve asset references by name without loading the Unity editor.
//!
//! ## Modules
//!
//! - [`store`] — on-disk schema (`AssetDb`, `AssetEntry`, `SubAsset`, `AssetType`).
//! - [`class_id`] — Unity classID enum.
//! - [`meta`] — `.meta` parser.
//! - [`asset`] — asset YAML parser.
//! - [`walk`] — project-root resolver + parallel walker.
//! - [`bake`] — orchestrator (`BakeOptions`, `bake`, `parse_one`).
//! - [`query`] — read-only lookups against a baked `asset-db.bin`.
//! - [`register`] — synthesize a `.meta` outside Unity, incremental db insert.
//! - [`suggest`] — fuzzy "did you mean" helper used by the query CLI.
//! - [`usage`] — scan project files for references to a given GUID.

pub mod asset;
pub mod bake;
pub mod class_id;
pub mod meta;
pub mod query;
pub mod register;
pub mod store;
pub mod suggest;
pub mod usage;
pub mod walk;
