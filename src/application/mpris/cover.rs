//! Cover art for `mpris:artUrl`: a cheaply comparable image handle and the
//! PNG file a desktop shell reads it from.

use std::fmt;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::{DynamicImage, ImageFormat};

pub const COVER_EDGE: u32 = 512;

/// Equal only to its own clones, so a `Snapshot` stays `PartialEq`.
#[derive(Clone)]
pub struct CoverImage(pub Arc<DynamicImage>);

impl PartialEq for CoverImage {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for CoverImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CoverImage({}x{})", self.0.width(), self.0.height())
    }
}

/// Writes `cover-<pid>-<seq>.png`: the pid keeps URLs distinct between runs
/// and keeps two instances sharing a cache folder apart.
pub fn write_cover(dir: &Path, seq: u64, image: &DynamicImage) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let small;
    let image = if image.width() > COVER_EDGE || image.height() > COVER_EDGE {
        small = image.thumbnail(COVER_EDGE, COVER_EDGE);
        &small
    } else {
        image
    };
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(io::Error::other)?;
    let pid = std::process::id();
    let path = dir.join(format!("cover-{pid}-{seq}.png"));
    let tmp = dir.join(format!("cover-{pid}-{seq}.png.tmp"));
    std::fs::write(&tmp, png.into_inner())?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Removes cover files left by earlier runs: ours, and those of processes
/// that no longer exist. A live process's files stay. Names without a pid
/// (the pre-pid format) go too.
pub fn clear_covers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let own = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("cover-") else {
            continue;
        };
        if !(name.ends_with(".png") || name.ends_with(".png.tmp")) {
            continue;
        }
        let pid = rest.split('-').next().and_then(|p| p.parse::<u32>().ok());
        if pid.is_none_or(|pid| pid == own || !process_alive(pid)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Off Linux there is no `/proc`: "can't tell" counts as not alive.
fn process_alive(pid: u32) -> bool {
    cfg!(target_os = "linux") && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::DynamicImage;

    fn img(w: u32, h: u32) -> DynamicImage {
        DynamicImage::new_rgb8(w, h)
    }

    #[test]
    fn cover_images_compare_by_pointer() {
        let a = CoverImage(Arc::new(img(2, 2)));
        let b = CoverImage(Arc::new(img(2, 2)));
        assert_ne!(a, b);
        assert_eq!(a, a.clone());
    }

    #[test]
    fn write_cover_caps_the_long_edge_and_keeps_aspect() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cover(dir.path(), 1, &img(2000, 1000)).unwrap();
        let read = image::open(&path).unwrap();
        assert_eq!((read.width(), read.height()), (512, 256));
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn write_cover_keeps_a_small_image_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cover(dir.path(), 2, &img(100, 80)).unwrap();
        let read = image::open(path).unwrap();
        assert_eq!((read.width(), read.height()), (100, 80));
    }

    #[test]
    fn clear_covers_removes_only_cover_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["cover-3.png", "cover-4.png.tmp", "keep.txt"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        clear_covers(dir.path());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["keep.txt"]);
    }

    #[test]
    fn cover_files_carry_the_pid_so_urls_differ_between_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cover(dir.path(), 1, &img(4, 4)).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("cover-{}-1.png", std::process::id()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clear_covers_spares_a_live_process_and_removes_dead_and_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let own = std::process::id();
        // Pid 1 is alive; u32::MAX is never a pid.
        let names = [
            "cover-1-1.png".to_string(),
            "cover-1-2.png.tmp".to_string(),
            format!("cover-{}-1.png", u32::MAX),
            format!("cover-{own}-7.png"),
            "keep.txt".to_string(),
        ];
        for name in &names {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        clear_covers(dir.path());
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["cover-1-1.png", "cover-1-2.png.tmp", "keep.txt"]);
    }
}
