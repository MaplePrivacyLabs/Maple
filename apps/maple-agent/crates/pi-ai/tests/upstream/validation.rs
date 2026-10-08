use pi_ai::types::{Schema, Tool, ToolCall};
use pi_ai::utils::validation::{validate_tool_arguments as validate, validate_tool_call};
use serde_json::{Value, json};

fn tool_and_call(schema: Schema, args: Value) -> (Tool, ToolCall) {
    let mut tool: Tool =
        serde_json::from_value(json!({"name":"echo", "description":"Echo tool", "parameters":{}}))
            .unwrap();
    tool.parameters = schema;
    let call = serde_json::from_value(
        json!({"type":"toolCall", "id":"tool-1", "name":"echo", "arguments":args}),
    )
    .unwrap();
    (tool, call)
}
fn plain_value(schema: Value, value: Value) -> (Tool, ToolCall) {
    tool_and_call(
        Schema::json_schema(
            json!({"type":"object", "properties":{"value":schema}, "required":["value"]}),
        ),
        json!({"value":value}),
    )
}

#[test]
fn validation_coercion_preserves_shared_argument_input() {
    let (tool, call) = plain_value(json!({"type":"number"}), json!("42"));
    let alias = call.arguments.clone();
    assert_eq!(validate(&tool, &call).unwrap(), json!({"value":42}));
    assert!(alias.ptr_eq(&call.arguments));
    assert_eq!(alias.snapshot(), json!({"value":"42"}));
}

mod validate_tool_arguments {
    use super::*;

    #[test]
    fn still_validates_when_function_constructor_is_unavailable() {
        // Rust has no dynamic Function constructor; the same input exercises
        // the non-JIT validation path unconditionally.
        let (tool, call) = tool_and_call(
            Schema::typebox(
                json!({"type":"object", "required":["count"], "properties":{"count":{"type":"number"}}}),
            ),
            json!({"count":"42"}),
        );
        assert_eq!(validate(&tool, &call).unwrap(), json!({"count":42}));
    }

    #[test]
    fn coerces_serialized_plain_json_schemas_with_ajv_compatible_primitive_rules() {
        let cases = [
            (json!({"type":"number"}), json!("42"), json!(42)),
            (json!({"type":"number"}), json!(true), json!(1)),
            (json!({"type":"number"}), Value::Null, json!(0)),
            (json!({"type":"integer"}), json!("42"), json!(42)),
            (json!({"type":"boolean"}), json!("true"), json!(true)),
            (json!({"type":"boolean"}), json!("false"), json!(false)),
            (json!({"type":"boolean"}), json!(1), json!(true)),
            (json!({"type":"boolean"}), json!(0), json!(false)),
            (json!({"type":"string"}), Value::Null, json!("")),
            (json!({"type":"string"}), json!(true), json!("true")),
            (json!({"type":"null"}), json!(""), Value::Null),
            (json!({"type":"null"}), json!(0), Value::Null),
            (json!({"type":"null"}), json!(false), Value::Null),
            (json!({"type":["number","string"]}), json!("1"), json!("1")),
            (json!({"type":["boolean","number"]}), json!("1"), json!(1)),
        ];
        for (schema, input, expected) in cases {
            let (tool, call) = plain_value(schema, input);
            assert_eq!(validate(&tool, &call).unwrap(), json!({"value":expected}));
        }
    }

    #[test]
    fn treats_null_as_omission_for_optional_non_nullable_properties() {
        let schema = Schema::typebox(
            json!({"type":"object", "required":["path","metadata"], "properties": {
                "path":{"type":"string"}, "offset":{"type":"number"},
                "nullable":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "metadata":{"type":"object", "properties":{"enabled":{"type":"boolean"}}}
            }}),
        );
        let (tool, call) = tool_and_call(
            schema,
            json!({"path":"file.txt", "offset":null, "nullable":null, "metadata":{"enabled":null}}),
        );
        assert_eq!(
            validate(&tool, &call).unwrap(),
            json!({"path":"file.txt", "nullable":null, "metadata":{}})
        );
    }

