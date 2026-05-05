use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ToolLimits {
    pub max_read_bytes: usize,
    pub max_output_bytes: usize,
    pub bash_timeout: Duration,
    pub max_glob_results: usize,
    pub max_grep_results: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: 1024 * 1024,
            max_output_bytes: 64 * 1024,
            bash_timeout: Duration::from_secs(30),
            max_glob_results: 500,
            max_grep_results: 500,
        }
    }
}
