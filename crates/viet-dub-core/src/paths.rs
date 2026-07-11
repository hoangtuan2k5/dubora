use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const VIDEO_EXTENSIONS: [&str; 4] = ["mp4", "mov", "mkv", "webm"];

/// Relative directories every project root must have for the desktop app / pipeline.
pub const PROJECT_LAYOUT_DIRS: [&str; 4] = [
    "assets/video",
    "assets/srt",
    "work",
    "outputs",
];

/// Create the standard layout under `root` if missing.
///
/// Returns the relative paths that were newly created (empty if all existed).
pub fn ensure_project_layout(root: &Path) -> Result<Vec<String>> {
    if root.as_os_str().is_empty() {
        bail!("Project root path is empty");
    }
    if root.exists() && !root.is_dir() {
        bail!(
            "Project root exists but is not a directory: {}",
            root.display()
        );
    }

    let mut created = Vec::new();
    for relative in PROJECT_LAYOUT_DIRS {
        let path = root.join(relative);
        if path.is_dir() {
            continue;
        }
        if path.exists() {
            bail!(
                "Expected a directory at {} but found a file",
                path.display()
            );
        }
        fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create {}", path.display()))?;
        created.push(relative.to_owned());
    }
    Ok(created)
}

pub fn list_videos(root: &Path) -> Result<Vec<PathBuf>> {
    let mut videos = Vec::new();
    collect_videos(&root.join("assets/video"), &mut videos)?;
    videos.sort();
    Ok(videos)
}

pub fn find_targets(root: &Path, target: Option<&Path>) -> Result<Vec<PathBuf>> {
    let Some(target) = target else {
        return list_videos(root);
    };

    if target.is_absolute() {
        if target.is_file() {
            return Ok(vec![target.canonicalize()?]);
        }
        if target.is_dir() {
            let mut videos = Vec::new();
            collect_videos(target, &mut videos)?;
            videos.sort();
            return Ok(videos);
        }
    }

    let candidate = root.join("assets/video").join(target);
    if candidate.is_file() {
        return Ok(vec![candidate.canonicalize()?]);
    }
    if candidate.is_dir() {
        let mut videos = Vec::new();
        collect_videos(&candidate, &mut videos)?;
        videos.sort();
        return Ok(videos);
    }

    let Some(name) = candidate.file_name() else {
        bail!("Target not found: {}", target.display());
    };
    let prefix = format!("{}.", name.to_string_lossy());
    let mut matches = Vec::new();
    if let Some(parent) = candidate.parent()
        && let Ok(entries) = fs::read_dir(parent)
    {
        for entry in entries {
            let path = entry?.path();
            if path.is_file()
                && path
                    .file_name()
                    .is_some_and(|value| value.to_string_lossy().starts_with(&prefix))
                && is_video(&path)
            {
                matches.push(path.canonicalize()?);
            }
        }
    }
    matches.sort();
    match matches.len() {
        0 => bail!("Target not found: {}", target.display()),
        1 => Ok(matches),
        _ => bail!(
            "Multiple video files match '{}': {matches:?}",
            target.display()
        ),
    }
}

pub fn video_relative_path(root: &Path, video: &Path) -> PathBuf {
    let video_dir = root.join("assets/video");
    let resolved_video = video.canonicalize().unwrap_or_else(|_| video.to_path_buf());
    let resolved_dir = video_dir.canonicalize().unwrap_or(video_dir);
    resolved_video
        .strip_prefix(resolved_dir)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| {
            video
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("video"))
        })
}

pub fn find_matching_srt(root: &Path, video: &Path) -> Result<PathBuf> {
    let relative = video_relative_path(root, video);
    let mut candidates = vec![
        root.join("assets/srt")
            .join(&relative)
            .with_extension("srt"),
    ];

    let mut parts = relative.components();
    if let Some(first) = parts.next() {
        let mut prefixed = PathBuf::from(format!("CC-{}", first.as_os_str().to_string_lossy()));
        prefixed.extend(parts);
        candidates.push(root.join("assets/srt").join(prefixed).with_extension("srt"));
    }

    if let Some(path) = candidates.iter().find(|path| path.is_file()) {
        return path
            .canonicalize()
            .with_context(|| format!("Failed to resolve {}", path.display()));
    }

    let tried = candidates
        .iter()
        .map(|path| format!("- {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "Missing matching SRT for assets/video/{}. Tried:\n{tried}",
        relative.display()
    )
}

pub fn safe_work_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter_map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_' | '.') {
                Some(character)
            } else if character.is_whitespace() {
                Some('_')
            } else {
                None
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', '_']);
    if cleaned.is_empty() {
        "job".to_owned()
    } else {
        cleaned.to_owned()
    }
}

pub fn safe_work_path(root: &Path, relative_video: &Path) -> PathBuf {
    let without_extension = relative_video.with_extension("");
    without_extension
        .components()
        .fold(root.join("work"), |path, part| {
            path.join(safe_work_name(&part.as_os_str().to_string_lossy()))
        })
}

pub fn output_path(root: &Path, video: &Path, relative_video: &Path) -> Result<PathBuf> {
    let stem = video
        .file_stem()
        .and_then(OsStr::to_str)
        .context("Video path has no filename")?;
    Ok(root
        .join("outputs")
        .join(relative_video)
        .with_file_name(format!("{stem}.vi-dub.mp4")))
}

fn collect_videos(directory: &Path, videos: &mut Vec<PathBuf>) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("Failed to read {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_videos(&path, videos)?;
        } else if path.is_file() && is_video(&path) {
            videos.push(path.canonicalize()?);
        }
    }
    Ok(())
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|expected| extension.eq_ignore_ascii_case(expected))
        })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn sanitizes_each_work_path_component() {
        let root = Path::new("/project");
        let relative = Path::new("[NEW] Khóa học/part 1/1. Intro!.mp4");
        assert_eq!(
            safe_work_path(root, relative),
            Path::new("/project/work/NEW_Khóa_học/part_1/1._Intro")
        );
        assert_eq!(safe_work_name("..."), "job");
    }

    #[test]
    fn scans_case_insensitively_and_finds_cc_srt() -> Result<()> {
        let root = temp_root();
        let video = root.join("assets/video/Course/Part/Lesson.MP4");
        let ignored = root.join("assets/video/Course/notes.txt");
        let srt = root.join("assets/srt/CC-Course/Part/Lesson.srt");
        for path in [&video, &ignored, &srt] {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, b"")?;
        }

        assert_eq!(list_videos(&root)?, vec![video.canonicalize()?]);
        assert_eq!(find_matching_srt(&root, &video)?, srt.canonicalize()?);
        assert!(find_targets(&root, Some(Path::new("Course/Part/Lesson")))?[0].is_absolute());

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn ensure_project_layout_creates_missing_dirs_once() -> Result<()> {
        let root = temp_root();
        assert!(!root.exists());

        let first = ensure_project_layout(&root)?;
        assert_eq!(
            first,
            vec![
                "assets/video".to_owned(),
                "assets/srt".to_owned(),
                "work".to_owned(),
                "outputs".to_owned(),
            ]
        );
        for relative in PROJECT_LAYOUT_DIRS {
            assert!(root.join(relative).is_dir());
        }

        let second = ensure_project_layout(&root)?;
        assert!(second.is_empty());

        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn temp_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("viet-dub-paths-{}-{unique}", std::process::id()))
    }
}
