//! Received files: sanitised names, `.part` staging, and no silent overwrite.
//!
//! The phone sends whatever name its viewer showed, which can contain slashes or leading dots; the
//! host decides the final path. A transfer that breaks halfway leaves only a hidden `.part` file.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub struct IncomingFile {
    folder: PathBuf,
    name: String,
    tmp: PathBuf,
    out: BufWriter<File>,
}

impl IncomingFile {
    pub fn create(folder: &Path, raw_name: &str) -> Result<Self> {
        fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
        let name = sanitize(raw_name);
        let tmp = folder.join(format!(".tapdrop-{}.part", uuid::Uuid::new_v4()));
        let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        Ok(Self {
            folder: folder.to_path_buf(),
            name,
            tmp,
            out: BufWriter::new(file),
        })
    }

    #[cfg(test)]
    pub fn transfer(&self) -> &Path {
        &self.tmp
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.out.write_all(data)?;
        Ok(())
    }

    /// Moves the finished file into place under a name that is free.
    pub fn commit(mut self) -> Result<PathBuf> {
        self.out.flush()?;
        self.out.get_ref().sync_all()?;
        let dest = unique_path(&self.folder, &self.name);
        fs::rename(&self.tmp, &dest).with_context(|| format!("moving into {}", dest.display()))?;
        Ok(dest)
    }
}

impl Drop for IncomingFile {
    fn drop(&mut self) {
        // `commit` renames the temporary file away; if it is still here, the transfer did not finish.
        if self.tmp.exists() {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

/// Keeps one path component: no separators, no control characters, no leading dot or space.
pub fn sanitize(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| match c {
            '/' | ':' | '\\' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    s = s.trim_start_matches(['.', ' ', '_']).to_string();
    if s.is_empty() {
        s = "file".to_string();
    }
    if s.chars().count() > 200 {
        let suffix = Path::new(&s)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        s = format!("{}{}", s.chars().take(180).collect::<String>(), suffix);
    }
    s
}

/// `name.ext`, then `name (1).ext`, `name (2).ext`…
pub fn unique_path(folder: &Path, name: &str) -> PathBuf {
    let candidate = folder.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..10_000 {
        let candidate = folder.join(format!("{stem} ({i}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    folder.join(format!("{stem}-{}{ext}", uuid::Uuid::new_v4()))
}

pub fn mime_for(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

pub fn size_of(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_the_folder() {
        assert_eq!(sanitize("../../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize("/tmp/x.png"), "tmp_x.png");
        assert_eq!(sanitize(".."), "file");
        assert_eq!(sanitize(""), "file");
        assert_eq!(sanitize("photo.jpg"), "photo.jpg");
    }

    #[test]
    fn unique_names_keep_the_extension() {
        let dir = std::env::temp_dir().join(format!("tapdrop-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt").file_name().unwrap(), "a (1).txt");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_partial_transfer_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("tapdrop-test-{}", uuid::Uuid::new_v4()));
        let mut file = IncomingFile::create(&dir, "clip.png").unwrap();
        file.write(b"half").unwrap();
        let tmp = file.transfer().to_path_buf();
        assert!(tmp.exists());
        drop(file);
        assert!(!tmp.exists(), "an unfinished transfer leaves no file");
        fs::remove_dir_all(&dir).unwrap();
    }
}