    #[test]
    fn preserves_optional_nulls_whose_referenced_schema_is_nullable() {
        let (tool, call) = tool_and_call(
            Schema::json_schema(
                json!({"type":"object", "properties":{"value":{"$ref":"#/$defs/value"}}, "$defs":{"value":{"anyOf":[{"type":"number"},{"type":"null"}]}}}),
            ),
            json!({"value":null}),
        );
        assert_eq!(validate(&tool, &call).unwrap(), json!({"value":null}));
    }

    #[test]
    fn preserves_a_value_that_already_matches_a_nullable_union_arm() {
        let (tool, call) = tool_and_call(
            Schema::typebox(
                json!({"type":"object", "required":["value"], "properties":{"value":{"anyOf":[{"type":"number"},{"type":"null"}]}}}),
            ),
            json!({"value":null}),
        );
        assert_eq!(validate(&tool, &call).unwrap(), json!({"value":null}));
    }

    #[test]
    fn preserves_a_value_that_already_matches_a_one_of_nullable_union_arm() {
        let (tool, call) = plain_value(
            json!({"oneOf":[{"type":"number"},{"type":"null"}]}),
            Value::Null,
        );
        assert_eq!(validate(&tool, &call).unwrap(), json!({"value":null}));
    }

    #[test]
    fn still_coerces_nullable_unions_when_the_original_value_does_not_match_any_arm() {
        let (tool, call) = plain_value(
            json!({"anyOf":[{"type":"number"},{"type":"null"}]}),
            json!("42"),
        );
        assert_eq!(validate(&tool, &call).unwrap(), json!({"value":42}));
    }

    #[test]
    fn accepts_null_for_nullable_array_schemas_with_items() {
        let (tool, call) = plain_value(
            json!({"type":["array","null"], "items":{"type":"string"}}),
            Value::Null,
        );
        assert_eq!(validate(&tool, &call).unwrap(), json!({"value":null}));
    }

    #[test]
    fn rejects_invalid_coercions_for_serialized_plain_json_schemas() {
        for (schema, input) in [
            (json!({"type":"boolean"}), json!("1")),
            (json!({"type":"boolean"}), json!("0")),
            (json!({"type":"null"}), json!("null")),
            (json!({"type":"integer"}), json!("42.1")),
        ] {
            let (tool, call) = plain_value(schema, input);
            assert!(
                validate(&tool, &call)
                    .unwrap_err()
                    .to_string()
                    .contains("Validation failed")
            );
        }
    }
}

#[test]
fn remote_and_file_references_are_denied_even_with_unified_resolver_features() {
    for reference in ["https://example.invalid/schema.json", "file:///etc/passwd"] {
        let (tool, call) = plain_value(json!({"$ref": reference}), json!(3));
        assert!(validate(&tool, &call).is_err());
    }
}

#[test]
fn modern_typebox_metadata_is_not_the_legacy_symbol_flag() {
    let schema = Schema::typebox(
        json!({"type":"object", "properties":{"flag":{"type":"boolean"}}, "required":["flag"]}),
    );
    assert!(!schema.is_typebox);
    assert_eq!(serde_json::to_value(&schema).unwrap(), schema.schema);
    let (tool, call) = tool_and_call(schema, json!({"flag":"1"}));
    assert_eq!(validate(&tool, &call).unwrap(), json!({"flag":true}));
    let serialized = serde_json::to_value(&tool).unwrap();
    let plain: Tool = serde_json::from_value(serialized).unwrap();
    assert!(validate(&plain, &call).is_err());
}

#[test]
fn required_and_nested_errors_follow_typebox_order_and_received_args_are_original() {
    let schema = Schema::json_schema(
        json!({"type":"object","properties":{"count":{"type":"number"},"nested":{"type":"object","required":["a","b"]}},"required":["missing","other"]}),
    );
    let (tool, call) = tool_and_call(schema, json!({"count":"oops", "nested":{}}));
    assert_eq!(
        validate(&tool, &call).unwrap_err().to_string(),
        "Validation failed for tool \"echo\":\n  - missing: must have required properties missing, other\n  - count: must be number\n  - nested.a: must have required properties a, b\n\nReceived arguments:\n{\n  \"count\": \"oops\",\n  \"nested\": {}\n}"
    );
    assert_eq!(call.arguments.snapshot()["count"], json!("oops"));
}

