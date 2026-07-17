//! Pathname-resolution conformance (`pjdfstest`'s namespace rules): the
//! VFS accepts only normalised absolute paths and rejects relative paths,
//! `.`/`..` components, embedded NUL bytes, and over-long components. A
//! rejected path is reported as [`VfsError::InvalidPath`].

use tairix_test_posix_fs_suite::*;

#[test]
fn a_normalised_absolute_path_parses() {
    let p = Path::parse("/System/Logs").expect("absolute path parses");
    assert_eq!(p.depth(), 2);
    assert_eq!(p.top_level(), Some("System"));
    assert_eq!(p.file_name(), Some("Logs"));
}

#[test]
fn redundant_and_trailing_slashes_collapse() {
    let p = Path::parse("//Users///alice/").expect("collapses");
    assert_eq!(p.depth(), 2);
    assert_eq!(p.top_level(), Some("Users"));
    assert_eq!(p.file_name(), Some("alice"));
}

#[test]
fn a_relative_path_is_invalid() {
    assert_eq!(Path::parse("Users/alice"), Err(VfsError::InvalidPath));
    assert_eq!(Path::parse(""), Err(VfsError::InvalidPath));
}

#[test]
fn dot_and_dotdot_components_are_invalid() {
    assert_eq!(Path::parse("/Users/./alice"), Err(VfsError::InvalidPath));
    assert_eq!(Path::parse("/Users/../alice"), Err(VfsError::InvalidPath));
    assert_eq!(Path::parse("/.."), Err(VfsError::InvalidPath));
}

#[test]
fn an_embedded_nul_byte_is_invalid() {
    assert_eq!(Path::parse("/Users/a\0b"), Err(VfsError::InvalidPath));
}

#[test]
fn an_over_long_component_is_invalid() {
    // A component longer than the 255-byte limit is refused.
    let long = "a".repeat(256);
    let text = format!("/{long}");
    assert_eq!(Path::parse(&text), Err(VfsError::InvalidPath));
}

#[test]
fn a_component_at_the_length_limit_is_accepted() {
    let max = "a".repeat(255);
    let text = format!("/{max}");
    let p = Path::parse(&text).expect("a 255-byte component is permitted");
    assert_eq!(p.file_name(), Some(max.as_str()));
}
