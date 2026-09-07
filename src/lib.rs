#![allow(
    // We don't use syntax sugar where it's not necessary.
    clippy::match_like_matches_macro,
    // Redundant matching is more explicit.
    clippy::redundant_pattern_matching,
    // Explicit lifetimes are often easier to reason about.
    clippy::needless_lifetimes,
    // No need for defaults in the internal types.
    clippy::new_without_default,
    // Matches are good and extendable, no need to make an exception here.
    clippy::single_match,
    // Push commands are more regular than macros.
    clippy::vec_init_then_push,
    // More regular structure is fine.
    clippy::collapsible_if,
)]
#![warn(
    trivial_numeric_casts,
    unused_extern_crates,
    // We don't match on a reference, unless required.
    clippy::pattern_type_mismatch,
)]

pub mod app_state;
pub mod archive;
pub mod core;
pub mod elevate;
pub mod settings;
pub mod sftp;
pub mod snapshot;
pub mod ssh;
pub mod syntax;
pub mod theme;
pub mod workers;

/// Deliberately excludes machine names, paths, SSH configuration and error text.
pub const SUPPORT: navigato_support::Info = navigato_support::Info {
    app: navigato_support::App::Fileman,
    version: env!("CARGO_PKG_VERSION"),
    revision: option_env!("GITHUB_SHA"),
    private_email: option_env!("NAVIGATO_PRIVATE_REPORT_EMAIL"),
};
