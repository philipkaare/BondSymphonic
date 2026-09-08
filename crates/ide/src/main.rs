fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("BS_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    let code = bondsymphonic_ide::ffi::ffi::run_app();
    std::process::exit(code);
}
