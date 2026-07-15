use serde_json::Value;

fn jsonl(name: &str) -> Vec<Value> {
    let raw = match name {
        "codex" => include_str!("fixtures/codex_token_count.jsonl"),
        "claude" => include_str!("fixtures/claude_assistant.jsonl"),
        _ => panic!("unknown fixture"),
    };
    raw.lines()
        .map(|line| serde_json::from_str(line).expect("fixture line is valid JSON"))
        .collect()
}

#[test]
fn codex_cached_input_is_a_subset_of_input() {
    let rows = jsonl("codex");
    let usage = &rows[3]["payload"]["info"]["total_token_usage"];
    let input = usage["input_tokens"].as_u64().unwrap();
    let cached = usage["cached_input_tokens"].as_u64().unwrap();
    let output = usage["output_tokens"].as_u64().unwrap();
    let total = usage["total_tokens"].as_u64().unwrap();

    assert!(cached <= input);
    assert_eq!(total, input + output);
    assert_ne!(total, input + cached + output);
}

#[test]
fn claude_cache_categories_are_separate_usage_fields() {
    let rows = jsonl("claude");
    let usage = &rows[0]["message"]["usage"];
    let sum = usage["input_tokens"].as_u64().unwrap()
        + usage["output_tokens"].as_u64().unwrap()
        + usage["cache_creation_input_tokens"].as_u64().unwrap()
        + usage["cache_read_input_tokens"].as_u64().unwrap();
    assert_eq!(sum, 1_000);
}

#[test]
fn claude_live_fixture_contains_multiple_scoped_limits() {
    let usage: Value = serde_json::from_str(include_str!("fixtures/claude_usage.json")).unwrap();
    let scoped = usage["limits"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|limit| limit["kind"] == "weekly_scoped")
        .count();
    assert_eq!(scoped, 2);
}