#[test]
fn missing_tool_error_is_exact() {
    let (_, call) = plain_value(json!({}), Value::Null);
    assert_eq!(
        validate_tool_call(&[], &call).unwrap_err().to_string(),
        "Tool \"echo\" not found"
    );
}

#[test]
fn typebox_array_wrap_and_integer_truncation_are_preserved() {
    let (tool, call) = tool_and_call(
        Schema::typebox(
            json!({"type":"object", "required":["value"], "properties":{"value":{"type":"array", "items":{"type":"integer"}}}}),
        ),
        json!({"value":"2.7"}),
    );
    assert_eq!(validate(&tool, &call).unwrap(), json!({"value":[2]}));
}

#[test]
fn compact_grapheme_length_and_multiple_of_tolerance_match_typebox() {
    for value in ["e\u{301}", "👨‍👩‍👦", "🇺🇸"] {
        let (tool, call) = plain_value(
            json!({"type":"string", "minLength":1, "maxLength":1}),
            json!(value),
        );
        assert_eq!(validate(&tool, &call).unwrap(), json!({"value":value}));
    }
    let (tool, call) = plain_value(
        json!({"type":"number", "multipleOf":0.1}),
        json!(0.30000000001),
    );
    assert!(validate(&tool, &call).is_ok());
}

#[test]
fn additional_properties_and_union_errors_are_typebox_messages() {
    let (tool, call) = tool_and_call(
        Schema::json_schema(
            json!({"type":"object", "properties":{"x":{"anyOf":[{"type":"array"},{"type":"object"}]}}, "additionalProperties":false}),
        ),
        json!({"extra":1, "x":2}),
    );
    assert_eq!(
        validate(&tool, &call).unwrap_err().to_string(),
        "Validation failed for tool \"echo\":\n  - extra: schema is false\n  - root: must not have additional properties\n  - x: must be array\n  - x: must be object\n  - x: must match a schema in anyOf\n\nReceived arguments:\n{\n  \"extra\": 1,\n  \"x\": 2\n}"
    );
}

#[test]
fn numeric_whitespace_and_bigint_like_strings_follow_javascript_rules() {
    for text in ["\u{85}", "01n", "+2n"] {
        let (tool, call) = tool_and_call(
            Schema::typebox(
                json!({"type":"object", "required":["value"], "properties":{"value":{"type":"number"}}}),
            ),
            json!({"value":text}),
        );
        assert!(validate(&tool, &call).is_err(), "{text:?}");
    }
    let (tool, call) = plain_value(json!({"type":"number"}), json!("\u{feff}"));
    assert!(validate(&tool, &call).is_err());
    let (tool, call) = tool_and_call(
        Schema::typebox(
            json!({"type":"object", "required":["value"], "properties":{"value":{"type":"number"}}}),
        ),
        json!({"value":"\u{feff}"}),
    );
    assert_eq!(validate(&tool, &call).unwrap(), json!({"value":0}));
}

#[test]
fn legacy_symbol_flag_and_per_node_kinds_are_independent() {
    let mut schema = Schema::json_schema(
        json!({"type":"object", "required":["value"], "properties":{"value":{"type":"number"}}}),
    );
    schema.is_typebox = true;
    let (tool, call) = tool_and_call(schema, json!({"value":"2"}));
    assert!(validate(&tool, &call).is_err());
    let mut schema = Schema::typebox(tool.parameters.schema.clone());
    schema.is_typebox = true;
    let (tool, call) = tool_and_call(schema, json!({"value":"2"}));
    assert_eq!(validate(&tool, &call).unwrap(), json!({"value":2}));
}

