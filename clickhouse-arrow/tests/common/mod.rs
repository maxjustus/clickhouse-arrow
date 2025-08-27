pub mod arrow_helpers;
pub mod constants;
pub mod dynamic_nested_tests;
pub mod native_helpers;
pub mod test_helpers;
pub mod typed_paths_tests;
pub mod version_compat;

pub const SEP: &str = "\n-------------------------------\n";

/// Little helper function to print headers for tests
pub fn header(qid: impl std::fmt::Display, msg: impl AsRef<str>) {
    eprintln!("{SEP} Query ID = {qid}\n {} {SEP}", msg.as_ref());
}
