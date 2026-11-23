#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::{Component, Path};

fn check_no_escape(root_canon: &Path, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        assert!(
            !path.components().any(|c| matches!(c, Component::ParentDir)),
            "unpacked path contains ..: {}",
            path.display()
        );
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_dir() {
            if let Ok(canon) = std::fs::canonicalize(&path) {
                assert!(
                    canon.starts_with(root_canon),
                    "unpacked directory escaped root: {}",
                    canon.display()
                );
            }
            check_no_escape(root_canon, &path);
        } else if let Some(parent) = path.parent()
            && let Ok(canon) = std::fs::canonicalize(parent)
        {
            assert!(
                canon.starts_with(root_canon),
                "unpacked leaf parent escaped root: {}",
                canon.display()
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 * 1024 {
        return;
    }

    let Ok(dir) = tempfile::tempdir() else {
        return;
    };
    let archive = dir.path().join("fuzz.tar");
    if std::fs::write(&archive, data).is_err() {
        return;
    }
    let out = dir.path().join("out");

    if cuma_sandbox::sync::unpack(&archive, &out).is_ok() {
        if let Ok(root_canon) = std::fs::canonicalize(&out) {
            check_no_escape(&root_canon, &out);
        }
    }
});
