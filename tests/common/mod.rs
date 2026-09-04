//! Helpers shared by the integration test crates.
//!
//! Files under `tests/` are each compiled as their own test binary, but a
//! subdirectory module like this one is not, so it is the standard place to
//! put code common to several of them.

use std::process::Command;

/// The tests all talk over loopback, so an ambient proxy in the developer's
/// environment must not be inherited: the HTTP client honours `*_PROXY` and
/// would try to reach 127.0.0.1 through it.
///
/// Shared rather than copied per crate so a new variable, or a rename, is one
/// edit instead of three.
pub fn no_inherited_proxy(cmd: &mut Command) {
    for var in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
        cmd.env_remove(var);
    }
}
