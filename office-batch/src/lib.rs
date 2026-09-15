//! Bounded in-memory Office file transformations.
//!
//! The caller owns input authorization, immutable snapshots, output publication
//! and durable receipts. Passing package checks never authorizes native Office
//! activation. No functions in this crate open paths or launch applications.
mod docx_body;
pub mod docx_copy;
pub mod docx_inspect;
mod docx_parts;
pub mod ooxml_package;
mod ooxml_relationships;
pub mod pptx_copy;
pub mod pptx_inspect;
mod pptx_notes;
pub mod pptx_parts;
mod pptx_text;
pub mod xlsx_cells;
pub mod xlsx_edit;
pub mod xlsx_formula;
pub mod xlsx_formula_inventory;
mod xlsx_insert;
pub mod xlsx_native_inputs;
pub mod xlsx_native_merge;
pub mod xlsx_native_preflight;
pub mod xlsx_native_preservation;
pub mod xlsx_parts;
pub mod xlsx_result;
mod xlsx_rule_formulas;
mod xlsx_strings;
mod xlsx_target;
pub mod xlsx_workbook_result;