#[test]
fn error_count_is_capped_at_typebox_default() {
    let mut properties = serde_json::Map::new();
    let mut args = serde_json::Map::new();
    for index in 0..10 {
        properties.insert(format!("key{index}"), json!({"type":"number"}));
        args.insert(format!("key{index}"), json!("bad"));
    }
    let (tool, call) = tool_and_call(
        Schema::json_schema(json!({"type":"object", "properties":properties})),
        Value::Object(args),
    );
    let error = validate(&tool, &call).unwrap_err().to_string();
    assert_eq!(
        error
            .lines()
            .filter(|line| line.starts_with("  - "))
            .count(),
        8
    );
    assert!(error.contains("  - key7: must be number"));
    assert!(!error.contains("  - key8:"));
}

#[test]
fn typebox_intersection_structurally_narrows_before_conversion() {
    let (tool, call) = tool_and_call(
        Schema::typebox(
            json!({"type":"object", "required":["value"], "properties":{"value":{"allOf":[{"type":"array","items":{"type":"number"}},{"type":"array","items":{"type":"string"}}]}}}),
        ),
        json!({"value":"2"}),
    );
    assert_eq!(
        validate(&tool, &call).unwrap_err().to_string(),
        "Validation failed for tool \"echo\":\n  - value: must be array\n  - value: must be array\n\nReceived arguments:\n{\n  \"value\": \"2\"\n}"
    );
    let (tool, call) = tool_and_call(
        Schema::typebox(
            json!({"type":"object", "required":["value"], "properties":{"value":{"allOf":[{"type":"object","required":["a"],"properties":{"a":{"type":"number"}}},{"type":"object","required":["b"],"properties":{"b":{"type":"boolean"}}}]}}}),
        ),
        json!({"value":{"a":"true","b":"1"}}),
    );
    assert_eq!(
        validate(&tool, &call).unwrap(),
        json!({"value":{"a":1,"b":true}})
    );
}

#[test]
fn finite_template_conversion_is_literal_selective() {
    let mut schema = Schema::typebox(
        json!({"type":"object", "required":["value"], "properties":{"value":{"type":"string", "pattern":"^(true|false)$"}}}),
    );
    schema
        .typebox_kinds
        .insert("/properties/value".into(), "TemplateLiteral".into());
    // With the legacy symbol, Pi skips its second plain-schema conversion
    // pass, exposing the finite template's own selective coercion behavior.
    schema.is_typebox = true;
    let (tool, call) = tool_and_call(schema.clone(), json!({"value":true}));
    assert_eq!(validate(&tool, &call).unwrap(), json!({"value":"true"}));
    let (tool, call) = tool_and_call(schema, json!({"value":123}));
    assert!(
        validate(&tool, &call)
            .unwrap_err()
            .to_string()
            .contains("  - value: must be string")
    );
}

mod javascript_values {
    use super::*;
    use pi_ai::utils::js_json::stringify;
    use pi_ai::utils::js_value::{JsObject, JsString, JsValue};

    fn exact_call(schema: Schema, value: JsValue) -> (Tool, ToolCall) {
        let (tool, mut call) = tool_and_call(schema, json!({}));
        call.arguments = value.into();
        (tool, call)
    }
    fn nested(schema: Value, value: JsValue) -> (Tool, ToolCall) {
        exact_call(
            Schema::json_schema(
                json!({"type":"object","properties":{"v":schema},"required":["v"]}),
            ),
            JsValue::Object(JsObject::from_iter([("v", value)])),
        )
    }

    #[test]
    fn nonfinite_numbers_reach_validation_before_json_observation() {
        for number in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let (tool, call) = nested(json!({"type":"number"}), JsValue::Number(number));
            assert_eq!(
                validate(&tool, &call).unwrap_err().message(),
                "Validation failed for tool \"echo\":\n  - v: must be number\n\nReceived arguments:\n{\n  \"v\": null\n}"
            );
            let (tool, call) = nested(json!({}), JsValue::Number(number));
            let output = validate(&tool, &call).unwrap();
            let actual = output["v"].as_f64().unwrap();
            assert!(actual == number || actual.is_nan() && number.is_nan());
            assert_eq!(stringify(&output), r#"{"v":null}"#);
        }
    }

