fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for name in ["ffmpeg.exe", "ffprobe.exe", "LICENSE", "README.txt"] {
            let path = std::path::Path::new("resources/ffmpeg").join(name);
            assert!(
                path.is_file(),
                "missing {}; run scripts/prepare-ffmpeg-windows.ps1 first",
                path.display()
            );
        }
    }
    tauri_build::build()
}
