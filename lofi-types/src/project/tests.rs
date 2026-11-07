//! The marker walk, over real directories: which project a file belongs to,
//! what a checkout wins over, where the home bound stops the walk, and the
//! manifest fallback. Every unbounded walk starts inside a temporary directory
//! that may sit under a real project, so these walks are bounded by it.

use std::path::{Path, PathBuf};

use super::{
    nearest_any, nearest_below, walk, ProjectRoot, Tiers, MANIFEST_MARKERS, MANIFEST_SUFFIXES,
    VCS_MARKERS,
};

fn dirs(paths: &[&Path]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| {
            std::fs::create_dir_all(path).unwrap();
            path.to_path_buf()
        })
        .collect()
}

fn marker(dir: &Path, name: &str) {
    std::fs::write(dir.join(name), "").unwrap();
}

/// The walk confined to one temporary tree, with the grant's tier order.
fn under(start: &Path, tmp: &Path) -> Option<ProjectRoot> {
    walk(start, |dir| dir.starts_with(tmp), Tiers::VcsFirst)
}

#[test]
fn every_listed_marker_names_its_own_directory() {
    for name in VCS_MARKERS.iter().chain(MANIFEST_MARKERS) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("project");
        dirs(&[&dir]);
        marker(&dir, name);
        let root = under(&dir, tmp.path()).unwrap_or_else(|| panic!("{name} was not recognized"));
        assert_eq!(root.dir, dir);
        assert_eq!(root.marker, *name);
    }
    for suffix in MANIFEST_SUFFIXES {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("project");
        dirs(&[&dir]);
        marker(&dir, &format!("Demo{suffix}"));
        let root = under(&dir, tmp.path()).unwrap_or_else(|| panic!("{suffix} was not recognized"));
        assert_eq!(root.marker, *suffix);
    }
}

#[test]
fn a_bare_dot_entry_is_not_a_manifest() {
    // cabal-install creates ~/.cabal, and a marker there would make the whole
    // home tree a project for the unbounded walk.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("plain");
    dirs(&[&dir, &dir.join(".cabal")]);
    assert!(under(&dir, tmp.path()).is_none());
}

#[test]
fn a_checkout_marker_names_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let deep = repo.join("a/b/c");
    dirs(&[&deep]);
    marker(&repo, ".jj");
    marker(&deep, "Cargo.toml");
    let root = under(&deep, tmp.path()).unwrap();
    assert_eq!(root.dir, repo);
    assert_eq!(root.marker, ".jj");
}

#[test]
fn one_directory_with_two_markers_reports_the_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    dirs(&[&repo]);
    marker(&repo, "Cargo.toml");
    marker(&repo, ".git");
    assert_eq!(under(&repo, tmp.path()).unwrap().marker, ".git");
    assert_eq!(nearest_any(&repo).unwrap().marker, ".git");
}

#[test]
fn a_nested_manifest_does_not_beat_the_checkout_root() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    let member = workspace.join("member/src");
    dirs(&[&member]);
    marker(&workspace, "Cargo.toml");
    marker(&workspace, ".git");
    marker(&member, "Cargo.toml");
    assert_eq!(under(&member, tmp.path()).unwrap().dir, workspace);
}

#[test]
fn the_nearest_manifest_stands_in_for_a_missing_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let member = project.join("crate");
    dirs(&[&member]);
    marker(&project, "Cargo.toml");
    marker(&member, "pyproject.toml");
    let root = under(&member, tmp.path()).unwrap();
    assert_eq!(root.dir, member);
    assert_eq!(root.marker, "pyproject.toml");
    // Without the nearer manifest the manifest above it names the root.
    std::fs::remove_file(member.join("pyproject.toml")).unwrap();
    assert_eq!(under(&member, tmp.path()).unwrap().dir, project);
}

#[test]
fn the_nearest_project_wins_over_a_checkout_above_it() {
    // The AGENTS.md boundary stops at the nearest project of either kind; a
    // checkout above a nearer manifest does not reach down into it.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = home.join("work/project");
    let src = project.join("src");
    dirs(&[&src]);
    marker(&home, ".git");
    marker(&project, "Cargo.toml");
    assert_eq!(nearest_any(&src).unwrap().dir, project);
    // A grant prefers the checkout, and the home bound stops it there.
    assert_eq!(nearest_below(&src, Some(&home)).unwrap().dir, project);
    assert!(nearest_below(&src, None).unwrap().dir.ends_with("home"));
}

#[test]
fn a_plain_directory_has_no_root() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    dirs(&[&plain]);
    assert!(under(&plain, tmp.path()).is_none());
}

#[test]
fn a_marker_in_the_home_directory_is_not_a_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = home.join("dev/project/src");
    dirs(&[&work]);
    marker(&home, "flake.nix");
    let home = home.canonicalize().unwrap();
    // A stray manifest in the home directory names it for the unbounded walk,
    // and the bound rejects it instead of granting the whole home tree.
    assert_eq!(under(&work, tmp.path()).unwrap().dir, home);
    assert!(nearest_below(&work, Some(&home)).is_none());
    marker(&home.join("dev/project"), ".git");
    let bounded = nearest_below(&work, Some(&home)).unwrap();
    assert_eq!(bounded.dir, home.join("dev/project"));
}

#[test]
fn an_ancestor_of_home_is_not_a_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let src = tmp.path().join("work/src");
    dirs(&[&home, &src]);
    // The only marker sits at the temporary root, above the home directory.
    marker(tmp.path(), ".jj");
    assert_eq!(nearest_any(&src).unwrap().dir, tmp.path());
    assert!(nearest_below(&src, Some(&home)).is_none());
}