    #[test]
    fn plain_schema_coerces_nonfinite_numbers_to_javascript_strings() {
        for (number, text) in [
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (f64::NAN, "NaN"),
        ] {
            let (tool, call) = nested(json!({"type":"string"}), JsValue::Number(number));
            assert_eq!(validate(&tool, &call).unwrap()["v"].as_str(), Some(text));
            let mut tool = tool;
            tool.parameters = Schema::typebox(tool.parameters.schema.clone());
            tool.parameters.is_typebox = true;
            assert!(validate(&tool, &call).is_err());
        }
    }

    #[test]
    fn raw_root_conversion_keeps_pis_early_return_behavior() {
        let (tool, call) = exact_call(
            Schema::json_schema(json!({"type":"number","minimum":50})),
            JsValue::String("42".into()),
        );
        assert_eq!(validate(&tool, &call).unwrap().as_str(), Some("42"));
        let (tool, call) = exact_call(
            Schema::json_schema(json!({"type":"number"})),
            JsValue::Number(f64::NAN),
        );
        assert!(validate(&tool, &call).unwrap().as_f64().unwrap().is_nan());
        let (tool, call) = exact_call(Schema::json_schema(json!({"type":"object"})), JsValue::Null);
        assert!(validate(&tool, &call).is_err());
    }

