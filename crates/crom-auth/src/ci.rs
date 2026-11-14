//! Detect common CI environments so we can warn (or block) when
//! subscription auth is selected automatically.

use std::collections::BTreeMap;

/// Environment variable names commonly set by hosted CI runners.
pub const CI_ENV_VARS: &[&str] = &[
    "CI",
    "GITHUB_ACTIONS",
    "BUILDKITE",
    "GITLAB_CI",
    "CIRCLECI",
    "TRAVIS",
    "JENKINS_URL",
    "TF_BUILD",
];

/// Returns the name of the first CI marker env var found, if any.
pub fn detect_ci(env: &BTreeMap<String, String>) -> Option<&'static str> {
    for name in CI_ENV_VARS {
        match env.get(*name).map(|s| s.as_str()) {
            Some("") | None => continue,
            Some("0" | "false" | "False" | "FALSE") => continue,
            Some(_) => return Some(name),
        }
    }
    None
}

/// Decision returned by [`enforce_ci_policy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiDecision {
    /// CI not detected, or method is API-key based.
    Allowed,
    /// Subscription auth selected in CI; explicit override turned off.
    BlockedSubscription { marker: &'static str },
    /// Subscription auth selected in CI; explicitly allowed via setting.
    AllowedWithWarning { marker: &'static str },
}

/// Check whether selecting `is_subscription_auth` is allowed in the
/// current environment. `allow_in_ci` mirrors
/// `auth.allow_subscription_auth_in_ci`.
pub fn enforce_ci_policy(
    env: &BTreeMap<String, String>,
    is_subscription_auth: bool,
    allow_in_ci: bool,
) -> CiDecision {
    let marker = match detect_ci(env) {
        Some(m) => m,
        None => return CiDecision::Allowed,
    };
    if !is_subscription_auth {
        return CiDecision::Allowed;
    }
    if allow_in_ci {
        CiDecision::AllowedWithWarning { marker }
    } else {
        CiDecision::BlockedSubscription { marker }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(name: &str, value: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert(name.into(), value.into());
        m
    }

    #[test]
    // CI marker detection ignores empty/disabled values.
    fn detect_ci_ignores_disabled_values() {
        let m = env_with("CI", "false");
        assert_eq!(detect_ci(&m), None);
        let m = env_with("CI", "1");
        assert_eq!(detect_ci(&m), Some("CI"));
    }

    #[test]
    // CI + subscription auth + allow_in_ci=false yields BlockedSubscription.
    fn ci_blocks_subscription_by_default() {
        let env = env_with("GITHUB_ACTIONS", "true");
        let d = enforce_ci_policy(&env, true, false);
        assert_eq!(
            d,
            CiDecision::BlockedSubscription {
                marker: "GITHUB_ACTIONS"
            }
        );
    }

    #[test]
    // CI + subscription auth + allow_in_ci=true yields AllowedWithWarning.
    fn ci_allows_subscription_when_explicitly_enabled() {
        let env = env_with("GITHUB_ACTIONS", "true");
        let d = enforce_ci_policy(&env, true, true);
        assert!(matches!(d, CiDecision::AllowedWithWarning { .. }));
    }

    #[test]
    // CI + API-key auth is always Allowed.
    fn ci_allows_api_key_auth() {
        let env = env_with("CI", "true");
        let d = enforce_ci_policy(&env, false, false);
        assert_eq!(d, CiDecision::Allowed);
    }

    #[test]
    // Outside CI, subscription auth is always Allowed.
    fn no_ci_allows_subscription() {
        let env = BTreeMap::new();
        let d = enforce_ci_policy(&env, true, false);
        assert_eq!(d, CiDecision::Allowed);
    }
}
