//! Structural checks for `acceptance/thresholds.toml` (PLAN.md section 4.2).
//!
//! Uses the same parser as `cargo xtask accept`. It checks shape (ids, ops, units, value
//! presence) and deliberately does not pin numeric values, so tightening a threshold does
//! not require editing this test. The per-rule rejection cases are unit tests in
//! `xtask/src/thresholds.rs`.

use xtask::thresholds::{self, EXPECTED_IDS};

fn load() -> Vec<thresholds::Threshold> {
    thresholds::load(&thresholds::path_in(&xtask::repo_root())).unwrap_or_else(|e| panic!("{e:#}"))
}

#[test]
fn has_exactly_the_plan_table_ids() {
    let problems = thresholds::check_ids(&load(), EXPECTED_IDS);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn every_entry_is_well_formed() {
    // `load` fails with every problem listed if any entry is malformed.
    assert_eq!(load().len(), EXPECTED_IDS.len());
}

#[test]
fn plan_table_matches_expected_ids() {
    let md = std::fs::read_to_string(xtask::repo_root().join("PLAN.md")).expect("read PLAN.md");
    assert_eq!(thresholds::plan_table_ids(&md), EXPECTED_IDS);
}