    #[test]
    fn ignored_array_return_still_keeps_referenced_object_mutations() {
        let mut schema = Schema::typebox(json!({"type":"array","items":{"type":"number"}}));
        schema.is_typebox = true;
        let (tool, call) = exact_call(schema, JsValue::Array(vec![JsValue::String("42".into())]));
        assert!(validate(&tool, &call).is_err());
        let mut schema = Schema::typebox(
            json!({"type":"array","items":{"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}}),
        );
        schema.is_typebox = true;
        let (tool, call) = exact_call(
            schema,
            JsValue::from_json_with_js_numbers(json!([{"n":"42"}])),
        );
        assert_eq!(validate(&tool, &call).unwrap(), json!([{"n":42}]));
    }

    #[test]
    fn lone_utf16_units_remain_strings_and_use_typebox_grapheme_count() {
        let value = JsString::from_utf16(vec![0xd800, 0x0301]);
        let (tool, call) = nested(
            json!({"type":"string","minLength":1,"maxLength":1}),
            JsValue::String(value.clone()),
        );
        assert_eq!(
            validate(&tool, &call).unwrap()["v"].as_js_str(),
            Some(&value)
        );
        let (tool, call) = nested(
            json!({"type":"string","maxLength":1}),
            JsValue::String(JsString::from_utf16(vec![0xd800, 0xd800])),
        );
        assert!(validate(&tool, &call).is_err());
    }

    #[test]
    fn schema_patterns_use_unicode_regexp_with_unpaired_surrogates() {
        for pattern in [r"^\uD800$", r"^.$", r"^\p{Cs}$"] {
            let (tool, call) = nested(
                json!({"type":"string","pattern":pattern}),
                JsValue::String(JsString::from_utf16(vec![0xd800])),
            );
            assert!(validate(&tool, &call).is_ok(), "{pattern}");
        }
        let (tool, call) = nested(
            json!({"type":"string","pattern":r"^\uD800$"}),
            JsValue::String("�".into()),
        );
        assert!(validate(&tool, &call).is_err());
    }

    #[test]
    fn exact_error_accessors_preserve_tool_names_and_property_paths() {
        let key = JsString::from_utf16(vec![0xd800]);
        let (tool, mut call) = exact_call(
            Schema::json_schema(json!({"type":"object","additionalProperties":{"type":"number"}})),
            JsValue::Object(JsObject::from_iter([(
                key.clone(),
                JsValue::String("bad".into()),
            )])),
        );
        call.name = JsString::from_utf16(vec![120, 0xdc00]);
        let error = validate(&tool, &call).unwrap_err().into_message();
        let mut expected = JsString::from("Validation failed for tool \"x");
        expected.push(&JsString::from_utf16(vec![0xdc00]));
        expected.push_str("\":\n  - ");
        expected.push(&key);
        expected.push_str(": must be number\n  - root: must not have additional properties\n\nReceived arguments:\n{\n  \"\\ud800\": \"bad\"\n}");
        assert_eq!(error, expected);
        let missing = validate_tool_call(&[tool], &call)
            .unwrap_err()
            .into_message();
        let mut expected = JsString::from("Tool \"");
        expected.push(&call.name);
        expected.push_str("\" not found");
        assert_eq!(missing, expected);
    }

    #[test]
    fn unique_items_uses_typebox_hashing_of_numbers_and_utf16_text() {
        let schema = json!({"type":"array","uniqueItems":true});
        let (tool, call) = nested(
            schema.clone(),
            JsValue::Array(vec![JsValue::Number(0.0), JsValue::Number(-0.0)]),
        );
        assert!(validate(&tool, &call).is_ok());
        let (tool, call) = nested(
            schema,
            JsValue::Array(vec![
                JsValue::String(JsString::from_utf16(vec![0xd800])),
                JsValue::String("�".into()),
            ]),
        );
        assert!(validate(&tool, &call).is_err());
    }

    #[test]
    fn utf16_formats_preserve_units_until_native_url_conversion() {
        for (format, text, expected) in [
            ("email", "\"~\"@example.com", true),
            ("email", "~@example.com", false),
            ("idn-email", "e\u{301}~@example.com", true),
            ("iri", "http://example.com/~", true),
            ("uri", "http://example.com/~", false),
            ("url", "http://~.com", false),
            ("json-pointer", "/~", true),
            ("unknown-format", "~", true),
        ] {
            let units: Vec<_> = text
                .encode_utf16()
                .map(|unit| if unit == 126 { 0xd800 } else { unit })
                .collect();
            let value = JsString::from_utf16(units);
            let (tool, call) = nested(
                json!({"type":"string","format":format}),
                JsValue::String(value.clone()),
            );
            let result = validate(&tool, &call);
            assert_eq!(result.is_ok(), expected, "{format}: {text}");
            if let Ok(output) = result {
                assert_eq!(output["v"].as_js_str(), Some(&value));
            }
        }
    }
}

mod javascript_schemas {
    use super::*;
    use pi_ai::utils::js_json::stringify;
    use pi_ai::utils::js_value::from_json;
    use pi_ai::utils::js_value::{JsString, JsValue};
    use pi_ai::utils::json_parse::parse_json;

    fn raw(schema: &str, args: &str) -> (Tool, ToolCall) {
        let (mut tool, mut call) = tool_and_call(Schema::default(), json!({}));
        tool.parameters = from_json(schema).unwrap();
        call.arguments = parse_json(args).unwrap().into();
        (tool, call)
    }

    #[test]
    fn schema_deserialization_preserves_nonfinite_and_utf16_literals() {
        let schema: Schema = from_json(r#"{"const":1e400,"examples":["\ud800"]}"#).unwrap();
        assert_eq!(schema.schema["const"].as_f64(), Some(f64::INFINITY));
        assert_eq!(
            schema.schema["examples"][0].as_js_str().unwrap().as_utf16(),
            vec![0xd800]
        );
        assert_eq!(
            stringify(&schema.schema),
            r#"{"const":null,"examples":["\ud800"]}"#
        );
        for (schema, args) in [
            (r#"{"const":1e400}"#, "1e400"),
            (r#"{"const":"\ud800"}"#, r#""\ud800""#),
            (r#"{"type":"string","pattern":"^\ud800$"}"#, r#""\ud800""#),
        ] {
            let (tool, call) = raw(schema, args);
            assert_eq!(validate(&tool, &call).unwrap(), call.arguments.snapshot());
        }
    }

    #[test]
    fn nonfinite_schema_constraints_keep_typebox_finite_guards() {
        for schema in [
            r#"{"type":"number","minimum":1e400}"#,
            r#"{"type":"number","maximum":-1e400}"#,
        ] {
            let (tool, call) = raw(schema, "42");
            assert_eq!(validate(&tool, &call).unwrap(), json!(42));
        }
        let (tool, call) = raw(r#"{"const":1e400}"#, "null");
        assert!(validate(&tool, &call).is_err());
    }

    #[test]
    fn raw_property_metadata_and_required_error_names_retain_utf16() {
        let (mut tool, call) = raw(
            r#"{"type":"object","properties":{"\ud800":{"type":"number"}},"required":["\ud800"]}"#,
            r#"{"\ud800":"TRUE"}"#,
        );
        tool.parameters = Schema::typebox(tool.parameters.schema);
        let mut pointer = JsString::from("/properties/");
        pointer.push(&JsString::from_utf16(vec![0xd800]));
        assert_eq!(
            tool.parameters
                .typebox_kinds
                .get(&pointer)
                .map(String::as_str),
            Some("Number")
        );
        let output = validate(&tool, &call).unwrap();
        assert_eq!(
            output
                .as_object()
                .unwrap()
                .get(JsString::from_utf16(vec![0xd800]))
                .unwrap(),
            &JsValue::Number(1.0)
        );
        let (tool, call) = raw(r#"{"type":"object","required":["a/\ud800"]}"#, "{}");
        let mut expected = JsString::from("Validation failed for tool \"echo\":\n  - a/");
        expected.push(&JsString::from_utf16(vec![0xd800]));
        expected.push_str(": must have required properties a/");
        expected.push(&JsString::from_utf16(vec![0xd800]));
        expected.push_str("\n\nReceived arguments:\n{}");
        assert_eq!(validate(&tool, &call).unwrap_err().into_message(), expected);
    }

    #[test]
    fn record_patterns_compile_as_ecmascript_and_conversion_keeps_source_flags() {
        let (mut tool, call) = raw(
            r#"{"type":"object","patternProperties":{"^\\uD800$":{"type":"number"}}}"#,
            r#"{"\ud800":"42"}"#,
        );
        tool.parameters = Schema::typebox(tool.parameters.schema);
        assert_eq!(
            stringify(&validate(&tool, &call).unwrap()),
            r#"{"\ud800":42}"#
        );
        let (mut tool, call) = raw(
            r#"{"type":"object","patternProperties":{"^.$":{"type":"number"}}}"#,
            r#"{"😀":"42"}"#,
        );
        tool.parameters = Schema::typebox(tool.parameters.schema);
        tool.parameters.is_typebox = true;
        // Validation uses 'u' and therefore matches this key. Convert uses no
        // flags and leaves its value unchanged because the key has two units.
        assert!(validate(&tool, &call).is_err());
    }

    #[test]
    fn schema_ref_url_boundary_preserves_pis_unresolvable_result() {
        let (tool, call) = raw(
            r##"{"type":"object","properties":{"v":{"$ref":"#/$defs/\ud800"}},"$defs":{"\ud800":{"type":"string"}}}"##,
            r#"{"v":"\ud800"}"#,
        );
        assert_eq!(
            validate(&tool, &call).unwrap_err().message(),
            "Validation failed for tool \"echo\":\n  - v: schema is false\n\nReceived arguments:\n{\n  \"v\": \"\\ud800\"\n}"
        );
    }

    #[test]
    fn raw_schema_compatibility_path_also_denies_external_references() {
        for reference in ["https://example.invalid/schema.json", "file:///etc/passwd"] {
            let schema = format!(r#"{{"$ref":"{reference}","const":"\ud800"}}"#);
            let (tool, call) = raw(&schema, r#""\ud800""#);
            assert!(
                validate(&tool, &call)
                    .unwrap_err()
                    .to_string()
                    .contains("offline")
            );
        }
    }
}
