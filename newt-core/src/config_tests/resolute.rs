//! #2451: real-file grounding for the pure importer/decoder regressions.
//! Both production readers must reject a source before layering erases its version.
use super::*;

#[test]
fn resolute_2451_unknown_source_version_is_rejected_before_layering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text =
        "# Keep my source intact\n[tenacity]\nversion = 3 # unknown\ndefault = \"relentless\"\n";
    std::fs::write(&path, text).unwrap();
    assert!(Config::load(&path, &mut |_| {}).is_err());
    assert!(
        Config::load_value(&path, &mut |_| {}).is_err(),
        "raw layered input must reject the unsupported source before overlaying version 2"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn resolute_2451_current_config_fast_and_layered_readers_agree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for level in ["normal", "resolute", "relentless"] {
        let text =
            format!("# preserved\n[tenacity]\nversion = 2 # current\ndefault = \"{level}\"\n");
        std::fs::write(&path, &text).unwrap();
        let fast = Config::load(&path, &mut |_| {}).unwrap();
        let layered: Config = Config::load_value(&path, &mut |_| {})
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(fast.tenacity, layered.tenacity);
        assert_eq!(fast.tenacity.unwrap().default.unwrap().label(), level);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}
