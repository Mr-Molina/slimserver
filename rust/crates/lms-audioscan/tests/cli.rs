use lofty::config::WriteOptions;
use lofty::id3::v2::Id3v2Tag;
use lofty::picture::MimeType;
use lofty::picture::Picture;
use lofty::picture::PictureType;
use lofty::tag::Accessor;
use lofty::tag::TagExt;
use std::path::Path;
use std::process::Command;

fn cli_command(bin_path: &str) -> Command {
    let mut cmd = Command::new(bin_path);
    cmd.env_remove("AUDIO_SCAN_NO_ARTWORK");
    cmd
}

fn write_id3v2_mp3_with_apic(path: &Path, title: &str, artist: &str, jpeg: &[u8]) {
    let mut frame = vec![0u8; 417];
    frame[0] = 0xFF;
    frame[1] = 0xFB;
    frame[2] = 0x90;
    frame[3] = 0x00;
    std::fs::write(path, frame.repeat(20)).expect("write synthetic mp3 frames");

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
    tag.save_to_path(path, WriteOptions::default())
        .expect("save ID3v2 tag with APIC");
}

#[test]
fn test_cli_binary_execution() {
    let dir = tempfile::tempdir().expect("create tempdir for binary execution test");
    let mp3_path = dir.path().join("cli_test.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
    write_id3v2_mp3_with_apic(&mp3_path, "CLI Title", "CLI Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let mp3_str = mp3_path.to_str().expect("mp3_path is valid UTF-8");

    // Test scan with AUDIO_SCAN_NO_ARTWORK=1.
    let output = cli_command(bin_path)
        .args(["scan", mp3_str])
        .env("AUDIO_SCAN_NO_ARTWORK", "1")
        .output()
        .expect("execute lms-audioscan scan");
    assert!(output.status.success());
    let val: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse scan JSON output");
    assert_eq!(val["tags"]["TIT2"], "CLI Title");
    assert_eq!(val["tags"]["APIC"][3], 10);
    assert_eq!(val["info"]["samplerate"], 44100);

    // Test scan-tags with AUDIO_SCAN_NO_ARTWORK=0.
    let output_tags = cli_command(bin_path)
        .args(["scan-tags", mp3_str])
        .env("AUDIO_SCAN_NO_ARTWORK", "0")
        .output()
        .expect("execute lms-audioscan scan-tags");
    assert!(output_tags.status.success());
    let val_tags: serde_json::Value =
        serde_json::from_slice(&output_tags.stdout).expect("parse scan-tags JSON output");
    assert!(val_tags["info"]
        .as_object()
        .expect("info field is a JSON object")
        .is_empty());
    assert!(val_tags["tags"]["APIC"][3].is_string());

    // Test find-frame.
    let output_frame = cli_command(bin_path)
        .args(["find-frame", "mp3", mp3_str, "0"])
        .output()
        .expect("execute lms-audioscan find-frame");
    assert!(output_frame.status.success());
    let val_frame: serde_json::Value =
        serde_json::from_slice(&output_frame.stdout).expect("parse find-frame JSON output");
    assert!(
        val_frame["byte_offset"]
            .as_i64()
            .expect("byte_offset is i64")
            > 0
    );
}

#[test]
fn test_cli_help_and_flag_errors() {
    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");

    for flag in ["--help", "-h"] {
        let out = cli_command(bin_path)
            .arg(flag)
            .output()
            .expect("execute lms-audioscan help flag");
        assert!(out.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("Usage: lms-audioscan"));
        assert!(stdout.contains("--no-artwork"));
        assert!(stdout.contains("--info-only"));
    }

    // No arguments prints usage and exits non-zero.
    let no_args = cli_command(bin_path)
        .output()
        .expect("execute lms-audioscan with no args");
    assert!(!no_args.status.success());

    // Unknown flags and unknown subcommands fail with non-zero exit.
    for bogus in ["--bogus-flag", "--bogus", "-x"] {
        let unknown_flag = cli_command(bin_path)
            .args([bogus, "scan", "missing.mp3"])
            .output()
            .expect("execute lms-audioscan with unknown flag");
        assert!(!unknown_flag.status.success(), "{bogus} should fail");
        assert!(
            String::from_utf8_lossy(&unknown_flag.stderr).contains("Unknown option"),
            "{bogus} should report unknown option"
        );
    }

    let unknown_cmd = cli_command(bin_path)
        .arg("bogus-cmd")
        .output()
        .expect("execute lms-audioscan with unknown subcommand");
    assert!(!unknown_cmd.status.success());
}

#[test]
fn test_cli_negative_find_frame_offset_without_double_dash() {
    let dir = tempfile::tempdir().expect("create tempdir for negative offset test");
    let mp3_path = dir.path().join("neg_offset.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
    write_id3v2_mp3_with_apic(&mp3_path, "Neg Offset", "Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let mp3_str = mp3_path.to_str().expect("mp3_path is valid UTF-8");

    // Negative signed 64-bit offsets must pass through without '--'.
    for offset in ["-500", "-1"] {
        let out = cli_command(bin_path)
            .args(["find-frame", "mp3", mp3_str, offset])
            .output()
            .expect("execute find-frame with negative offset");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "find-frame with {offset} failed: {stderr}"
        );
        assert!(!stderr.contains("Unknown option"));
        let val: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("parse negative offset find-frame JSON");
        let min_offset = offset
            .trim_start_matches('-')
            .parse::<i64>()
            .expect("parse positive offset magnitude");
        assert!(val["byte_offset"].as_i64().expect("byte_offset is i64") >= min_offset);
        assert_eq!(val["samplerate"], 44100);
    }

    // Unknown flags positioned like offsets still fail with Unknown option.
    for bad_flag in ["--bogus", "-x"] {
        let out = cli_command(bin_path)
            .args(["find-frame", "mp3", mp3_str, bad_flag])
            .output()
            .expect("execute find-frame with bogus flag");
        assert!(!out.status.success(), "{bad_flag} should fail");
        assert!(String::from_utf8_lossy(&out.stderr).contains("Unknown option"));
    }
}

#[test]
fn test_cli_help_filename_and_double_dash() {
    let dir = tempfile::tempdir().expect("create tempdir for help filename test");
    let help_mp3 = dir.path().join("help.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
    write_id3v2_mp3_with_apic(&help_mp3, "Help Track", "Help Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let help_str = help_mp3.to_str().expect("help_mp3 is valid UTF-8");

    // Existing file named help.mp3 should scan, not print help.
    let ok_help_file = cli_command(bin_path)
        .args(["scan", help_str])
        .output()
        .expect("execute scan on help.mp3");
    assert!(ok_help_file.status.success());
    let stdout = String::from_utf8_lossy(&ok_help_file.stdout);
    assert!(!stdout.contains("Usage: lms-audioscan"));
    let val: serde_json::Value =
        serde_json::from_slice(&ok_help_file.stdout).expect("parse help.mp3 scan JSON");
    assert_eq!(val["tags"]["TIT2"], "Help Track");

    // Missing file named help.mp3 should fail rather than printing help.
    let missing_help = dir.path().join("missing_help.mp3");
    let missing_str = missing_help.to_str().expect("missing_help is valid UTF-8");
    let fail_help = cli_command(bin_path)
        .args(["scan", missing_str])
        .output()
        .expect("execute scan on missing_help.mp3");
    assert!(!fail_help.status.success());
    assert!(!String::from_utf8_lossy(&fail_help.stdout).contains("Usage: lms-audioscan"));

    // Flag-like argument after '--' must be treated as a path, not help.
    let after_dd = cli_command(bin_path)
        .current_dir(dir.path())
        .args(["scan", "--", "--help"])
        .output()
        .expect("execute scan with '-- --help'");
    assert!(!after_dd.status.success());
    assert!(!String::from_utf8_lossy(&after_dd.stdout).contains("Usage: lms-audioscan"));
}

#[test]
fn test_cli_missing_corrupt_and_extra_args() {
    let dir = tempfile::tempdir().expect("create tempdir for missing/corrupt/extra args test");
    let valid_mp3 = dir.path().join("valid.mp3");
    let corrupt_mp3 = dir.path().join("corrupt.mp3");
    let missing_mp3 = dir.path().join("does_not_exist.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
    write_id3v2_mp3_with_apic(&valid_mp3, "Title", "Artist", &fake_jpeg);
    std::fs::write(&corrupt_mp3, b"not a valid audio file at all")
        .expect("write corrupt mp3 fixture");

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let valid_str = valid_mp3.to_str().expect("valid_mp3 is valid UTF-8");
    let corrupt_str = corrupt_mp3.to_str().expect("corrupt_mp3 is valid UTF-8");
    let missing_str = missing_mp3.to_str().expect("missing_mp3 is valid UTF-8");

    let missing_out = cli_command(bin_path)
        .args(["scan", missing_str])
        .output()
        .expect("execute scan on missing file");
    assert!(!missing_out.status.success());

    let corrupt_out = cli_command(bin_path)
        .args(["scan", corrupt_str])
        .output()
        .expect("execute scan on corrupt file");
    assert!(!corrupt_out.status.success());

    // Extra positional arguments must be rejected for all subcommands.
    let extra_scan = cli_command(bin_path)
        .args(["scan", valid_str, "extra"])
        .output()
        .expect("execute scan with extra arg");
    assert!(!extra_scan.status.success());

    let extra_tags = cli_command(bin_path)
        .args(["scan-tags", valid_str, "extra"])
        .output()
        .expect("execute scan-tags with extra arg");
    assert!(!extra_tags.status.success());

    let extra_frame = cli_command(bin_path)
        .args(["find-frame", "mp3", valid_str, "0", "extra"])
        .output()
        .expect("execute find-frame with extra arg");
    assert!(!extra_frame.status.success());
}

#[test]
fn test_cli_no_artwork_and_info_only_flags() {
    let dir = tempfile::tempdir().expect("create tempdir for flags test");
    let mp3_path = dir.path().join("flags.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
    write_id3v2_mp3_with_apic(&mp3_path, "Flags Title", "Flags Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let mp3_str = mp3_path.to_str().expect("mp3_path is valid UTF-8");

    // --no-artwork omits artwork payload even when AUDIO_SCAN_NO_ARTWORK=0.
    let out_no_art = cli_command(bin_path)
        .args(["scan", "--no-artwork", mp3_str])
        .env("AUDIO_SCAN_NO_ARTWORK", "0")
        .output()
        .expect("execute scan with --no-artwork");
    assert!(out_no_art.status.success());
    let val_no_art: serde_json::Value =
        serde_json::from_slice(&out_no_art.stdout).expect("parse --no-artwork JSON");
    assert_eq!(val_no_art["tags"]["APIC"][3], 10);
    assert_eq!(val_no_art["info"]["samplerate"], 44100);

    // --info-only returns info and omits tags.
    let out_info = cli_command(bin_path)
        .args(["--info-only", "scan", mp3_str])
        .output()
        .expect("execute scan with --info-only");
    assert!(out_info.status.success());
    let val_info: serde_json::Value =
        serde_json::from_slice(&out_info.stdout).expect("parse --info-only JSON");
    assert_eq!(val_info["info"]["samplerate"], 44100);
    assert!(val_info["tags"]
        .as_object()
        .expect("tags field is a JSON object")
        .is_empty());

    // --info-only is rejected on incompatible subcommands.
    let bad_info_tags = cli_command(bin_path)
        .args(["scan-tags", "--info-only", mp3_str])
        .output()
        .expect("execute scan-tags with --info-only");
    assert!(!bad_info_tags.status.success());
}

#[test]
fn test_cli_find_frame_invalid_and_overflowing_offsets() {
    let dir = tempfile::tempdir().expect("create tempdir for invalid offset test");
    let mp3_path = dir.path().join("offset_test.mp3");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
    write_id3v2_mp3_with_apic(&mp3_path, "Offset Title", "Offset Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let mp3_str = mp3_path.to_str().expect("mp3_path is valid UTF-8");

    // Non-numeric and positively overflowing offsets fail as invalid offset_or_ms.
    for bad_offset in ["not_a_number", "99999999999999999999999"] {
        let out = cli_command(bin_path)
            .args(["find-frame", "mp3", mp3_str, bad_offset])
            .output()
            .expect("execute find-frame with invalid positive/non-numeric offset");
        assert!(!out.status.success(), "{bad_offset} should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("Invalid offset_or_ms"), "stderr: {stderr}");
    }

    // Overflowing negative i64 offsets must be classified as invalid numeric
    // arguments, not unknown options.
    for neg_overflow in ["-9223372036854775809", "-99999999999999999999999"] {
        let out = cli_command(bin_path)
            .args(["find-frame", "mp3", mp3_str, neg_overflow])
            .output()
            .expect("execute find-frame with overflowing negative offset");
        assert!(!out.status.success(), "{neg_overflow} should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Invalid numeric argument"),
            "expected invalid numeric argument for {neg_overflow}, got: {stderr}"
        );
        assert!(!stderr.contains("Unknown option"), "stderr: {stderr}");
    }

    // Overflowing negative offset after '--' is rejected by find-frame offset
    // parsing.
    let out_dd = cli_command(bin_path)
        .args(["find-frame", "mp3", mp3_str, "--", "-9223372036854775809"])
        .output()
        .expect("execute find-frame with overflowing negative offset after '--'");
    assert!(!out_dd.status.success());
    let stderr_dd = String::from_utf8_lossy(&out_dd.stderr);
    assert!(
        stderr_dd.contains("Invalid offset_or_ms"),
        "stderr: {stderr_dd}"
    );
    assert!(!stderr_dd.contains("Unknown option"));
}

#[test]
fn test_cli_rejects_non_regular_files() {
    let dir = tempfile::tempdir().expect("create tempdir for non-regular file test");
    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let dir_str = dir.path().to_str().expect("dir path is valid UTF-8");

    for args in [
        vec!["scan", dir_str],
        vec!["scan-tags", dir_str],
        vec!["find-frame", "mp3", dir_str, "0"],
    ] {
        let out = cli_command(bin_path)
            .args(&args)
            .output()
            .expect("execute lms-audioscan against directory");
        assert!(!out.status.success(), "directory should be rejected");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("Not a regular file"), "stderr: {stderr}");
    }

    #[cfg(unix)]
    {
        let target_mp3 = dir.path().join("target.mp3");
        let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        write_id3v2_mp3_with_apic(&target_mp3, "Target", "Artist", &fake_jpeg);

        let symlink_path = dir.path().join("link.mp3");
        std::os::unix::fs::symlink(&target_mp3, &symlink_path).expect("create symlink fixture");
        let link_out = cli_command(bin_path)
            .args([
                "scan",
                symlink_path.to_str().expect("symlink path is UTF-8"),
            ])
            .output()
            .expect("execute scan on symlink");
        assert!(!link_out.status.success(), "symlink should be rejected");

        let fifo_path = dir.path().join("audio_fifo.mp3");
        let fifo_status = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("invoke mkfifo for FIFO test");
        assert!(fifo_status.success(), "mkfifo failed");

        let fifo_out = cli_command(bin_path)
            .args(["scan", fifo_path.to_str().expect("fifo path is UTF-8")])
            .output()
            .expect("execute scan on FIFO without hanging");
        assert!(!fifo_out.status.success(), "FIFO should be rejected");
        let fifo_stderr = String::from_utf8_lossy(&fifo_out.stderr);
        assert!(
            fifo_stderr.contains("Not a regular file"),
            "stderr: {fifo_stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_cli_non_utf8_unix_path_no_panic() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().expect("create tempdir for non-utf8 unix test");
    let non_utf8_name = OsString::from_vec(b"track_\xff_\xfe.mp3".to_vec());
    let mp3_path = dir.path().join(non_utf8_name);
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
    write_id3v2_mp3_with_apic(&mp3_path, "NonUTF8 Title", "Artist", &fake_jpeg);

    let bin_path = env!("CARGO_BIN_EXE_lms-audioscan");
    let out = cli_command(bin_path)
        .arg("scan")
        .arg(&mp3_path)
        .output()
        .expect("execute scan on non-utf8 path");
    assert!(out.status.success());
    let val: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("parse non-utf8 path scan JSON");
    assert_eq!(val["tags"]["TIT2"], "NonUTF8 Title");

    let bad_cmd = OsString::from_vec(b"cmd_\xff".to_vec());
    let err_out = cli_command(bin_path)
        .arg(bad_cmd)
        .output()
        .expect("execute lms-audioscan with non-utf8 subcommand");
    assert!(!err_out.status.success());
    assert!(!String::from_utf8_lossy(&err_out.stderr).contains("panicked"));
}
