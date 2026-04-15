fn main() {
    if let Err(error) = codex_telegram_bridge::main_entry() {
        println!("{}", codex_telegram_bridge::render_error_envelope(&error));
        std::process::exit(1);
    }
}
