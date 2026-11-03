use super::*;
use pretty_assertions::assert_eq;
use std::io::Write;

#[tokio::test]
async fn recent_rollout_items_bound_history_and_preserve_newest_records() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("rollout.jsonl");
    let mut file = std::fs::File::create(&path)?;
    let metadata = serde_json::json!({
        "timestamp": "2026-10-12T00:00:00Z",
        "type": "session_meta",
        "payload": codex_rollout::SessionMeta::default()
    });
    writeln!(file, "{metadata}")?;
    let old = serde_json::json!({
        "timestamp": "2026-10-12T00:00:00Z",
        "type": "response_item",
        "payload": {"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "older evidence".repeat(/*n*/ 20_000)}
        ]}
    });
    let old = serde_json::to_string(&old)?;
    for _ in 0..(MAX_ROLLOUT_BYTES / old.len() + 2) {
        writeln!(file, "{old}")?;
    }
    let mut newest = Vec::new();
    for text in ["recent question", "latest correction 🦀"] {
        // Neither an oversized nor a malformed record may hide the newest valid evidence.
        writeln!(file, "{}", "x".repeat(MAX_RECORD_BYTES + 1))?;
        file.write_all(b"\xff\n")?;
        let line = serde_json::json!({
            "timestamp": "2026-10-12T00:00:00Z",
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": text}
            ]}
        });
        writeln!(file, "{line}")?;
        newest.push(codex_rollout::decode_rollout_line(line)?.item);
    }
    drop(file);

    let items = read_recent_rollout_items(&path).await?;
    assert_eq!(
        serde_json::to_value(&items[items.len() - newest.len()..])?,
        serde_json::to_value(&newest)?
    );
    let retained_bytes = items.iter().try_fold(/*init*/ 0, |bytes, item| {
        serde_json::to_vec(item).map(|json| bytes + json.len())
    })?;
    assert!(retained_bytes <= MAX_ROLLOUT_BYTES);
    let older_item_bytes =
        serde_json::to_vec(&codex_rollout::parse_rollout_line(&old)?.item)?.len();
    assert!(retained_bytes + older_item_bytes > MAX_ROLLOUT_BYTES);

    let mut metadata = metadata;
    metadata["payload"]["history_mode"] = serde_json::json!("future");
    std::fs::write(&path, format!("{metadata}\n{old}\n"))?;
    assert!(read_recent_rollout_items(&path).await.is_err());
    Ok(())
}

#[test]
fn extraction_chunks_preserve_unicode_evidence_with_bounded_messages() {
    let evidence = "User correction: 🐈\n".repeat(2_000);
    let mut reconstructed = String::new();
    for message in extraction_messages(&evidence) {
        let ResponseItem::Message { role, content, .. } = message else {
            panic!("message")
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("text")
        };
        assert!(text.len() < 9_000);
        reconstructed.push_str(text);
    }
    assert_eq!(reconstructed, evidence);
}

#[test]
fn classifies_memory_excluded_fragments() {
    let cases = [
        (
            "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
            true,
        ),
        (
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
            true,
        ),
        (
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
            true,
        ),
        (
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
            false,
        ),
        (
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
            false,
        ),
    ];

    for (text, expected) in cases {
        assert_eq!(
            is_memory_excluded_contextual_user_fragment(&ContentItem::InputText {
                text: text.to_string(),
            }),
            expected,
            "{text}",
        );
    }
}
