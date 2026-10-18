fn main() {
    if let Err(error) = ai_fence_cli_core::workspace::entry(std::env::args_os()) {
        eprintln!("ai-fence workspace: {error:#}");
        std::process::exit(1);
    }
}
