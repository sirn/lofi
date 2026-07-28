use lofi_types::{CommandEntry, MatchMode, ShellPolicyMode, WrapperKind, WrapperRuleConfig};

use super::engine::ResolvedPolicy;
use super::extract::build_wrapper_map;

fn standard_wrappers() -> Vec<WrapperRuleConfig> {
    vec![
        WrapperRuleConfig {
            name: "bash".into(),
            kind: WrapperKind::ShellC,
        },
        WrapperRuleConfig {
            name: "sh".into(),
            kind: WrapperKind::ShellC,
        },
        WrapperRuleConfig {
            name: "zsh".into(),
            kind: WrapperKind::ShellC,
        },
        WrapperRuleConfig {
            name: "dash".into(),
            kind: WrapperKind::ShellC,
        },
        WrapperRuleConfig {
            name: "ksh".into(),
            kind: WrapperKind::ShellC,
        },
        WrapperRuleConfig {
            name: "sudo".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "doas".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "time".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "nohup".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "nice".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "chroot".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "timeout".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "setsid".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "command".into(),
            kind: WrapperKind::UtilityOperand,
        },
        WrapperRuleConfig {
            name: "env".into(),
            kind: WrapperKind::Env,
        },
        WrapperRuleConfig {
            name: "xargs".into(),
            kind: WrapperKind::Xargs,
        },
        WrapperRuleConfig {
            name: "docker".into(),
            kind: WrapperKind::DockerRun,
        },
        WrapperRuleConfig {
            name: "podman".into(),
            kind: WrapperKind::DockerRun,
        },
    ]
}

fn universal_deny() -> Vec<CommandEntry> {
    vec![
        entry("sudo", MatchMode::Prefix),
        entry("doas", MatchMode::Prefix),
        entry("kill", MatchMode::Prefix),
        entry("killall", MatchMode::Prefix),
        entry("pkill", MatchMode::Prefix),
        entry("find /", MatchMode::Prefix),
        entry("find /nix", MatchMode::Prefix),
        entry("find /home", MatchMode::Prefix),
        entry("find /etc", MatchMode::Prefix),
        entry("find /var", MatchMode::Prefix),
        entry("find /usr", MatchMode::Prefix),
        entry("find /proc", MatchMode::Prefix),
        entry("find /sys", MatchMode::Prefix),
    ]
}

fn destructive_ask() -> Vec<CommandEntry> {
    vec![
        entry("rm", MatchMode::Prefix),
        entry("rmdir", MatchMode::Prefix),
        entry("shred", MatchMode::Prefix),
        entry("chmod", MatchMode::Prefix),
        entry("chown", MatchMode::Prefix),
        entry("dd", MatchMode::Prefix),
        entry("fdisk", MatchMode::Prefix),
        entry("mkfs", MatchMode::Prefix),
        entry("git commit", MatchMode::Prefix),
        entry("git push", MatchMode::Prefix),
        entry("git reset", MatchMode::Prefix),
        entry("git rebase", MatchMode::Prefix),
        entry("git clean", MatchMode::Prefix),
        entry("jj abandon", MatchMode::Prefix),
        entry("jj rebase", MatchMode::Prefix),
        entry("jj squash", MatchMode::Prefix),
        entry("jj undo", MatchMode::Prefix),
        entry("jj describe", MatchMode::Prefix),
        entry("nix run", MatchMode::Prefix),
        entry("printenv", MatchMode::Exact),
    ]
}

fn read_only_allow() -> Vec<CommandEntry> {
    vec![
        entry("ls", MatchMode::Prefix),
        entry("cat", MatchMode::Prefix),
        entry("head", MatchMode::Prefix),
        entry("tail", MatchMode::Prefix),
        entry("echo", MatchMode::Prefix),
        entry("printf", MatchMode::Prefix),
        entry("grep", MatchMode::Prefix),
        entry("rg", MatchMode::Prefix),
        entry("fd", MatchMode::Prefix),
        entry("find", MatchMode::Prefix),
        entry("tree", MatchMode::Prefix),
        entry("wc", MatchMode::Prefix),
        entry("sort", MatchMode::Prefix),
        entry("seq", MatchMode::Prefix),
        entry("uniq", MatchMode::Prefix),
        entry("tr", MatchMode::Prefix),
        entry("cut", MatchMode::Prefix),
        entry("diff", MatchMode::Prefix),
        entry("stat", MatchMode::Prefix),
        entry("file", MatchMode::Prefix),
        entry("which", MatchMode::Prefix),
        entry("command -v", MatchMode::Prefix),
        entry("pwd", MatchMode::Prefix),
        entry("date", MatchMode::Prefix),
        entry("id", MatchMode::Prefix),
        entry("whoami", MatchMode::Prefix),
        entry("uname", MatchMode::Prefix),
        entry("test", MatchMode::Prefix),
        entry("[", MatchMode::Prefix),
        entry("sleep", MatchMode::Prefix),
        entry("true", MatchMode::Prefix),
        entry("false", MatchMode::Prefix),
        entry("git status", MatchMode::Prefix),
        entry("git diff", MatchMode::Prefix),
        entry("git log", MatchMode::Prefix),
        entry("git branch", MatchMode::Prefix),
        entry("git show", MatchMode::Prefix),
        entry("git rev-parse", MatchMode::Prefix),
        entry("git config --get", MatchMode::Prefix),
        entry("git config --list", MatchMode::Prefix),
        entry("git remote", MatchMode::Prefix),
        entry("jj status", MatchMode::Prefix),
        entry("jj diff", MatchMode::Prefix),
        entry("jj log", MatchMode::Prefix),
        entry("jj show", MatchMode::Prefix),
        entry("jj file", MatchMode::Prefix),
        entry("jq", MatchMode::Prefix),
        entry("jaq", MatchMode::Prefix),
        entry("sed -n", MatchMode::Prefix),
        entry("awk", MatchMode::Prefix),
    ]
}

