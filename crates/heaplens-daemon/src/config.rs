pub struct Config {
    /// Named pipe path the daemon listens on.
    /// Default: r"\\.\pipe\heaplens"
    /// Override: env var HEAPLENS_PIPE
    pub pipe_name: String,

    /// Diff/tick interval in milliseconds.
    /// Default: 33 (≈30 Hz)
    /// Override: env var HEAPLENS_TICK_MS
    pub tick_ms: u64,

    /// Exponential moving average time constant in milliseconds.
    /// Default: 5000
    /// Override: env var HEAPLENS_TAU_MS
    pub tau_ms: u64,

    /// Hot cluster threshold (allocation count).
    /// Default: 32
    /// Override: env var HEAPLENS_HOT_THRESHOLD
    pub hot_cluster_threshold: usize,

    /// Storm rate threshold (allocations/sec).
    /// Default: 1000
    /// Override: env var HEAPLENS_STORM_RATE
    pub storm_rate_threshold: u64,

    /// Storm window in milliseconds.
    /// Default: 1000
    /// Override: env var HEAPLENS_STORM_WINDOW_MS
    pub storm_window_ms: u64,

    /// WebSocket server address.
    /// Default: "127.0.0.1:9999"
    /// Override: env var HEAPLENS_WS_ADDR
    pub ws_addr: String,

    /// Path to the heap snapshot database.
    /// Default: "heaplens.db"
    /// Override: env var HEAPLENS_DB_PATH
    pub db_path: String,
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
            tau_ms: std::env::var("HEAPLENS_TAU_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5000),
            hot_cluster_threshold: std::env::var("HEAPLENS_HOT_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(32),
            storm_rate_threshold: std::env::var("HEAPLENS_STORM_RATE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000),
            storm_window_ms: std::env::var("HEAPLENS_STORM_WINDOW_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000),
            ws_addr: std::env::var("HEAPLENS_WS_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:9999".to_owned()),
            db_path: std::env::var("HEAPLENS_DB_PATH")
                .unwrap_or_else(|_| "heaplens.db".to_owned()),
        }
    }
}
