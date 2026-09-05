/// Returns `true` if the given environment variable is set to a recognized truthy value
/// (`"1"`, `"true"`, `"yes"`, `"on"`, case-insensitive). Whitespace is trimmed.
pub fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}
