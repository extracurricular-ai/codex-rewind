//! V3 transcript reconciliation across interleaved speakers, reconnects, and turn boundaries.

use super::*;
use codex_protocol::protocol::RealtimeTranscriptDone;
use pretty_assertions::assert_eq;

fn events(transcript_state: RealtimeTranscriptState) -> RealtimeWebsocketEvents {
    let (_, rx_message) = async_channel::unbounded();
    RealtimeWebsocketEvents {
        rx_message,
        pending_events: Arc::new(Mutex::new(VecDeque::new())),
        transcript_state,
        event_parser: RealtimeEventParser::FramelessBidi,
        is_closed: Arc::new(AtomicBool::new(false)),
    }
}

fn entry(role: &str, text: &str) -> RealtimeTranscriptEntry {
    RealtimeTranscriptEntry {
        role: role.to_string(),
        text: text.to_string(),
    }
}

async fn observe(events: &RealtimeWebsocketEvents, mut event: RealtimeEvent) {
    let original = event.clone();
    events
        .update_active_transcript(&mut event, /*frame*/ None)
        .await;
    assert_eq!(
        event, original,
        "live transcript events must pass through unchanged"
    );
}

#[tokio::test]
async fn interleaved_transcripts_reconcile_across_reconnect_in_either_completion_order() {
    for assistant_finishes_first in [false, true] {
        let state = RealtimeTranscriptState::default();
        let first = events(state.clone());
        for event in [
            RealtimeEvent::InputTranscriptDelta(RealtimeTranscriptDelta {
                delta: "What can ".into(),
            }),
            RealtimeEvent::OutputTranscriptDelta(RealtimeTranscriptDelta {
                delta: "A bunch ".into(),
            }),
            RealtimeEvent::InputTranscriptDelta(RealtimeTranscriptDelta {
                delta: "you do?".into(),
            }),
            RealtimeEvent::OutputTranscriptDelta(RealtimeTranscriptDelta {
                delta: "of stuff".into(),
            }),
        ] {
            observe(&first, event).await;
        }
        let reconnected = events(state);
        let user = RealtimeEvent::InputTranscriptDone(RealtimeTranscriptDone {
            text: "What can you do?".into(),
        });
        let assistant = RealtimeEvent::OutputTranscriptDone(RealtimeTranscriptDone {
            text: "A bunch of stuff, including coding.".into(),
        });
        let finals = if assistant_finishes_first {
            [assistant, user]
        } else {
            [user, assistant]
        };
        for event in finals {
            observe(&reconnected, event).await;
        }
        assert_eq!(
            reconnected.take_transcript_tail().await,
            vec![
                entry("user", "What can you do?"),
                entry("assistant", "A bunch of stuff, including coding."),
            ]
        );
        assert_eq!(reconnected.take_transcript_tail().await, vec![]);
    }
}

#[tokio::test]
async fn completed_utterances_keep_repeated_speech_and_final_only_turns_distinct() {
    let events = events(RealtimeTranscriptState::default());
    for role in ["user", "assistant"] {
        for with_delta in [true, true, false] {
            if with_delta {
                let delta = RealtimeTranscriptDelta {
                    delta: "Again".into(),
                };
                observe(
                    &events,
                    if role == "user" {
                        RealtimeEvent::InputTranscriptDelta(delta)
                    } else {
                        RealtimeEvent::OutputTranscriptDelta(delta)
                    },
                )
                .await;
            }
            let done = RealtimeTranscriptDone {
                text: "Again.".into(),
            };
            observe(
                &events,
                if role == "user" {
                    RealtimeEvent::InputTranscriptDone(done)
                } else {
                    RealtimeEvent::OutputTranscriptDone(done)
                },
            )
            .await;
            // An empty delta must not reopen the utterance we just completed.
            let empty = RealtimeTranscriptDelta {
                delta: String::new(),
            };
            observe(
                &events,
                if role == "user" {
                    RealtimeEvent::InputTranscriptDelta(empty)
                } else {
                    RealtimeEvent::OutputTranscriptDelta(empty)
                },
            )
            .await;
        }
        assert_eq!(
            events.take_transcript_tail().await,
            vec![entry(role, "Again."); 3]
        );
    }
}

