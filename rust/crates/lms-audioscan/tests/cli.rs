use lofty::config::WriteOptions;
use lofty::id3::v2::Id3v2Tag;
use lofty::picture::MimeType;
use lofty::picture::Picture;
use lofty::picture::PictureType;
use lofty::tag::Accessor;
use lofty::tag::TagExt;
use std::path::Path;
use std::process::Command;

fn write_id3v2_mp3_with_apic(path: &Path, title: &str, artist: &str, jpeg: &[u8]) {
    let mut frame = vec![0u8; 417];
    frame[0] = 0xFF;
    frame[1] = 0xFB;
    frame[2] = 0x90;
    frame[3] = 0x00;
    std::fs::write(path, frame.repeat(20)).unwrap();

    let mut tag = Id3v2Tag::default();
    tag.set_title(title.to_string());
    tag.set_artist(artist.to_string());
    let pic = Picture::new_unchecked(
        PictureType::CoverFront,
        Some(MimeType::Jpeg),
        Some("cover".to_string()),
        jpeg.to_vec(),
    );
    tag.insert_picture(pic);
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

#[test]
fn test_cli_binary_execution() {
    let dir = tempfile::tempdir().unwrap();
    let mp3_path = dir.path().join("cli_test.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
    write_id3v2_mp3_with_apic(&mp3_path, "CLI Title", "CLI Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");

    // Test scan with AUDIO_SCAN_NO_ARTWORK=1.
    let output = Command::new(bin_path)
        .args(["scan", mp3_path.to_str().unwrap()])
        .env("AUDIO_SCAN_NO_ARTWORK", "1")
        .output()
        .unwrap();
    assert!(output.status.success());
    let val: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["tags"]["TIT2"], "CLI Title");
    assert_eq!(val["tags"]["APIC"][3], 10);
    assert_eq!(val["info"]["samplerate"], 44100);

    // Test scan-tags with AUDIO_SCAN_NO_ARTWORK=0.
    let output_tags = Command::new(bin_path)
        .args(["scan-tags", mp3_path.to_str().unwrap()])
        .env("AUDIO_SCAN_NO_ARTWORK", "0")
        .output()
        .unwrap();
    assert!(output_tags.status.success());
    let val_tags: serde_json::Value = serde_json::from_slice(&output_tags.stdout).unwrap();
    assert!(val_tags["info"].as_object().unwrap().is_empty());
    assert!(val_tags["tags"]["APIC"][3].is_string());

    // Test find-frame.
    let output_frame = Command::new(bin_path)
        .args(["find-frame", "mp3", mp3_path.to_str().unwrap(), "0"])
        .output()
        .unwrap();
    assert!(output_frame.status.success());
    let val_frame: serde_json::Value = serde_json::from_slice(&output_frame.stdout).unwrap();
    assert!(val_frame["byte_offset"].as_i64().unwrap() > 0);
}

#[test]
fn test_cli_help_and_flag_errors() {
    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");

    for flag in ["--help", "-h"] {
        let out = Command::new(bin_path).arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("Usage: lms-audioscan"));
        assert!(stdout.contains("--no-artwork"));
        assert!(stdout.contains("--info-only"));
    }

    // No arguments prints usage and exits non-zero.
    let no_args = Command::new(bin_path).output().unwrap();
    assert!(!no_args.status.success());

    // Unknown flags and unknown subcommands fail with non-zero exit.
    for bogus in ["--bogus-flag", "--bogus", "-x"] {
        let unknown_flag = Command::new(bin_path)
            .args([bogus, "scan", "missing.mp3"])
            .output()
            .unwrap();
        assert!(!unknown_flag.status.success(), "{bogus} should fail");
        assert!(
            String::from_utf8_lossy(&unknown_flag.stderr).contains("Unknown option"),
            "{bogus} should report unknown option"
        );
    }

    let unknown_cmd = Command::new(bin_path).arg("bogus-cmd").output().unwrap();
    assert!(!unknown_cmd.status.success());
}

#[test]
fn test_cli_negative_find_frame_offset_without_double_dash() {
    let dir = tempfile::tempdir().unwrap();
    let mp3_path = dir.path().join("neg_offset.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
    write_id3v2_mp3_with_apic(&mp3_path, "Neg Offset", "Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");

    // Negative signed 64-bit offsets must pass through without '--'.
    for offset in ["-500", "-1"] {
        let out = Command::new(bin_path)
            .args(["find-frame", "mp3", mp3_path.to_str().unwrap(), offset])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "find-frame with {offset} failed: {stderr}"
        );
        assert!(!stderr.contains("Unknown option"));
        let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let min_offset = offset.trim_start_matches('-').parse::<i64>().unwrap();
        assert!(val["byte_offset"].as_i64().unwrap() >= min_offset);
        assert_eq!(val["samplerate"], 44100);
    }

    // Unknown flags positioned like offsets still fail with Unknown option.
    for bad_flag in ["--bogus", "-x"] {
        let out = Command::new(bin_path)
            .args(["find-frame", "mp3", mp3_path.to_str().unwrap(), bad_flag])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{bad_flag} should fail");
        assert!(String::from_utf8_lossy(&out.stderr).contains("Unknown option"));
    }
}

