use lofi_code::BashEnv;
use lofi_types::BashConfig;

const BASELINE_NAMES: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "TZ", "LANG", "TMPDIR",
];

// --env pairs are injected into the child environment and redacted from
// captured output regardless of strip_env, so a per-instance credential
// passed on the command line behaves like a pass_env entry.
#[must_use]
pub fn resolve_bash_env(cfg: &BashConfig, env: &[(String, String)]) -> BashEnv {
    let mut env_res = BashEnv {
        strip_env: cfg.strip_env,
        baseline: Vec::new(),
        extras: Vec::new(),
        redact: Vec::new(),
    };
    if cfg.strip_env {
        env_res.baseline = baseline_from_parent();
    }
    for name in &cfg.pass_env {
        if let Ok(val) = std::env::var(name) {
            env_res.extras.push((name.clone(), val.clone()));
            env_res.redact.push(val);
        }
    }
    for (name, val) in env {
        env_res.extras.push((name.clone(), val.clone()));
        env_res.redact.push(val.clone());
    }
    if let Some(path) = &cfg.env_file {
        let expanded = expand_tilde(path);
        if let Ok(text) = std::fs::read_to_string(&expanded) {
            for (k, v) in parse_env_file(&text) {
                env_res.extras.push((k, v.clone()));
                env_res.redact.push(v);
            }
        }
    }
    env_res.redact.sort_by_key(|v| std::cmp::Reverse(v.len()));
    env_res.redact.dedup();
    env_res.redact.retain(|v| !v.is_empty());
    env_res
}

fn baseline_from_parent() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in std::env::vars() {
        if BASELINE_NAMES.contains(&k.as_str()) || k.starts_with("LC_") {
            out.push((k, v));
        }
    }
    out
}

fn expand_tilde(p: &std::path::Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        return match std::env::var_os("HOME") {
            Some(home) => std::path::PathBuf::from(home),
            None => p.to_path_buf(),
        };
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut joined = std::path::PathBuf::from(home);
            joined.push(rest);
            return joined;
        }
    }
    p.to_path_buf()
}

fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let mut val = v.trim().to_string();
        if val.len() >= 2 {
            let bytes = val.as_bytes();
            let first = bytes[0];
            let last = bytes[val.len() - 1];
            if (first == b'"' || first == b'\'') && first == last {
                val = val[1..val.len() - 1].to_string();
            }
        }
        out.push((key, val));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn baseline_includes_path_and_locale() {
        std::env::set_var("LOFI_BASH_TEST_BASELINE", "x");
        let b = baseline_from_parent();
        assert!(!b.iter().any(|(k, _)| k == "LOFI_BASH_TEST_BASELINE"));
        std::env::remove_var("LOFI_BASH_TEST_BASELINE");
        assert!(b.iter().any(|(k, _)| k == "PATH"));
    }

    #[test]
    fn parse_env_file_skips_comments_and_quotes() {
        let text = "# comment\nFOO=bar\n\nEMPTY=\nQUOTED=\"a b\"\nNOSIGN\nKEY='v'\n";
        let pairs = parse_env_file(text);
        let map: std::collections::HashMap<&str, &str> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(map.get("FOO"), Some(&"bar"));
        assert_eq!(map.get("EMPTY"), Some(&""));
        assert_eq!(map.get("QUOTED"), Some(&"a b"));
        assert_eq!(map.get("KEY"), Some(&"v"));
        assert!(!map.contains_key("NOSIGN"));
        assert!(!map.contains_key("comment"));
    }

    #[test]
    fn env_pairs_are_injected_and_redacted() {
        let cfg = BashConfig {
            strip_env: true,
            pass_env: Vec::new(),
            env_file: None,
        };
        let env = resolve_bash_env(&cfg, &[("EXAMPLE_KEY".to_string(), "secret".to_string())]);
        assert!(env
            .extras
            .iter()
            .any(|(k, v)| k == "EXAMPLE_KEY" && v == "secret"));
        assert!(env.redact.contains(&"secret".to_string()));
    }
}
