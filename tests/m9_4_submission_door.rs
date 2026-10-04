//! M9.4: production code reaches the engine only through
//! `EngineHandle::submit`, which applies each command's out-of-band rule.
//! The raw `commands()` and `wake()` senders skip those rules and are a test
//! seam only.

use std::path::Path;

#[allow(clippy::unwrap_used)] // A bare helper: an unreadable source tree is a test failure.
fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn production_code_never_uses_the_raw_engine_senders() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut offenders = Vec::new();
    for file in files
        .iter()
        .filter(|file| !file.ends_with("playback/engine.rs"))
    {
        let text = std::fs::read_to_string(file).unwrap();
        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default();
            if ["engine", "handle"].iter().any(|name| {
                code.contains(&format!("{name}.commands()"))
                    || code.contains(&format!("{name}.wake()"))
            }) {
                offenders.push(format!(
                    "{}:{}: {}",
                    file.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "use EngineHandle::submit:\n{}",
        offenders.join("\n")
    );
}
