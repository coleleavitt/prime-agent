fn main() {
    // Allocator tuning before any thread spawns: the session-load and
    // attach-snapshot phases are large transient bursts, and glibc's
    // per-thread arenas otherwise keep each burst's high-water pages
    // resident for the process lifetime.
    pa_types::memory_release::cap_thread_arenas();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The kernel harness store's one-shot, before anything else starts:
    // it serves one request and exits.
    if args == [pa_cli::HARNESS_REQUEST_FLAG] {
        std::process::exit(pa_cli::run_harness_request());
    }
    let features = pa_cli::features::install_enabled_features();
    let code = pa_cli::main_with_runtime(&args, &pa_cli::PrintRuntime);
    pa_cli::features::flush_enabled_features();
    features.finish();
    std::process::exit(code);
}
