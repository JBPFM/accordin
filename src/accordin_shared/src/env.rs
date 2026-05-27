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

pub const INACTIVE_DSQ_ENV: &str = "ACCORDIN_INACTIVE_DSQ";
pub const EAGER_TOKEN_RELEASE_ENV: &str = "ACCORDIN_EAGER_TOKEN_RELEASE";

pub fn inactive_dsq_global_enabled_for_env(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        Some(value)
            if value.eq_ignore_ascii_case("global") || value.eq_ignore_ascii_case("shared") =>
        {
            true
        }
        _ => false,
    }
}

pub fn inactive_dsq_global_enabled_by_env() -> bool {
    inactive_dsq_global_enabled_for_env(std::env::var(INACTIVE_DSQ_ENV).ok().as_deref())
}

pub fn eager_token_release_enabled_for_env(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("eager") => true,
        Some(value) => {
            value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
        }
        None => false,
    }
}

pub fn eager_token_release_enabled_by_env() -> bool {
    eager_token_release_enabled_for_env(std::env::var(EAGER_TOKEN_RELEASE_ENV).ok().as_deref())
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

    #[test]
    fn inactive_dsq_global_parser_defaults_to_per_cpu() {
        assert!(!super::inactive_dsq_global_enabled_for_env(None));
        assert!(!super::inactive_dsq_global_enabled_for_env(Some("")));
        assert!(!super::inactive_dsq_global_enabled_for_env(Some("per_cpu")));
        assert!(!super::inactive_dsq_global_enabled_for_env(Some("per-cpu")));
        assert!(!super::inactive_dsq_global_enabled_for_env(Some("local")));
    }

    #[test]
    fn inactive_dsq_global_parser_accepts_global_aliases() {
        assert!(super::inactive_dsq_global_enabled_for_env(Some("global")));
        assert!(super::inactive_dsq_global_enabled_for_env(Some("shared")));
        assert!(super::inactive_dsq_global_enabled_for_env(Some("GLOBAL")));
    }

    #[test]
    fn eager_token_release_parser_defaults_to_lazy_and_accepts_truthy_values() {
        assert!(!super::eager_token_release_enabled_for_env(None));
        assert!(!super::eager_token_release_enabled_for_env(Some("lazy")));
        assert!(!super::eager_token_release_enabled_for_env(Some("0")));

        for value in ["1", "true", "yes", "on", "eager"] {
            assert!(super::eager_token_release_enabled_for_env(Some(value)));
        }
    }
}
