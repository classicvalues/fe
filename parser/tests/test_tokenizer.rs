extern crate difference;
extern crate pyo3;
extern crate rust_vyper_parser;

use std::fmt;

use pyo3::prelude::*;
use pyo3::types::PyBytes;
use rust_vyper_parser::tokenizer::*;

/// Return the lines in `lines` prefixed with the prefix in `prefix.
fn prefix_lines(prefix: &str, lines: &str) -> String {
    lines
        .lines()
        .map(|i| [prefix, i].concat())
        .collect::<Vec<String>>()
        .join("\n")
}

/// Wrapper struct for formatting changesets from the `difference` package.
pub struct Diff(difference::Changeset);

impl Diff {
    pub fn new(left: &str, right: &str) -> Self {
        Self(difference::Changeset::new(left, right, "\n"))
    }
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for d in &self.0.diffs {
            match *d {
                difference::Difference::Same(ref x) => {
                    write!(f, "{}{}", prefix_lines(" ", x), self.0.split)?;
                }
                difference::Difference::Add(ref x) => {
                    write!(f, "\x1b[92m{}\x1b[0m{}", prefix_lines("+", x), self.0.split)?;
                }
                difference::Difference::Rem(ref x) => {
                    write!(f, "\x1b[91m{}\x1b[0m{}", prefix_lines("-", x), self.0.split)?;
                }
            }
        }
        Ok(())
    }
}

/// Compare the given strings and panic when not equal with a colorized line diff.
macro_rules! assert_strings_eq {
    ($left:expr , $right:expr,) => ({
        assert_eq!($left, $right)
    });
    ($left:expr , $right:expr) => ({
        match (&($left), &($right)) {
            (left_val, right_val) => {
                if *left_val != *right_val {
                    panic!(
                        "assertion failed: `(left == right)`\ndiff:\n{}",
                        Diff::new(left_val, right_val),
                    )
                }
            }
        }
    });
    ($left:expr , $right:expr, $($arg:tt)*) => ({
        match (&($left), &($right)) {
            (left_val, right_val) => {
                if *left_val != *right_val {
                    panic!(
                        "assertion failed: `(left == right)`: {}\ndiff:\n{}",
                        format_args!($($arg)*),
                        Diff::new(left_val, right_val),
                    )
                }
            }
        }
    });
}

/// Load the `token_helpers.py` module for generating tokenizations using the python std lib
/// `tokenize` module.
fn get_token_helpers<'p>(py: Python<'p>) -> PyResult<&'p PyModule> {
    PyModule::from_code(
        py,
        include_str!("token_helpers.py"),
        "token_helpers.py",
        "token_helpers",
    )
}

#[test]
fn test_tokenize() {
    // Generate tokenization using rust
    let test_py = include_str!("fixtures/tokenizer/test.py");
    let test_py_tokens = tokenize(test_py).unwrap();
    let actual_token_json = serde_json::to_string_pretty(&test_py_tokens).unwrap();

    // Generate known-good tokenization using python
    let gil = Python::acquire_gil();
    let py = gil.python();
    let token_helpers = get_token_helpers(py).unwrap();

    let source = PyBytes::new(py, test_py.as_bytes());
    let result = token_helpers.call("get_token_json", (source,), None);

    match result {
        Ok(py_string) => {
            let expected_token_json: String = py_string.extract().unwrap();
            assert_strings_eq!(actual_token_json, expected_token_json);
        }
        Err(py_err) => py_err.print(py),
    }
}