#[test]
fn test_cli_help_filename_and_double_dash() {
    let dir = tempfile::tempdir().unwrap();
    let help_mp3 = dir.path().join("help.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
    write_id3v2_mp3_with_apic(&help_mp3, "Help Track", "Help Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");

    // Existing file named help.mp3 should scan, not print help.
    let ok_help_file = Command::new(bin_path)
        .args(["scan", help_mp3.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(ok_help_file.status.success());
    let stdout = String::from_utf8_lossy(&ok_help_file.stdout);
    assert!(!stdout.contains("Usage: lms-audioscan"));
    let val: serde_json::Value = serde_json::from_slice(&ok_help_file.stdout).unwrap();
    assert_eq!(val["tags"]["TIT2"], "Help Track");

    // Missing file named help.mp3 should fail rather than printing help.
    let missing_help = dir.path().join("missing_help.mp3");
    let fail_help = Command::new(bin_path)
        .args(["scan", missing_help.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!fail_help.status.success());
    assert!(!String::from_utf8_lossy(&fail_help.stdout).contains("Usage: lms-audioscan"));

    // Flag-like argument after '--' must be treated as a path, not help.
    let after_dd = Command::new(bin_path)
        .current_dir(dir.path())
        .args(["scan", "--", "--help"])
        .output()
        .unwrap();
    assert!(!after_dd.status.success());
    assert!(!String::from_utf8_lossy(&after_dd.stdout).contains("Usage: lms-audioscan"));
}

#[test]
fn test_cli_missing_corrupt_and_extra_args() {
    let dir = tempfile::tempdir().unwrap();
    let valid_mp3 = dir.path().join("valid.mp3");
    let corrupt_mp3 = dir.path().join("corrupt.mp3");
    let missing_mp3 = dir.path().join("does_not_exist.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
    write_id3v2_mp3_with_apic(&valid_mp3, "Title", "Artist", &fake_jpeg);
    std::fs::write(&corrupt_mp3, b"not a valid audio file at all").unwrap();

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");

    let missing_out = Command::new(bin_path)
        .args(["scan", missing_mp3.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!missing_out.status.success());

    let corrupt_out = Command::new(bin_path)
        .args(["scan", corrupt_mp3.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!corrupt_out.status.success());

    // Extra positional arguments must be rejected for all subcommands.
    let extra_scan = Command::new(bin_path)
        .args(["scan", valid_mp3.to_str().unwrap(), "extra"])
        .output()
        .unwrap();
    assert!(!extra_scan.status.success());

    let extra_tags = Command::new(bin_path)
        .args(["scan-tags", valid_mp3.to_str().unwrap(), "extra"])
        .output()
        .unwrap();
    assert!(!extra_tags.status.success());

    let extra_frame = Command::new(bin_path)
        .args([
            "find-frame",
            "mp3",
            valid_mp3.to_str().unwrap(),
            "0",
            "extra",
        ])
        .output()
        .unwrap();
    assert!(!extra_frame.status.success());
}

#[test]
fn test_cli_no_artwork_and_info_only_flags() {
    let dir = tempfile::tempdir().unwrap();
    let mp3_path = dir.path().join("flags.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
    write_id3v2_mp3_with_apic(&mp3_path, "Flags Title", "Flags Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");

    // --no-artwork omits artwork payload even when AUDIO_SCAN_NO_ARTWORK=0.
    let out_no_art = Command::new(bin_path)
        .args(["scan", "--no-artwork", mp3_path.to_str().unwrap()])
        .env("AUDIO_SCAN_NO_ARTWORK", "0")
        .output()
        .unwrap();
    assert!(out_no_art.status.success());
    let val_no_art: serde_json::Value = serde_json::from_slice(&out_no_art.stdout).unwrap();
    assert_eq!(val_no_art["tags"]["APIC"][3], 10);
    assert_eq!(val_no_art["info"]["samplerate"], 44100);

    // --info-only returns info and omits tags.
    let out_info = Command::new(bin_path)
        .args(["--info-only", "scan", mp3_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out_info.status.success());
    let val_info: serde_json::Value = serde_json::from_slice(&out_info.stdout).unwrap();
    assert_eq!(val_info["info"]["samplerate"], 44100);
    assert!(val_info["tags"].as_object().unwrap().is_empty());

    // --info-only is rejected on incompatible subcommands.
    let bad_info_tags = Command::new(bin_path)
        .args(["scan-tags", "--info-only", mp3_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!bad_info_tags.status.success());
}

#[cfg(unix)]
#[test]
fn test_cli_non_utf8_unix_path_no_panic() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().unwrap();
    let non_utf8_name = OsString::from_vec(b"track_\xff_\xfe.mp3".to_vec());
    let mp3_path = dir.path().join(non_utf8_name);
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
    write_id3v2_mp3_with_apic(&mp3_path, "NonUTF8 Title", "Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let out = Command::new(bin_path)
        .arg("scan")
        .arg(&mp3_path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["tags"]["TIT2"], "NonUTF8 Title");

    let bad_cmd = OsString::from_vec(b"cmd_\xff".to_vec());
    let err_out = Command::new(bin_path).arg(bad_cmd).output().unwrap();
    assert!(!err_out.status.success());
    assert!(!String::from_utf8_lossy(&err_out.stderr).contains("panicked"));
}
