//! The running engine's version and build channel.
//!
//! Release and nightly builds set `ARKLAY_CHANNEL` and `ARKLAY_COMMIT` at
//! compile time; a local `cargo build` keeps the `dev` defaults. The launcher
//! shows the result in its header and refuses to self-update a development
//! build.

/// The crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The build channel: `release`, `nightly` or `dev`.
pub const CHANNEL: &str = match option_env!("ARKLAY_CHANNEL") {
    Some(channel) => channel,
    None => "dev",
};
/// The full commit the build came from, when the build system supplied it.
pub const COMMIT: &str = match option_env!("ARKLAY_COMMIT") {
    Some(commit) => commit,
    None => "",
};

/// Whether this is a published build that may replace itself.
pub fn is_published() -> bool {
    matches!(CHANNEL, "release" | "nightly")
}

/// A short commit for display (at most seven characters).
pub fn short_commit() -> &'static str {
    let bytes = COMMIT.as_bytes();
    let len = bytes.len().min(7);
    // The commit is ASCII hex from the build system; fall back whole if not.
    if COMMIT.is_char_boundary(len) {
        &COMMIT[..len]
    } else {
        COMMIT
    }
}

/// The header label, e.g. `0.1.0 DEV` or `0.1.0 NIGHTLY 4f2a1c`.
pub fn label() -> String {
    let channel = CHANNEL.to_ascii_uppercase();
    if COMMIT.is_empty() {
        format!("{VERSION} {channel}")
    } else {
        format!("{VERSION} {channel} {}", short_commit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_names_the_channel() {
        let label = label();
        assert!(label.starts_with(VERSION), "{label}");
        assert!(label.contains(&CHANNEL.to_ascii_uppercase()), "{label}");
    }

    #[test]
    fn a_local_build_is_not_published() {
        assert_eq!(is_published(), matches!(CHANNEL, "release" | "nightly"));
    }
}
