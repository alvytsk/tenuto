use std::path::Path;

use tenuto::application::browse::{TreeCollected, collect_tree};
use tenuto::playlist::PlaylistId;

fn dest() -> PlaylistId {
    PlaylistId::from_raw_for_tests(1)
}

fn touch(root: &Path, relative: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|error| panic!("mkdir: {error}"));
    }
    std::fs::write(&path, b"").unwrap_or_else(|error| panic!("write: {error}"));
}

fn names(root: &Path, tree: &TreeCollected) -> Vec<String> {
    tree.items
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

#[test]
fn subdirectories_come_before_a_levels_own_files_each_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    for file in [
        "z.mp3",
        "B.flac",
        "a/2.mp3",
        "a/1.mp3",
        "a/deep/0.wav",
        "c/x.m4a",
        "notes.txt",
        "a/cover.jpg",
    ] {
        touch(dir.path(), file);
    }
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 4096);
    assert_eq!(
        names(dir.path(), &tree),
        [
            "a/deep/0.wav",
            "a/1.mp3",
            "a/2.mp3",
            "c/x.m4a",
            "B.flac",
            "z.mp3"
        ],
        "list_directory's order at every level: a/1.mp3 before z.mp3"
    );
    assert!(tree.unreadable.is_empty());
    assert!(!tree.scan_limit_reached);
    assert_eq!(tree.dest, dest());
}

#[test]
fn the_limit_keeps_the_earliest_candidates_in_walk_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    for file in ["z.mp3", "a/1.mp3", "a/2.mp3"] {
        touch(dir.path(), file);
    }
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 2);
    assert_eq!(names(dir.path(), &tree), ["a/1.mp3", "a/2.mp3"]);
    assert!(tree.scan_limit_reached);

    let exact = collect_tree(&[dir.path().to_path_buf()], dest(), 3);
    assert!(
        !exact.scan_limit_reached,
        "reaching the limit with nothing left unvisited is not a truncation"
    );
}

#[cfg(unix)]
#[test]
fn at_capacity_the_walk_stops_instead_of_reading_the_rest_of_the_tree() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "a/1.mp3");
    touch(dir.path(), "b/2.mp3");
    let later = dir.path().join("b");
    std::fs::set_permissions(&later, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 1);
    std::fs::set_permissions(&later, std::fs::Permissions::from_mode(0o755)).expect("chmod back");

    assert_eq!(names(dir.path(), &tree), ["a/1.mp3"]);
    assert!(
        tree.scan_limit_reached,
        "b was left unvisited, so files may exist there"
    );
    assert!(
        tree.unreadable.is_empty(),
        "b was never opened: a read attempt would have reported it (unless root)"
    );
}

#[test]
fn a_remaining_root_counts_as_unvisited_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "a/1.mp3");
    touch(dir.path(), "b/2.mp3");
    let tree = collect_tree(&[dir.path().join("a"), dir.path().join("b")], dest(), 1);
    assert_eq!(names(dir.path(), &tree), ["a/1.mp3"]);
    assert!(tree.scan_limit_reached);
}

#[test]
fn overlapping_roots_and_a_root_that_is_a_file_are_deduplicated() {
    let dir = tempfile::tempdir().expect("tempdir");
    for file in ["a/1.mp3", "a/2.mp3"] {
        touch(dir.path(), file);
    }
    let roots = [
        dir.path().join("a/2.mp3"),
        dir.path().to_path_buf(),
        dir.path().join("a"),
    ];
    let tree = collect_tree(&roots, dest(), 4096);
    assert_eq!(
        names(dir.path(), &tree),
        ["a/2.mp3", "a/1.mp3"],
        "roots in the order given; each file once"
    );
}

#[cfg(unix)]
#[test]
fn a_directory_symlink_is_skipped_so_a_cycle_cannot_recurse() {
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "a/1.mp3");
    std::os::unix::fs::symlink(dir.path(), dir.path().join("a/loop")).expect("symlink");
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 4096);
    assert_eq!(names(dir.path(), &tree), ["a/1.mp3"]);
}

#[cfg(unix)]
#[test]
fn a_directory_symlink_selected_as_a_root_is_not_traversed_either() {
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "real/1.mp3");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(dir.path().join("real"), &link).expect("symlink");
    let tree = collect_tree(&[link], dest(), 4096);
    assert!(tree.items.is_empty(), "{:?}", tree.items);
    assert!(tree.unreadable.is_empty(), "skipped, not failed");
}

#[cfg(unix)]
#[test]
fn a_file_symlink_alias_of_a_collected_file_is_not_added_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "a/1.mp3");
    std::os::unix::fs::symlink(dir.path().join("a/1.mp3"), dir.path().join("alias.mp3"))
        .expect("symlink");
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 4096);
    assert_eq!(tree.items.len(), 1, "{:?}", tree.items);
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_reported_and_the_walk_goes_on() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "locked/1.mp3");
    touch(dir.path(), "open/2.mp3");
    let locked = dir.path().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 4096);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
    if std::fs::read_dir(&locked).is_ok() && tree.unreadable.is_empty() {
        return; // running as root: nothing is unreadable
    }
    assert_eq!(tree.unreadable, [locked]);
    assert_eq!(names(dir.path(), &tree), ["open/2.mp3"]);
}
