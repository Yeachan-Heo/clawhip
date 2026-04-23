#[test]
fn runtime_is_configured_from_cli_worker_threads_flag() {
    let main_rs = include_str!("../src/main.rs");
    let cli_rs = include_str!("../src/cli.rs");

    assert!(
        cli_rs.contains("worker_threads: Option<NonZeroUsize>"),
        "expected Start command to expose an optional worker_threads CLI flag"
    );
    assert!(
        main_rs.contains("Builder::new_multi_thread()"),
        "expected clawhip to build the Tokio runtime explicitly"
    );
    assert!(
        main_rs.contains(".worker_threads(worker_threads)"),
        "expected runtime builder to honor the CLI worker_threads override"
    );
    assert!(
        !main_rs.contains("#[tokio::main(flavor = \"multi_thread\", worker_threads = 2)]"),
        "runtime should no longer hardcode two worker threads"
    );
}