#[tokio::test]
async fn transcript_budget_uses_frame_recency_without_reordering_speakers() {
    use serde_json::json;

    let old_user = "u".repeat(4000);
    let assistant = "a".repeat(4000);
    let newest_user = "n".repeat(211);
    let current_user = "u".repeat(4160);
    let same_frame_user = "n".repeat(22);
    let unicode_user = "é".repeat(2000);
    let oversized_user = "u".repeat(MAX_ACTIVE_TRANSCRIPT_BYTES);
    let extended_assistant = format!("{assistant}{newest_user}!");
    for (frames, final_assistant, expected) in [
        (
            vec![
                ("user", old_user.as_str(), Some(0)),
                ("assistant", assistant.as_str(), Some(200)),
                ("user", newest_user.as_str(), Some(400)),
                // Commit eviction before the delayed final arrives.
                ("user", "", Some(600)),
            ],
            // A late final must keep the evicted assistant's old raw-frame age.
            Some(assistant.as_str()),
            vec![entry("user", &format!("{old_user}{newest_user}"))],
        ),
        (
            vec![
                ("assistant", assistant.as_str(), Some(0)),
                ("user", current_user.as_str(), Some(200)),
                // The first event already exceeds the budget, before output updates its age.
                ("user", same_frame_user.as_str(), Some(400)),
                // Same-frame input is emitted first; output must not become newer.
                ("assistant", "bbbbbbbbbbbbbbbbbbbb", Some(400)),
            ],
            None,
            vec![
                entry("assistant", &format!("{assistant}bbbbbbbbbbbbbbbbbbbb")),
                entry("user", &format!("…{}{same_frame_user}", "u".repeat(4128))),
            ],
        ),
        (
            vec![
                ("user", old_user.as_str(), Some(0)),
                ("assistant", assistant.as_str(), Some(200)),
                ("user", newest_user.as_str(), Some(400)),
                ("assistant", "b", Some(400)),
            ],
            None,
            vec![
                entry("user", &format!("…{}{newest_user}", "u".repeat(3958))),
                entry("assistant", &format!("{assistant}b")),
            ],
        ),
        (
            vec![
                ("user", old_user.as_str(), Some(0)),
                ("assistant", assistant.as_str(), Some(200)),
                // Reversing the same-frame events must retain the same suffixes.
                ("assistant", "b", Some(400)),
                ("user", newest_user.as_str(), Some(400)),
                // Advancing the frame also commits eviction before the tail is taken.
                ("user", "", Some(600)),
            ],
            None,
            vec![
                entry("user", &format!("…{}{newest_user}", "u".repeat(3958))),
                entry("assistant", &format!("{assistant}b")),
            ],
        ),
        (
            vec![
                ("user", unicode_user.as_str(), Some(0)),
                ("assistant", assistant.as_str(), Some(200)),
                ("user", newest_user.as_str(), Some(400)),
                ("assistant", newest_user.as_str(), Some(400)),
            ],
            None,
            vec![
                entry("user", &format!("…{}{newest_user}", "é".repeat(1937))),
                entry("assistant", &format!("…{}{newest_user}", "a".repeat(3871))),
            ],
        ),
        (
            vec![
                ("user", oversized_user.as_str(), Some(0)),
                ("assistant", "b", Some(0)),
            ],
            None,
            vec![
                entry("user", &format!("…{}", "u".repeat(8169))),
                entry("assistant", "b"),
            ],
        ),
        (
            vec![
                ("assistant", assistant.as_str(), Some(0)),
                ("user", old_user.as_str(), Some(200)),
                ("assistant", newest_user.as_str(), Some(400)),
                ("user", "b", Some(400)),
                // Trim the tied utterances before a final extends the retained suffix.
                ("user", "", Some(600)),
            ],
            Some(extended_assistant.as_str()),
            vec![
                entry("assistant", &format!("…{}{newest_user}!", "a".repeat(3957))),
                entry("user", &format!("{old_user}b")),
            ],
        ),
        (
            vec![
                ("user", old_user.as_str(), None),
                ("assistant", assistant.as_str(), None),
                ("user", newest_user.as_str(), None),
            ],
            None,
            vec![entry("assistant", &assistant)],
        ),
        (
            vec![
                ("assistant", "I will", Some(0)),
                ("user", "Stop!", Some(200)),
                ("assistant", " do it", Some(200)),
            ],
            Some("I will do it"),
            vec![entry("assistant", "I will do it"), entry("user", "Stop!")],
        ),
    ] {
        let state = RealtimeTranscriptState::default();
        let mut events = events(state.clone());
        let (sender, receiver) = async_channel::unbounded();
        events.rx_message = receiver;
        for (role, text, start_ms) in frames {
            let event_type = if role == "user" {
                "input_transcript.added"
            } else {
                "output_transcript.added"
            };
            let mut payload = json!({"type": event_type, "item": {"text": text}});
            if let Some(start_ms) = start_ms {
                payload["start_ms"] = json!(start_ms);
                payload["end_ms"] = json!(start_ms + 200);
            }
            sender
                .send(Ok(Message::Text(payload.to_string().into())))
                .await
                .unwrap();
            let delta = RealtimeTranscriptDelta { delta: text.into() };
            assert_eq!(
                events.next_event().await.unwrap(),
                Some(if role == "user" {
                    RealtimeEvent::InputTranscriptDelta(delta)
                } else {
                    RealtimeEvent::OutputTranscriptDelta(delta)
                }),
            );
        }
        if let Some(text) = final_assistant {
            sender.send(Ok(Message::Text(json!({
                "type": "turn.done",
                "turn": {"role": "assistant", "transcript": text, "start_ms": 0, "end_ms": 800}
            }).to_string().into()))).await.unwrap();
            assert_eq!(
                events.next_event().await.unwrap(),
                Some(RealtimeEvent::OutputTranscriptDone(
                    RealtimeTranscriptDone { text: text.into() }
                )),
            );
        }
        assert_eq!(state.take_transcript_tail().await, expected);
        assert_eq!(state.take_transcript_tail().await, vec![]);
    }
}
