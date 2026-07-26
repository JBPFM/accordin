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

/// Like `env_flag`, but for a variable that is on unless explicitly turned off.
/// `env_flag` cannot express this: it reports "unset" and "0" alike as false.
pub fn env_flag_default_on(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            !(value == "0"
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no")
                || value.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

pub fn env_u32_clamped(name: &str, default: u32, min: u32, max: u32) -> u32 {
    let Ok(value) = std::env::var(name) else {
        return default;
    };

    let Ok(parsed) = value.trim().parse::<u32>() else {
        return default;
    };

    parsed.clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::{env_flag, env_u32_clamped};

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
    fn env_u32_clamped_uses_default_when_missing_or_invalid() {
        let key = "ACCORDIN_SHARED_TEST_ENV_U32_DEFAULT";
        unsafe {
            std::env::remove_var(key);
        }
        assert_eq!(env_u32_clamped(key, 60, 0, 100), 60);

        unsafe {
            std::env::set_var(key, "not-a-number");
        }
        assert_eq!(env_u32_clamped(key, 60, 0, 100), 60);

        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn env_u32_clamped_clamps_values_to_bounds() {
        let key = "ACCORDIN_SHARED_TEST_ENV_U32_CLAMP";

        unsafe {
            std::env::set_var(key, "35");
        }
        assert_eq!(env_u32_clamped(key, 60, 0, 100), 35);

        unsafe {
            std::env::set_var(key, "101");
        }
        assert_eq!(env_u32_clamped(key, 60, 0, 100), 100);

        unsafe {
            std::env::remove_var(key);
        }
    }
}