fn workspace_write_allow() -> Vec<CommandEntry> {
    vec![
        entry("cargo", MatchMode::Prefix),
        entry("rustc", MatchMode::Prefix),
        entry("make", MatchMode::Prefix),
        entry("just", MatchMode::Prefix),
        entry("npm", MatchMode::Prefix),
        entry("npx", MatchMode::Prefix),
        entry("pnpm", MatchMode::Prefix),
        entry("yarn", MatchMode::Prefix),
        entry("node", MatchMode::Prefix),
        entry("python", MatchMode::Prefix),
        entry("python3", MatchMode::Prefix),
        entry("pip", MatchMode::Prefix),
        entry("uv", MatchMode::Prefix),
        entry("go", MatchMode::Prefix),
        entry("gcc", MatchMode::Prefix),
        entry("clang", MatchMode::Prefix),
        entry("nix build", MatchMode::Prefix),
        entry("nix flake", MatchMode::Prefix),
        entry("nix eval", MatchMode::Prefix),
        entry("nixfmt", MatchMode::Prefix),
        entry("alejandra", MatchMode::Prefix),
        entry("nix-instantiate", MatchMode::Prefix),
        entry("pytest", MatchMode::Prefix),
        entry("ruff", MatchMode::Prefix),
        entry("mypy", MatchMode::Prefix),
        entry("shellcheck", MatchMode::Prefix),
        entry("shfmt", MatchMode::Prefix),
        entry("tsc", MatchMode::Prefix),
        entry("eslint", MatchMode::Prefix),
        entry("prettier", MatchMode::Prefix),
        entry("mkdir", MatchMode::Prefix),
        entry("touch", MatchMode::Prefix),
        entry("cp", MatchMode::Prefix),
        entry("mv", MatchMode::Prefix),
        entry("ln", MatchMode::Prefix),
        entry("tee", MatchMode::Prefix),
        entry("sed -i", MatchMode::Prefix),
        entry("cd", MatchMode::Prefix),
        entry("curl", MatchMode::Prefix),
        entry("wget", MatchMode::Prefix),
    ]
}

fn entry(match_str: &str, mode: MatchMode) -> CommandEntry {
    CommandEntry {
        match_str: match_str.into(),
        mode,
    }
}

#[must_use]
pub fn resolve(config: &lofi_types::ShellPolicyConfig) -> ResolvedPolicy {
    let (mut allow, mut ask, mut deny) = match config.mode {
        ShellPolicyMode::ReadOnly => (read_only_allow(), destructive_ask(), {
            let mut d = universal_deny();
            d.extend(workspace_write_allow().into_iter().filter(|e| {
                !matches!(
                    e.match_str.as_str(),
                    "cd" | "sleep" | "true" | "false" | "test" | "["
                )
            }));
            d
        }),
        ShellPolicyMode::WorkspaceWrite => {
            let mut a = read_only_allow();
            a.extend(workspace_write_allow());
            (a, destructive_ask(), universal_deny())
        }
        ShellPolicyMode::Unrestricted => (Vec::new(), Vec::new(), universal_deny()),
    };

    allow.extend(config.allow.clone());
    ask.extend(config.ask.clone());
    deny.extend(config.deny.clone());

    let mut wrappers = standard_wrappers();
    wrappers.extend(config.wrappers.clone());
    let wrapper_map = build_wrapper_map(&wrappers);

    let allow_by_default = matches!(config.mode, ShellPolicyMode::Unrestricted);

    ResolvedPolicy {
        allow,
        ask,
        deny,
        wrappers: wrapper_map,
        redirects: config.redirects.clone(),
        heredocs: config.heredocs.clone(),
        yolo: config.yolo,
        allow_by_default,
    }
}
