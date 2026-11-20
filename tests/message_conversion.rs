//! Offline Chat request -> canonical -> Raycast regression tests.
use raycast2api::core::message::Role;
use raycast2api::core::request::CanonicalRequest;
use raycast2api::protocol::InboundAdapter;
use raycast2api::protocol::chat_completions::ChatCompletionsInbound;
use raycast2api::raycast::builder::build_chat_request;
use serde_json::{Value, json};

fn parse(body: Value) -> Result<CanonicalRequest, raycast2api::error::GatewayError> {
    ChatCompletionsInbound::parse(serde_json::from_value(body).unwrap())
}

#[test]
fn preserves_system_developer_and_conversation_order() {
    let canonical = parse(json!({"model":"m","messages":[
        {"role":"system","content":"be terse"},
        {"role":"developer","content":[{"type":"text","text":"be accurate"}]},
        {"role":"user","content":"q1"},
        {"role":"assistant","content":"a1"},
        {"role":"system","content":"later instruction"},
        {"role":"user","content":[{"type":"text","text":"q"},{"type":"text","text":"2"}]}
    ]}))
    .unwrap();
    assert_eq!(
        canonical.system.as_deref(),
        Some("be terse\nbe accurate\nlater instruction")
    );
    let upstream = serde_json::to_value(build_chat_request(&canonical, "p", "m")).unwrap();
    assert_eq!(
        upstream["additional_system_instructions"],
        "be terse\nbe accurate\nlater instruction"
    );
    let messages = upstream["messages"].as_array().unwrap();
    let roles: Vec<_> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    assert_eq!(messages[0]["content"]["text"], "q1");
    assert_eq!(messages[1]["content"]["text"], "a1");
    assert_eq!(messages[2]["content"]["text"], "q2");
}

#[test]
fn absent_system_retains_upstream_base_prompt() {
    let canonical =
        parse(json!({"model":"m","messages":[{"role":"user","content":"hi"}]})).unwrap();
    assert!(canonical.system.is_none());
    assert_eq!(canonical.messages[0].role, Role::User);
    let upstream = build_chat_request(&canonical, "p", "m");
    assert_eq!(upstream.system_instructions, "markdown");
    // The real client never sends `temperature`; an unset value must be absent
    // from the wire, not defaulted (fingerprint defense, 2026-11-20).
    assert_eq!(upstream.temperature, None);
}

