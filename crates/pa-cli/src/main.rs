fn main() {
    // The sandbox launcher, first of all: it confines (or detaches) this
    // fresh process image and execs the real program, so it must not pay
    // for, or be changed by, any of the setup below. Its arguments are
    // paths, which need not be UTF-8.
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(pa_os_sandbox::LAUNCHER_FLAG))
    {
        std::process::exit(pa_os_sandbox::launch_main(std::env::args_os().skip(2)));
    }
    // Every confined or session-leading child of this process starts
    // through that launcher (`posix_spawn`, no fork of this process).
    if let Ok(launcher) = pa_os_sandbox::Launcher::this_executable() {
        let _ = pa_os_sandbox::set_launcher(launcher);
    }
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
