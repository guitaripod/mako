/// Writes one line to the worker log, or to stderr when the code runs in a host test, where the console is absent.
pub fn info(line: &str) {
    #[cfg(target_arch = "wasm32")]
    worker::console_log!("{}", line);
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{line}");
}

/// Like `info` for a failure.
pub fn error(line: &str) {
    #[cfg(target_arch = "wasm32")]
    worker::console_error!("{}", line);
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("ERROR {line}");
}

/// The first characters of an id, enough to correlate log lines without recording the id.
pub fn id_prefix(id: &str) -> String {
    id.chars().take(8).collect()
}
