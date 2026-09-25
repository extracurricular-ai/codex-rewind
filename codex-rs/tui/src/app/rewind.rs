//! File-aware rewind branches preserve the source conversation for `/redo`.

use super::session_lifecycle::ThreadAttachPresentation;
use super::*;
use crate::app_server_session::ForkGoalContinuation;
use crate::chatwidget::UserMessage;

impl App {
    pub(super) async fn fork_for_rewind(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        selected_cell: Arc<dyn HistoryCell>,
        mut prompt: UserMessage,
        restore_files: bool,
    ) {
        if self.chat_widget.thread_id() != Some(thread_id) {
            return;
        }
        if self.app_server_target.uses_remote_workspace()
            && (!prompt.local_images.is_empty() || prompt.text.trim_start().starts_with(['/', '!']))
        {
            self.chat_widget.add_error_message(
                "This remote prompt contains local image paths or command syntax that cannot be restored safely. Write a new message and reattach any images.".into(),
            );
            tui.frame_requester().schedule_frame();
            return;
        }
        if self.pending_server_profiles.contains_key(&thread_id) {
            self.chat_widget.restore_user_message_to_composer(prompt);
            self.chat_widget.add_error_message(
                "Wait for permissions to update before editing this prompt.".into(),
            );
            tui.frame_requester().schedule_frame();
            return;
        }
        let Some(index) = self
            .transcript_cells
            .iter()
            .position(|cell| Arc::ptr_eq(cell, &selected_cell))
        else {
            self.restore_backtrack_prompt_after_revert_error(
                prompt,
                "the selected prompt is no longer visible",
            );
            tui.frame_requester().schedule_frame();
            return;
        };
        let nth_user_message = crate::app_backtrack::user_count(&self.transcript_cells[..index]);
        let selection: Result<(String, bool)> = async {
            let channel = self.thread_event_channels.get(&thread_id).ok_or_else(|| {
                color_eyre::eyre::eyre!("the selected thread is no longer available")
            })?;
            let (start_item, loaded_tail, latest_turn_id) = {
                let store = channel.store.lock().await;
                (
                    store.turns.iter().find_map(|turn| {
                        turn.items
                            .first()
                            .map(|item| (turn.id.clone(), item.id().to_string()))
                    }),
                    store.turns.last().map(|turn| turn.id.clone()),
                    store.latest_turn_id.clone(),
                )
            };
            let mut thread = app_server
                .thread_read(thread_id, /*include_turns*/ false)
                .await?;
            app_server
                .hydrate_initial_thread_history(
                    &mut thread,
                    /*turn_cursor*/ None,
                    /*item_cursor*/ None,
                    /*config*/ None,
                    /*local_settings*/ None,
                    start_item
                        .as_ref()
                        .map(|(turn_id, _)| turn_id)
                        .or(loaded_tail.as_ref())
                        .map_or(
                            crate::app_server_session::HistoryHydrationScope::Complete,
                            |turn_id| {
                                crate::app_server_session::HistoryHydrationScope::ThroughTurn(
                                    turn_id,
                                )
                            },
                        ),
                )
                .await?;
            if thread.turns.last().map(|turn| &turn.id) != latest_turn_id.as_ref() {
                color_eyre::eyre::bail!(
                    "thread history changed; reload the session before editing this prompt"
                );
            }
            // With no retained visible items, the next prompt follows the metadata-only tail.
            let start_item = start_item.or_else(|| {
                loaded_tail.as_ref().and_then(|tail| {
                    thread
                        .turns
                        .iter()
                        .position(|turn| &turn.id == tail)
                        .and_then(|index| {
                            thread.turns[index + 1..].iter().find_map(|turn| {
                                turn.items
                                    .first()
                                    .map(|item| (turn.id.clone(), item.id().to_string()))
                            })
                        })
                })
            });
            let before_turn_id = crate::app_backtrack::backtrack_revert_before_turn_id(
                &thread.turns,
                start_item.as_ref(),
                nth_user_message,
                &mut prompt,
            )?;
            let is_first = thread
                .turns
                .first()
                .is_some_and(|turn| turn.id == before_turn_id)
                && !app_server.has_older_history(thread_id);
            Ok((before_turn_id, is_first))
        }
        .await;
        let (before_turn_id, is_first) = match selection {
            Ok(selection) => selection,
            Err(err) => {
                self.restore_backtrack_prompt_after_revert_error(prompt, err);
                tui.frame_requester().schedule_frame();
                return;
            }
        };
        self.refresh_in_memory_config_from_disk_best_effort("rewinding the thread")
            .await;
        let mut config = self.config.clone();
        config.model = Some(self.chat_widget.current_model().to_string());
        config.model_reasoning_effort = self.chat_widget.current_reasoning_effort();
        config
            .workspace_roots
            .clone_from(&self.chat_widget.config_ref().workspace_roots);
        let selected_profile = self.confirmed_server_profile(thread_id);
        let unrestorable = restore_files
            .then(|| crate::app_backtrack::unrestorable_notice(&config, thread_id, &before_turn_id))
            .flatten();
        let started = async {
            if is_first {
                if restore_files {
                    app_server
                        .thread_restore_files_to_turn(thread_id, before_turn_id)
                        .await?;
                }
                app_server
                    .start_thread_with_session_start_source(
                        &self.local_settings,
                        &config,
                        /*session_start_source*/ None,
                        /*remote_cwd_override*/ None,
                        selected_profile.as_ref(),
                    )
                    .await
            } else {
                app_server
                    .fork_thread_at(
                        &self.local_settings,
                        config,
                        thread_id,
                        /*last_turn_id*/ None,
                        Some(before_turn_id),
                        ForkGoalContinuation::StartIfIdle,
                        selected_profile.as_ref(),
                        restore_files,
                    )
                    .await
            }
        }
        .await;
        match started {
            Ok(forked) => {
                self.shutdown_current_thread(app_server).await;
                match self
                    .replace_chat_widget_with_app_server_thread(
                        tui,
                        forked,
                        ThreadAttachPresentation::Rewind,
                        /*initial_user_message*/ None,
                    )
                    .await
                {
                    Ok(()) => {
                        self.chat_widget.restore_user_message_to_composer(prompt);
                        self.chat_widget.add_info_message(
                            "You’re continuing from this point in a new conversation".to_string(),
                            /*hint*/ None,
                        );
                        self.retire_rewound_thread(app_server, thread_id).await;
                        if let Some((message, hint)) = unrestorable {
                            self.chat_widget.add_info_message(message, Some(hint));
                        }
                    }
                    Err(err) => self.restore_backtrack_prompt_after_revert_error(prompt, err),
                }
            }
            Err(err) => self.restore_backtrack_prompt_after_revert_error(prompt, err),
        }
        tui.frame_requester().schedule_frame();
    }

    /// Hide a thread a rewind superseded. Archiving keeps the rollout (so
    /// `/redo` can return to it) while keeping it out of `/resume`, where a
    /// growing pile of near-identical branches is only noise.
    pub(super) async fn retire_rewound_thread(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        if let Err(err) = app_server.thread_archive(thread_id).await {
            // Cosmetic: the rewind itself already succeeded.
            tracing::warn!("could not archive the thread a rewind replaced: {err}");
        }
    }
}
