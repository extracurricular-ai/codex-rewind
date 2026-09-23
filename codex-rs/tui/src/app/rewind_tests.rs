use super::*;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ImageReference;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn legacy_rewind_without_cached_turns() -> Result<()> {
    rewind_without_cached_turns(
        codex_app_server_protocol::ThreadHistoryMode::Legacy,
        /*nth*/ 1,
    )
    .await
}

#[tokio::test]
async fn paginated_rewind_without_cached_turns() -> Result<()> {
    rewind_without_cached_turns(
        codex_app_server_protocol::ThreadHistoryMode::Paginated,
        /*nth*/ 1,
    )
    .await
}

#[tokio::test]
async fn first_prompt_rewind_restores_files_and_starts_fresh() -> Result<()> {
    rewind_without_cached_turns(
        codex_app_server_protocol::ThreadHistoryMode::Paginated,
        /*nth*/ 0,
    )
    .await
}

async fn rewind_without_cached_turns(
    history_mode: codex_app_server_protocol::ThreadHistoryMode,
    nth: usize,
) -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let workspace = tempfile::tempdir()?;
    let mut config = app.chat_widget.config_ref().clone();
    config.cwd = workspace.path().abs();
    config.workspace_roots = vec![config.cwd.clone()];
    app.config = config.clone();
    let filename_ts = "2025-01-05T12-00-00";
    let create_rollout = match history_mode {
        codex_app_server_protocol::ThreadHistoryMode::Legacy => {
            app_test_support::create_fake_rollout
        }
        codex_app_server_protocol::ThreadHistoryMode::Paginated => {
            app_test_support::create_fake_paginated_rollout
        }
    };
    let source_thread_id = create_rollout(
        config.codex_home.as_path(),
        filename_ts,
        "2025-01-05T12:00:00Z",
        "unused preview",
        Some("test-provider"),
        /*git_info*/ None,
    )
    .expect("materialized rollout should be created");
    let source_path =
        app_test_support::rollout_path(config.codex_home.as_path(), filename_ts, &source_thread_id);
    let session_meta = std::fs::read_to_string(&source_path)?
        .lines()
        .next()
        .expect("fake rollout should have session metadata")
        .to_string();
    let mut session_meta: serde_json::Value = serde_json::from_str(&session_meta)?;
    session_meta["payload"]["cwd"] = serde_json::to_value(&config.cwd)?;
    std::fs::write(&source_path, format!("{session_meta}\n"))?;
    for (turn_id, message, images, local_images) in [
        ("turn-1", "retained prompt", None, Vec::new()),
        (
            "turn-2",
            "selected prompt [Image #1]",
            Some(vec!["https://example.com/backtrack.png".to_string()]),
            vec![PathBuf::from("/tmp/fake-image.png")],
        ),
    ] {
        let legacy_message = codex_protocol::protocol::UserMessageEvent {
            message: message.to_string(),
            images: images.clone(),
            local_images: local_images.clone(),
            ..Default::default()
        };
        let mut content = vec![UserInput::Text {
            text: message.to_string(),
            text_elements: Vec::new(),
        }];
        content.extend(
            images
                .into_iter()
                .flatten()
                .map(|image_url| UserInput::Image {
                    image: ImageReference::Inline { image_url },
                    detail: None,
                }),
        );
        content.extend(
            local_images
                .into_iter()
                .map(|path| UserInput::LocalImage { path, detail: None }),
        );
        for item in [
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: turn_id.to_string(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: ModeKind::default(),
            })),
            RolloutItem::EventMsg(match history_mode {
                codex_app_server_protocol::ThreadHistoryMode::Legacy => {
                    EventMsg::UserMessage(legacy_message)
                }
                codex_app_server_protocol::ThreadHistoryMode::Paginated => {
                    EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id: ThreadId::from_string(&source_thread_id)?,
                        turn_id: turn_id.to_string(),
                        item: TurnItem::UserMessage(UserMessageItem {
                            id: format!("user-{turn_id}"),
                            client_id: None,
                            content,
                        }),
                        started_at_ms: None,
                        completed_at_ms: 0,
                    })
                }
            }),
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn_id.to_string(),
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            })),
        ] {
            codex_rollout::append_rollout_item_to_path(&source_path, &item).await?;
        }
    }

    let file = config.cwd.join("rewind-notes.txt");
    let store = codex_file_snapshots::SnapshotStore::open(
        config.codex_home.as_path().join("file_snapshots"),
    )?;
    std::fs::write(&file, "before selected prompt")?;
    store.checkpoint(&source_thread_id, "turn-1", vec![file.to_path_buf()])?;
    store.checkpoint(&source_thread_id, "turn-2", vec![file.to_path_buf()])?;
    std::fs::write(&file, "after selected prompt")?;

    let source_thread_id = ThreadId::from_string(&source_thread_id)?;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(&config)).await?;
    let started = app_server
        .resume_thread(
            &app.local_settings,
            config.clone(),
            source_thread_id,
            crate::app_server_session::ResumeModelSettings::OverrideFromCurrentConfig,
        )
        .await?;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    while let Ok(event) = app_event_rx.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            app.transcript_cells.push(Arc::from(cell));
        }
    }
    // Keep the displayed prompts and live tail identity, but drop the loaded turns.
    // Rewind must resolve the selection against authoritative server history.
    app.thread_event_channels
        .get(&source_thread_id)
        .unwrap()
        .store
        .lock()
        .await
        .turns
        .clear();
    let selected_cell =
        Arc::clone(&app.transcript_cells[nth_user_position(&app.transcript_cells, nth).unwrap()]);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let prompt = if nth == 0 {
        crate::chatwidget::UserMessage::from("retained prompt")
    } else {
        crate::chatwidget::UserMessage {
            text: "selected prompt [Image #1]".to_string(),
            local_images: vec![crate::bottom_pane::LocalImageAttachment {
                placeholder: "[Image #1]".to_string(),
                path: PathBuf::from("/tmp/fake-image.png"),
            }],
            remote_image_urls: vec!["https://example.com/backtrack.png".to_string()],
            text_elements: Vec::new(),
            mention_bindings: Vec::new(),
        }
    };

    let control = Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::ForkSessionForPromptEdit {
            thread_id: source_thread_id,
            selected_cell,
            prompt: prompt.clone(),
            restore_files: true,
        },
    ))
    .await?;

    assert!(matches!(control, AppRunControl::Continue));
    let forked_thread_id = app
        .chat_widget
        .thread_id()
        .expect("prompt edit should switch to a forked thread");
    assert_ne!(forked_thread_id, source_thread_id);
    assert_eq!(std::fs::read_to_string(&file)?, "before selected prompt");
    assert_eq!(app.chat_widget.composer_text_with_pending(), prompt.text);
    assert_eq!(
        app.chat_widget.remote_image_urls(),
        prompt.remote_image_urls
    );
    // Upstream asserts the source rollout is unchanged *in place*. This build
    // retires it instead: a rewind archives the thread it supersedes, so the
    // file has moved out of `sessions/` by now and reading it here would fail.
    // The property that matters is checked below — the thread survives, and it
    // is simply no longer offered alongside its own continuation.
    assert_eq!(
        app_server
            .thread_read(source_thread_id, /*include_turns*/ true)
            .await?
            .turns
            .iter()
            .map(|turn| turn.id.as_str())
            .collect::<Vec<_>>(),
        vec!["turn-1", "turn-2"]
    );
    if nth == 0 {
        assert_eq!(app.chat_widget.forked_from(), None);
    } else {
        assert_eq!(
            app_server
                .thread_read(forked_thread_id, /*include_turns*/ true)
                .await?
                .turns
                .iter()
                .map(|turn| turn.id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn-1"]
        );
    }
    // Kept on disk for /redo, but retired from the session list, so a rewind
    // reads as one continuing conversation rather than as two branches the
    // user has to tell apart.
    let listed = app_server
        .thread_list(codex_app_server_protocol::ThreadListParams {
            // `archived: false` is the filter `/resume` itself applies, so this
            // asks the same question the user's session list does.
            archived: Some(false),
            cursor: None,
            limit: None,
            sort_key: None,
            sort_direction: None,
            model_providers: None,
            source_kinds: None,
            originators: None,
            section_id: None,
            parent_thread_id: None,
            ancestor_thread_id: None,
            cwd: None,
            use_state_db_only: false,
            search_term: None,
            project_id: None,
        })
        .await?
        .data
        .into_iter()
        .map(|thread| thread.id)
        .collect::<Vec<_>>();
    assert!(
        !listed.contains(&source_thread_id.to_string()),
        "the rewound thread should not still be offered alongside its continuation: {listed:?}"
    );

    let history = std::iter::from_fn(|| app_event_rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => {
                Some(lines_to_single_string(&cell.display_lines(/*width*/ 120)))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let notice_index = history
        .iter()
        .position(|line| line == "• You’re continuing from this point in a new conversation")
        .expect("rewind should emit the branch notice");
    assert!(
        !history
            .iter()
            .any(|line| line.contains("Thread forked from"))
    );
    if nth == 0 {
        insta::assert_snapshot!("first_rewind_history", history[notice_index]);
        assert_eq!(
            store.last_restore_target(&forked_thread_id.to_string())?,
            None
        );
    } else {
        let retained_index = history
            .iter()
            .position(|line| line.contains("retained prompt"))
            .expect("rewind should replay the retained prompt");
        assert!(retained_index < notice_index);
        insta::assert_snapshot!(
            "rewind_retained_history",
            history[retained_index..=notice_index].join("\n")
        );
        Box::pin(app.handle_event(&mut tui, &mut app_server, AppEvent::UndoLastRewind)).await?;
        assert_eq!(std::fs::read_to_string(&file)?, "after selected prompt");
        assert!(
            source_path.exists(),
            "redo unarchives the original conversation"
        );
        assert!(std::iter::from_fn(|| app_event_rx.try_recv().ok()).any(|event| {
            matches!(event, AppEvent::ResumeSessionByIdOrName(id) if id == source_thread_id.to_string())
        }));
    }
    app_server.shutdown().await?;

    Ok(())
}
