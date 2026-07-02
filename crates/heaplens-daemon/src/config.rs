pub struct Config {
    /// Named pipe path the daemon listens on.
    /// Default: r"\\.\pipe\heaplens"
    /// Override: env var HEAPLENS_PIPE
    pub pipe_name: String,

    /// Diff/tick interval in milliseconds.
    /// Default: 33 (≈30 Hz)
    /// Override: env var HEAPLENS_TICK_MS
    pub tick_ms: u64,
}

impl Config {
    pub fn load() -> Self {
        Config {
            pipe_name: std::env::var("HEAPLENS_PIPE")
                .unwrap_or_else(|_| r"\\.\pipe\heaplens".to_owned()),
            tick_ms: std::env::var("HEAPLENS_TICK_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(33),
        }
    }
}
