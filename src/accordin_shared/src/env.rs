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

#[cfg(test)]
mod tests {
    use super::env_flag;

    #[test]
    fn env_flag_defaults_to_false() {
        let key = "ACCORDIN_SHARED_TEST_ENV_FLAG_MISSING";
        unsafe {
            std::env::remove_var(key);
        }
        assert!(!env_flag(key));
    }

    #[test]
    fn env_flag_accepts_common_truthy_values() {
        let key = "ACCORDIN_SHARED_TEST_ENV_FLAG_TRUTHY";
        for value in ["1", "true", "TRUE", "yes", "on"] {
            unsafe {
                std::env::set_var(key, value);
            }
            assert!(env_flag(key), "expected truthy value {value}");
        }
        unsafe {
            std::env::remove_var(key);
        }
    }
}
