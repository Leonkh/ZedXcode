//! Scenario config schema. The structs live in the shared `xcode-dap-config`
//! crate (single source of truth with the WASM extension); re-exported here.

pub use xcode_dap_config::{BuildOutput, LaunchConfig};

/// Why a scenario `config` does not deserialize into [`LaunchConfig`]: the
/// key at fault and its error when one key alone fails (the extension names
/// the key the same way), else the whole error.
pub fn invalid_config_reason(config: &serde_json::Value, err: &serde_json::Error) -> String {
    let invalid = config.as_object().and_then(|object| {
        xcode_dap_config::invalid_key::<_, _, serde_json::Error>(
            object.iter().map(|(key, value)| (key.as_str(), value)),
        )
    });
    match invalid {
        Some((key, key_err)) => format!("the key \"{key}\" is invalid: {key_err}"),
        None => err.to_string(),
    }
}

/// `extension/debug_adapter_schemas/Xcode.json` must stay in parity with
/// `LaunchConfig` (one config surface, two artifacts): the same keys, and for
/// each key the same type, the same default and the same required-ness. Every
/// check is driven by the schema and by serde, so a field added on one side
/// only fails at least one of them.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map, Value};

    fn schema() -> Value {
        serde_json::from_str(include_str!(
            "../../../../extension/debug_adapter_schemas/Xcode.json"
        ))
        .unwrap()
    }

    fn properties(schema: &Value) -> &Map<String, Value> {
        schema["properties"]
            .as_object()
            .expect("schema has a properties object")
    }

    /// The schema's `required` list, sorted (empty when absent).
    fn required(schema: &Value) -> Vec<String> {
        let mut keys: Vec<String> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|keys| {
                keys.iter()
                    .map(|k| k.as_str().expect("required entries are strings").to_owned())
                    .collect()
            })
            .unwrap_or_default();
        keys.sort();
        keys
    }

    /// Values the schema allows for `key` (an enum's variants, or samples of
    /// its `type`), and values of another type that it rejects.
    fn samples(key: &str, prop: &Value) -> (Vec<Value>, Vec<Value>) {
        if let Some(variants) = prop.get("enum").and_then(Value::as_array) {
            assert!(
                variants.iter().all(Value::is_string),
                "{key}: only string enums are known to this test; extend samples()"
            );
            return (
                variants.clone(),
                vec![json!("not-a-listed-value"), json!(42)],
            );
        }
        match prop.get("type").and_then(Value::as_str) {
            Some("string") => (vec![json!("example")], vec![json!(42), json!(true)]),
            Some("boolean") => (
                vec![json!(true), json!(false)],
                vec![json!("true"), json!(1)],
            ),
            Some("integer") => (vec![json!(7)], vec![json!("7"), json!(true)]),
            Some("number") => (vec![json!(1.5)], vec![json!("1.5"), json!(true)]),
            Some("array") => {
                let item = prop
                    .get("items")
                    .map(|items| samples(&format!("{key}[]"), items).0.remove(0));
                (
                    vec![Value::Array(item.into_iter().collect())],
                    vec![json!("example"), json!(42)],
                )
            }
            Some("object") => {
                let mut entry = Map::new();
                if let Some(values) = prop.get("additionalProperties").filter(|v| v.is_object()) {
                    let value = samples(&format!("{key}.*"), values).0.remove(0);
                    entry.insert("EXAMPLE_KEY".to_owned(), value);
                }
                (
                    vec![Value::Object(entry)],
                    vec![json!("example"), json!(42)],
                )
            }
            other => {
                panic!("{key}: schema type {other:?} is unknown to this test; extend samples()")
            }
        }
    }

    /// A scenario config that sets every schema property to an allowed value.
    fn full_config(props: &Map<String, Value>) -> Map<String, Value> {
        props
            .iter()
            .map(|(key, prop)| (key.clone(), samples(key, prop).0.remove(0)))
            .collect()
    }

    /// The same config reduced to the schema's required keys.
    fn minimal_config(schema: &Value) -> Map<String, Value> {
        let required = required(schema);
        full_config(properties(schema))
            .into_iter()
            .filter(|(key, _)| required.contains(key))
            .collect()
    }

    fn parse(config: &Map<String, Value>) -> Result<LaunchConfig, serde_json::Error> {
        serde_json::from_value(Value::Object(config.clone()))
    }

    /// The keys `LaunchConfig` declares, sorted, as its derived `Deserialize`
    /// names them (after `rename_all`). Read off the field list it hands to
    /// `deserialize_struct`, so a field that never serializes (for example a
    /// `skip_serializing_if` field left unset) still counts.
    fn launch_config_fields() -> Vec<String> {
        use serde::de::{self, Visitor};
        use serde::Deserialize;

        struct FieldNames(Vec<&'static str>);

        impl<'de> de::Deserializer<'de> for &mut FieldNames {
            type Error = de::value::Error;

            fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
                Err(de::Error::custom("LaunchConfig is expected to be a struct"))
            }

            fn deserialize_struct<V: Visitor<'de>>(
                self,
                _name: &'static str,
                fields: &'static [&'static str],
                _visitor: V,
            ) -> Result<V::Value, Self::Error> {
                self.0.extend_from_slice(fields);
                Err(de::Error::custom("field names recorded"))
            }

            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
                bytes byte_buf option unit unit_struct newtype_struct seq tuple
                tuple_struct map enum identifier ignored_any
            }
        }

        let mut names = FieldNames(Vec::new());
        let _ = LaunchConfig::deserialize(&mut names);
        assert!(!names.0.is_empty(), "LaunchConfig named no fields");
        let mut fields: Vec<String> = names.0.iter().map(|f| (*f).to_owned()).collect();
        fields.sort();
        fields
    }

    #[test]
    fn schema_properties_match_launch_config_fields() {
        let mut schema_keys: Vec<String> = properties(&schema()).keys().cloned().collect();
        schema_keys.sort();
        assert_eq!(
            schema_keys,
            launch_config_fields(),
            "schema properties vs the fields LaunchConfig declares"
        );

        // All Options populated so every field serializes to a key.
        let cfg = LaunchConfig {
            workspace: Some("MyApp.xcworkspace".into()),
            scheme: Some("MyApp".into()),
            device: Some("iPhone 15 Pro Max".into()),
            os: Some("26.3".into()),
            configuration: Some("Debug".into()),
            preflight: Some("xcodegen generate".into()),
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

    /// Each schema-allowed value deserializes into the field of that name and
    /// serializes back unchanged; a value of another type is rejected. A key
    /// that exists only in the schema fails here, because `LaunchConfig`
    /// (no `deny_unknown_fields`) silently ignores the wrong-typed value.
    #[test]
    fn schema_types_match_launch_config_fields() {
        let schema = schema();
        let props = properties(&schema);
        let full = full_config(props);
        if let Err(e) = parse(&full) {
            panic!("a config that sets every schema property does not deserialize: {e}");
        }
        for (key, prop) in props {
            let (allowed, rejected) = samples(key, prop);
            for value in allowed {
                let mut cfg = full.clone();
                cfg.insert(key.clone(), value.clone());
                let parsed = parse(&cfg).unwrap_or_else(|e| {
                    panic!("{key} = {value}: the schema allows it, LaunchConfig rejects it: {e}")
                });
                let back = serde_json::to_value(parsed).unwrap();
                assert_eq!(
                    back.get(key),
                    Some(&value),
                    "{key} = {value} does not round-trip through LaunchConfig"
                );
            }
            for value in rejected {
                let mut cfg = full.clone();
                cfg.insert(key.clone(), value.clone());
                assert!(
                    parse(&cfg).is_err(),
                    "{key} = {value}: the schema rejects it, LaunchConfig accepts it"
                );
            }
        }
    }

    /// The schema's `required` list is exactly the set of keys whose absence
    /// makes deserialization fail, and the required keys alone suffice.
    #[test]
    fn schema_required_matches_launch_config() {
        let schema = schema();
        let props = properties(&schema);
        let full = full_config(props);
        let mut fail_when_absent: Vec<String> = props
            .keys()
            .filter(|key| {
                let mut cfg = full.clone();
                cfg.remove(key.as_str());
                parse(&cfg).is_err()
            })
            .cloned()
            .collect();
        fail_when_absent.sort();
        assert_eq!(
            required(&schema),
            fail_when_absent,
            "schema \"required\" vs the keys LaunchConfig cannot do without"
        );
        if let Err(e) = parse(&minimal_config(&schema)) {
            panic!("a config with only the schema's required keys does not deserialize: {e}");
        }
    }

    /// Every optional key's schema `default` is the value `LaunchConfig`
    /// takes when the key is absent; a key without a `default` stays unset
    /// (`null`). A required key carries no `default`, which would never apply.
    #[test]
    fn schema_defaults_match_serde_defaults() {
        let schema = schema();
        let required = required(&schema);
        let parsed = parse(&minimal_config(&schema))
            .expect("a config with only the schema's required keys deserializes");
        let effective = serde_json::to_value(parsed).unwrap();
        for (key, prop) in properties(&schema) {
            if required.contains(key) {
                assert!(
                    prop.get("default").is_none(),
                    "{key} is required, so its schema default would never apply"
                );
                continue;
            }
            let schema_default = prop.get("default").cloned().unwrap_or(Value::Null);
            let serde_default = effective.get(key).cloned().unwrap_or(Value::Null);
            assert_eq!(
                schema_default, serde_default,
                "{key}: schema default vs LaunchConfig's value when the key is absent"
            );
        }
    }

    /// The key the extension and the adapter name for a config that does not
    /// parse: each key is tried on its own.
    #[test]
    fn invalid_key_names_the_key_at_fault() {
        let reason = |config: Value| {
            let err = serde_json::from_value::<LaunchConfig>(config.clone()).unwrap_err();
            invalid_config_reason(&config, &err)
        };
        assert_eq!(
            reason(json!({"scheme": "MyApp", "oslog": "yes", "label": 1})),
            "the key \"oslog\" is invalid: invalid type: string \"yes\", expected a boolean"
        );
        assert_eq!(
            reason(json!({"workspace": 42})),
            "the key \"workspace\" is invalid: invalid type: integer `42`, expected path string"
        );
        assert_eq!(
            reason(json!({"buildOutput": "verbose"})),
            "the key \"buildOutput\" is invalid: unknown variant `verbose`, expected `filtered` \
             or `full`"
        );
        // Unknown keys never fail; a config that is no object has no key.
        assert_eq!(
            xcode_dap_config::invalid_key::<_, _, serde_json::Error>(
                json!({"stopOnEntry": true, "label": "Run"})
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.as_str(), v))
            )
            .map(|(k, _)| k),
            None
        );
        assert_eq!(
            reason(json!("Run")),
            "invalid type: string \"Run\", expected struct LaunchConfig"
        );
    }

    /// The extension's message, built the way `extension/src/lib.rs` builds
    /// it (that crate is wasm-only and has no host tests).
    #[test]
    fn the_extension_names_the_invalid_key_in_the_plan_wording() {
        let message = |config: &str| {
            let err = serde_json::from_str::<LaunchConfig>(config).unwrap_err();
            let parsed = serde_json::from_str::<Value>(config).ok();
            let invalid = parsed
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|object| {
                    xcode_dap_config::invalid_key::<_, _, serde_json::Error>(
                        object.iter().map(|(key, value)| (key.as_str(), value)),
                    )
                });
            xcode_dap_config::invalid_scenario_message("/Users/x/bin/xcode-dap", invalid, &err)
        };
        assert_eq!(
            message(r#"{"scheme": "MyApp", "terminateOnStop": "no"}"#),
            "Xcode Tools installed xcode-dap at /Users/x/bin/xcode-dap, but the key \
             \"terminateOnStop\" in this scenario is invalid: invalid type: string \"no\", \
             expected a boolean. Fix it in .zed/debug.json (hover shows its docs). Scheme, \
             Destination and Configuration are not set here: use the Xcode: Choose Scheme / \
             Choose Destination / Choose Configuration tasks (⇧⌘R) or Xcode: Set Up Project."
        );
        // No single key at fault (not an object): the whole error.
        let whole = message(r#""Run""#);
        assert!(
            whole.starts_with(
                "Xcode Tools installed xcode-dap at /Users/x/bin/xcode-dap, but this scenario \
                 is invalid: invalid type: string \"Run\", expected struct LaunchConfig"
            ),
            "{whole}"
        );
        assert!(
            whole.ends_with("(⇧⌘R) or Xcode: Set Up Project."),
            "{whole}"
        );
    }

    /// The schema's `buildOutput` enum lists exactly the `BuildOutput`
    /// variants, in declaration order.
    #[test]
    fn build_output_enum_matches_schema() {
        let variants = [BuildOutput::Filtered, BuildOutput::Full];
        // Exhaustive on purpose: a new variant stops this from compiling
        // until it is added to `variants` (and then to the schema).
        for variant in variants {
            match variant {
                BuildOutput::Filtered | BuildOutput::Full => {}
            }
        }
        let names: Vec<Value> = variants
            .iter()
            .map(|v| serde_json::to_value(v).unwrap())
            .collect();
        assert_eq!(
            schema()["properties"]["buildOutput"]["enum"],
            Value::Array(names)
        );
    }
}
