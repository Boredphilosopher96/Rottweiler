use rw_context::toon::encode;
use serde_json::{Value, json};
use tiktoken_rs::o200k_base;

fn representative_corpus() -> Vec<Value> {
    let search_hits: Vec<_> = (0..40)
        .map(|index| {
            json!({
                "path": format!("crates/rw-core/src/module_{index}.rs"),
                "line": 20 + index,
                "column": 9,
                "kind": "definition",
                "preview": "pub fn assemble_context(input: AssemblyInput) -> Result<Context>"
            })
        })
        .collect();
    let directory_entries: Vec<_> = (0..50)
        .map(|index| {
            json!({
                "name": format!("fixture_{index}.json"),
                "type": "file",
                "size": 1024 + index * 17,
                "modified": "2026-07-10T12:30:00Z"
            })
        })
        .collect();
    let diagnostics: Vec<_> = (0..35)
        .map(|index| {
            json!({
                "file": format!("src/check_{index}.rs"),
                "severity": if index % 2 == 0 { "warning" } else { "error" },
                "code": "E0308",
                "line": 10 + index,
                "message": "mismatched types: expected ContextItem, found Turn"
            })
        })
        .collect();
    let mcp_tools: Vec<_> = (0..30)
        .map(|index| {
            json!({
                "name": format!("workspace_search_{index}"),
                "description": "Search workspace symbols with bounded results",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 100}
                    },
                    "required": ["query"]
                }
            })
        })
        .collect();
    vec![
        json!({"search_hits": search_hits}),
        json!({"directory_entries": directory_entries}),
        json!({"diagnostics": diagnostics}),
        json!({"jsonrpc": "2.0", "id": 7, "result": {"tools": mcp_tools}}),
    ]
}

#[test]
fn toon_saves_at_least_thirty_percent_on_structured_corpus()
-> Result<(), Box<dyn std::error::Error>> {
    let tokenizer = o200k_base()?;
    let mut json_tokens = 0_usize;
    let mut toon_tokens = 0_usize;
    for (corpus_index, value) in representative_corpus().into_iter().enumerate() {
        let json = serde_json::to_string_pretty(&value)?;
        let toon = encode(&value).map_err(|error| {
            std::io::Error::other(format!(
                "representative corpus item {corpus_index} was not TOON-safe: {error}"
            ))
        })?;
        json_tokens = json_tokens.saturating_add(tokenizer.encode_with_special_tokens(&json).len());
        toon_tokens = toon_tokens.saturating_add(tokenizer.encode_with_special_tokens(&toon).len());
    }
    let saved = json_tokens.saturating_sub(toon_tokens);
    println!(
        "TOON structured corpus: JSON={json_tokens}, TOON={toon_tokens}, saved={saved} tokens"
    );
    assert!(
        saved.saturating_mul(100) >= json_tokens.saturating_mul(30),
        "TOON used {toon_tokens} tokens vs JSON {json_tokens}; saved {saved}"
    );
    Ok(())
}
