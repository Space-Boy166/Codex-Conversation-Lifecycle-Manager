use std::ffi::OsString;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match conversation_lifecycle_manager::run_proxy(args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("Codex CLM proxy failed: {error:#}");
            std::process::exit(1);
        }
    }
}
