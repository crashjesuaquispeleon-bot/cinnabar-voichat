//! Android templates render independently of the compiler and Android SDK.

#[cfg(unix)]
mod support;

#[cfg(unix)]
#[test]
fn android_manifest_renders_and_unknown_tokens_still_fail() {
    use std::{fs, path::Path, process::Command};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pins: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("packaging/android/runtime.json")).unwrap())
            .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("AndroidManifest.xml");
    let render = |source: &Path| {
        support::bounded_output(
            Command::new("bash")
                .args([
                    "-euc",
                    r#"
                pin() { printf '%s' "$FIXTURE_APPLICATION_ID"; }
                client_library=fixture_client
                product=Fixture
                version_code=1
                version=0.0.1
                source "$1"
                render "$2" "$3"
            "#,
                    "android-manifest-test",
                ])
                .arg(root.join("packaging/android/render-template.sh"))
                .arg(source)
                .arg(&output)
                .env(
                    "FIXTURE_APPLICATION_ID",
                    pins["application_id"].as_str().unwrap(),
                ),
        )
    };
    let result = render(&root.join("packaging/android/AndroidManifest.xml"));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let xml = fs::read_to_string(&output).unwrap();
    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut saw_manifest = false;
    let mut saw_record_audio = false;
    let mut saw_optional_microphone = false;
    loop {
        match reader.read_event().unwrap() {
            quick_xml::events::Event::Start(tag) if tag.name().as_ref() == b"manifest" => {
                let package = tag.try_get_attribute("package").unwrap().unwrap();
                assert_eq!(
                    package.decode_and_unescape_value(reader.decoder()).unwrap(),
                    pins["application_id"].as_str().unwrap()
                );
                saw_manifest = true;
            }
            quick_xml::events::Event::Empty(tag) if tag.name().as_ref() == b"uses-permission" => {
                if let Some(permission) = tag.try_get_attribute("android:name").unwrap() {
                    saw_record_audio |= permission
                        .decode_and_unescape_value(reader.decoder())
                        .unwrap()
                        == "android.permission.RECORD_AUDIO";
                }
            }
            quick_xml::events::Event::Empty(tag) if tag.name().as_ref() == b"uses-feature" => {
                if let Some(feature) = tag.try_get_attribute("android:name").unwrap() {
                    let name = feature
                        .decode_and_unescape_value(reader.decoder())
                        .unwrap();
                    if name == "android.hardware.microphone" {
                        let required = tag
                            .try_get_attribute("android:required")
                            .unwrap()
                            .expect("microphone feature must declare whether it is required")
                            .decode_and_unescape_value(reader.decoder())
                            .unwrap();
                        saw_optional_microphone = required == "false";
                    }
                }
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    assert!(saw_manifest);
    assert!(saw_record_audio, "Android voice alpha must declare RECORD_AUDIO");
    assert!(
        saw_optional_microphone,
        "microphone hardware must remain optional for non-voice gameplay"
    );
    let malformed = directory.path().join("unfilled.xml");
    fs::write(&malformed, "<manifest package=\"@UNKNOWN@\" />").unwrap();
    let result = render(&malformed);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("unfilled template token"));
}
