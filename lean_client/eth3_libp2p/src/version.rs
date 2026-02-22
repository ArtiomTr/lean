/// Application name for lean ethereum client
pub const APPLICATION_NAME: &str = "lean-eth";

/// Application version
pub const APPLICATION_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Full version string with platform info
pub fn version_with_platform() -> String {
    format!(
        "{}/{}/{}",
        APPLICATION_NAME,
        APPLICATION_VERSION,
        std::env::consts::OS
    )
}
