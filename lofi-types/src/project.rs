//! Where a project starts. One marker list decides both the boundary the
//! agent reads AGENTS.md instructions from and the directory a file-grant
//! prompt offers, so a request outside the workspace asks for the same unit.

use std::path::{Path, PathBuf};

/// Version-control roots. A checkout is a project boundary even when it holds
/// no manifest of its own, so a grant prefers one wherever it sits.
const VCS_MARKERS: &[&str] = &[
    ".git",
    ".jj",
    ".hg",
    ".svn",
    ".sl",
    ".pijul",
    "_darcs",
    ".bzr",
    "_FOSSIL_",
    ".fslckout",
];

/// Build and workspace manifests, used only when no version-control marker is
/// in the ancestor chain. The nearest directory wins, so the order here only
/// decides which name is reported when one directory holds several.
const MANIFEST_MARKERS: &[&str] = &[
    "Cargo.toml",
    "go.mod",
    "go.work",
    "package.json",
    "deno.json",
    "deno.jsonc",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradlew",
    "build.sbt",
    "Gemfile",
    "composer.json",
    "mix.exs",
    "pubspec.yaml",
    "Package.swift",
    "CMakeLists.txt",
    "meson.build",
    "configure.ac",
    "MODULE.bazel",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "stack.yaml",
    "cabal.project",
    "dune-project",
    "Project.toml",
    "DESCRIPTION",
    "flake.nix",
];

/// Manifests named by extension. A suffix match needs a directory listing, so
/// these are checked only when no name above matched.
const MANIFEST_SUFFIXES: &[&str] = &[
    ".gemspec",
    ".xcodeproj",
    ".xcworkspace",
    ".sln",
    ".csproj",
    ".fsproj",
    ".cabal",
];

/// A project root and the marker that named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    pub dir: PathBuf,
    /// Shown in a grant prompt, so the user sees why this directory.
    pub marker: &'static str,
}

/// Which marker decides the root.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tiers {
    /// Any marker counts and the nearest ancestor wins, whichever kind it is:
    /// the boundary of a near project is not widened by a checkout above it.
    Any,
    /// A version-control root wins wherever it sits, and a manifest is the
    /// fallback for a tree with no checkout marker.
    VcsFirst,
}

/// The nearest ancestor holding a marker, of either kind. The start directory
/// itself counts.
#[must_use]
pub fn nearest_any(start: &Path) -> Option<ProjectRoot> {
    walk(start, |_| true, Tiers::Any)
}

/// [`nearest_any`] bounded below the filesystem root and the home directory, and
/// preferring the checkout a manifest sits in. A stray marker in the home
/// directory must not turn one request into a grant for the whole home tree, so
/// neither the home directory nor an ancestor of it is a candidate. The bound
/// is compared with a canonical request, so it must be canonical too.
#[must_use]
pub fn nearest_below(start: &Path, home: Option<&Path>) -> Option<ProjectRoot> {
    walk(
        start,
        |dir| dir.parent().is_some() && home.is_none_or(|home| !home.starts_with(dir)),
        Tiers::VcsFirst,
    )
}

fn walk(start: &Path, allowed: impl Fn(&Path) -> bool, tiers: Tiers) -> Option<ProjectRoot> {
    let mut manifest: Option<ProjectRoot> = None;
    for dir in start.ancestors() {
        // A rejected directory rejects every ancestor above it as well.
        if !allowed(dir) {
            break;
        }
        match tiers {
            Tiers::Any => {
                if let Some(marker) = vcs_marker(dir).or_else(|| manifest_marker(dir)) {
                    return Some(ProjectRoot {
                        dir: dir.to_path_buf(),
                        marker,
                    });
                }
            }
            Tiers::VcsFirst => {
                if let Some(marker) = vcs_marker(dir) {
                    return Some(ProjectRoot {
                        dir: dir.to_path_buf(),
                        marker,
                    });
                }
                if manifest.is_none() {
                    manifest = manifest_marker(dir).map(|marker| ProjectRoot {
                        dir: dir.to_path_buf(),
                        marker,
                    });
                }
            }
        }
    }
    manifest
}

fn vcs_marker(dir: &Path) -> Option<&'static str> {
    // .git is a directory in a clone and a file in a linked worktree, so a
    // marker is detected by existence, not by kind.
    VCS_MARKERS
        .iter()
        .copied()
        .find(|marker| dir.join(marker).exists())
}

fn manifest_marker(dir: &Path) -> Option<&'static str> {
    if let Some(marker) = MANIFEST_MARKERS
        .iter()
        .copied()
        .find(|marker| dir.join(marker).exists())
    {
        return Some(marker);
    }
    MANIFEST_SUFFIXES
        .iter()
        .copied()
        .find(|suffix| holds_entry_ending_with(dir, suffix))
}

#[cfg(test)]
mod tests;

/// A named manifest needs a stem in front of its suffix: cabal-install creates
/// `~/.cabal`, and a bare dot-directory is not a project.
fn holds_entry_ending_with(dir: &Path, suffix: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.len() > suffix.len() && name.ends_with(suffix))
    })
}
