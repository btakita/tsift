fn main() {
    if let Err(err) = tsift_cli::run() {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
}