#[test]
fn function_tools_and_parallel_results_round_trip_without_losing_text_or_arguments() {
    for content in [json!(null), json!("Checking")] {
        let canonical = parse(json!({"model":"m", "reasoning_effort":"high",
            "tools":[{"type":"function","function":{"name":"weather","description":"Weather","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"weather"}},
            "messages":[
                {"role":"user","content":"weather?"},
                {"role":"assistant","content":content,"tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"weather","arguments":"{ \"city\": \"SF\" }"}},
                    {"id":"call_2","type":"function","function":{"name":"weather","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"call_2","content":[{"type":"text","text":"sunny"}]},
                {"role":"tool","tool_call_id":"call_1","name":"weather","content":"rainy"}
            ]})).unwrap();
        let upstream = serde_json::to_value(build_chat_request(&canonical, "p", "m")).unwrap();
        assert_eq!(upstream["reasoning_effort"], "high");
        assert_eq!(upstream["tool_choice"], "weather");
        assert_eq!(upstream["tools"][0]["type"], "local_tool");
        assert_eq!(upstream["tools"][0]["function"]["name"], "weather");
        assert_eq!(
            upstream["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{ \"city\": \"SF\" }"
        );
        assert_eq!(
            upstream["messages"][1]["content"]["text"],
            content.as_str().unwrap_or("")
        );
        assert_eq!(upstream["messages"][2]["tool_call_id"], "call_2");
        assert_eq!(upstream["messages"][2]["name"], "weather");
        assert_eq!(upstream["messages"][3]["tool_call_id"], "call_1");
    }
}

#[test]
fn assistant_content_can_be_omitted_on_tool_turns() {
    let request = json!({"model":"m", "messages":[
        {"role":"assistant","tool_calls":[{"type":"function","id":"a","function":{"name":"run","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"a","content":"done"}
    ]});
    assert!(parse(request).is_ok());
}

#[test]
fn only_unroutable_requests_are_rejected_locally() {
    // `model` and a known role are the only things the gateway itself needs.
    assert!(parse(json!({"messages":[{"role":"user","content":"hi"}]})).is_err());
    assert!(parse(json!({"model":" ","messages":[{"role":"user","content":"hi"}]})).is_err());
    assert!(parse(json!({"model":"m","messages":[{"role":"function","content":"hi"}]})).is_err());
}

#[test]
fn passes_requests_through_without_validating_options_or_history() {
    for (key, value) in [
        ("temperature", json!(2.1)),
        ("reasoning_effort", json!("invalid")),
        ("max_tokens", json!(10)),
        ("stop", json!(["END"])),
        ("top_p", json!(0.9)),
        ("n", json!(2)),
        ("response_format", json!({"type":"json_object"})),
        ("parallel_tool_calls", json!(false)),
        ("stream_options", json!({"include_usage":true})),
        ("tool_choice", json!("required")),
        (
            "tool_choice",
            json!({"type":"custom","custom":{"name":"x"}}),
        ),
        (
            "tool_choice",
            json!({"type":"function","function":{"name":"missing"}}),
        ),
        (
            "tools",
            json!([{"type":"function","function":{"name":"run","strict":true}}]),
        ),
        (
            "tools",
            json!([{"type":"function","function":{"name":"bad name"}}]),
        ),
        ("tools", json!([{"type":"custom","custom":{"name":"x"}}])),
    ] {
        let mut body = json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        body[key] = value;
        assert!(parse(body.clone()).is_ok(), "rejected {body}");
    }
    for messages in [
        json!([]),
        json!([{"role":"user","content":null}]),
        json!([{"role":"assistant","content":"hello","tool_calls":[]}]),
        json!([{"role":"user","content":[]}]),
        json!([{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}]}]),
        json!([{"role":"tool","content":"x"}]),
        json!([{"role":"tool","tool_call_id":"unknown","content":"x"}]),
        json!([{"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"run","arguments":"{}"}}]}]),
        json!([{"role":"user","content":"hi","metadata":{"k":"v"}}]),
    ] {
        assert!(
            parse(json!({"model":"m","messages":messages})).is_ok(),
            "rejected {messages}"
        );
    }
}

#[test]
fn non_text_parts_are_dropped_and_text_parts_kept() {
    let canonical = parse(json!({"model":"m","messages":[{"role":"user","content":[
        {"type":"text","text":"look: "},
        {"type":"image_url","image_url":{"url":"https://example.com/a.png"}},
        {"type":"text","text":"ok"}
    ]}]}))
    .unwrap();
    assert_eq!(canonical.messages[0].text, "look: ok");
}

#[test]
fn tool_result_name_falls_back_to_the_matching_call() {
    let canonical = parse(json!({"model":"m","messages":[
        {"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"run","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"a","content":"done"}
    ]}))
    .unwrap();
    assert_eq!(canonical.messages[1].tool_name.as_deref(), Some("run"));
}

#[test]
fn accepts_default_options_and_maps_stream_usage_and_effort() {
    for effort in ["low", "medium", "high", "xhigh"] {
        let canonical = parse(
            json!({"model":"m","messages":[{"role":"user","content":"hi"}],
                "n":1,"top_p":1.0,"frequency_penalty":0,"presence_penalty":0,
                "logprobs":false,"store":false,"parallel_tool_calls":true,
                "response_format":{"type":"text"},"modalities":["text"],
                "stream":true,"stream_options":{"include_usage":true},"reasoning_effort":effort
            }),
        )
        .unwrap();
        assert!(canonical.include_usage);
        assert_eq!(
            build_chat_request(&canonical, "p", "m")
                .reasoning_effort
                .as_deref(),
            Some(effort)
        );
        assert_eq!(canonical.requested_effort.as_deref(), Some(effort));
    }
}
