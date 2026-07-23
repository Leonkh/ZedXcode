//! Scenario config schema. The structs live in the shared `xcode-dap-config`
//! crate (single source of truth with the WASM extension); re-exported here.

pub use xcode_dap_config::{BuildOutput, LaunchConfig};

#[cfg(test)]
mod tests {
    use super::*;

    /// `extension/debug_adapter_schemas/Xcode.json` must stay
    /// field-for-field in parity with `LaunchConfig` (one config surface,
    /// two artifacts).
    #[test]
    fn schema_properties_match_launch_config_fields() {
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../extension/debug_adapter_schemas/Xcode.json"
        ))
        .unwrap();
        let mut schema_keys: Vec<String> = schema["properties"]
            .as_object()
            .expect("schema has a properties object")
            .keys()
            .cloned()
            .collect();
        schema_keys.sort();

        // All Options populated so every field serializes to a key.
        let cfg = LaunchConfig {
            workspace: "MyApp.xcworkspace".into(),
            scheme: "MyApp".into(),
            device: Some("iPhone 15 Pro Max".into()),
            os: Some("26.3".into()),
            configuration: Some("Debug".into()),
            preflight: Some("make project CI=true".into()),
            oslog: true,
            oslog_predicate: Some("subsystem == \"x\"".into()),
            terminate_on_stop: true,
            build_output: BuildOutput::Filtered,
            verbose_logging: false,
            derived_data: Some("/Users/x/dd".into()),
        };
        let mut config_keys: Vec<String> = serde_json::to_value(&cfg)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        config_keys.sort();

        assert_eq!(schema_keys, config_keys);
    }
}
