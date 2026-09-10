fn main() {
    // Answered before anything else, and in particular before Qt is
    // initialised: `--version` is what a packaged build is asked first, and
    // that answer must not itself depend on the DLLs beside it loading.
    if std::env::args().skip(1).any(|a| a == "--version") {
        println!("{}", version_line());
        std::process::exit(0);
    }
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("BS_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    let code = bondsymphonic_ide::ffi::ffi::run_app();
    std::process::exit(code);
}

/// What `--version` prints: this build's crate version and the daemon protocol
/// it speaks. The IDE and the daemon ship as a pair, so the protocol number is
/// the one that says whether a given daemon will talk to this exe at all.
fn version_line() -> String {
    format!(
        "bondsymphonic-ide {} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        bondsymphonic_proto::PROTOCOL_VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_crate_and_the_protocol() {
        assert_eq!(
            version_line(),
            format!(
                "bondsymphonic-ide {} (protocol 1)",
                env!("CARGO_PKG_VERSION")
            )
        );
        // The protocol number is read, not typed twice: this is what makes the
        // line change on its own when the wire format does.
        assert_eq!(bondsymphonic_proto::PROTOCOL_VERSION, 1);
    }
}
