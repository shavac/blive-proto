fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/event.proto");
    let mut config = prost_build::Config::new();
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        config.protoc_executable(protoc);
    }
    config.type_attribute(
        ".",
        "#[derive(serde::Serialize, serde::Deserialize)]\n#[serde(default)]",
    );
    config.field_attribute("EventUserBase.name", "#[serde(alias = \"uname\")]");
    config.field_attribute("EventUserMedal.name", "#[serde(alias = \"medal_name\")]");
    config.field_attribute("EventUserMedal.level", "#[serde(alias = \"medal_level\")]");
    config.field_attribute(
        "EventUserMedal.color_v2",
        "#[serde(alias = \"v2_medal_color_text\")]",
    );
    config.field_attribute(
        "EventUserInfo.medal",
        "#[serde(alias = \"fans_medal\", alias = \"medal_info\")]",
    );
    config.compile_protos(&["proto/event.proto"], &["proto/"])?;
    Ok(())
}
