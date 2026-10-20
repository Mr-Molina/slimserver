use std::process::Command;

fn write_id3v2_mp3_with_apic(path: &std::path::Path, title: &str, artist: &str, jpeg: &[u8]) {
    use lofty::config::WriteOptions;
    use lofty::id3::v2::Id3v2Tag;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::*;

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

    // Test scan with AUDIO_SCAN_NO_ARTWORK=1
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

    // Test scan-tags with AUDIO_SCAN_NO_ARTWORK=0
    let output_tags = Command::new(bin_path)
        .args(["scan-tags", mp3_path.to_str().unwrap()])
        .env("AUDIO_SCAN_NO_ARTWORK", "0")
        .output()
        .unwrap();
    assert!(output_tags.status.success());
    let val_tags: serde_json::Value = serde_json::from_slice(&output_tags.stdout).unwrap();
    assert!(val_tags["info"].as_object().unwrap().is_empty());
    assert!(val_tags["tags"]["APIC"][3].is_string());

    // Test find-frame
    let output_frame = Command::new(bin_path)
        .args(["find-frame", "mp3", mp3_path.to_str().unwrap(), "0"])
        .output()
        .unwrap();
    assert!(output_frame.status.success());
    let val_frame: serde_json::Value = serde_json::from_slice(&output_frame.stdout).unwrap();
    assert!(val_frame["byte_offset"].as_i64().unwrap() > 0);
}
