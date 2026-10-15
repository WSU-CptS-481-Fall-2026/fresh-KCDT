//! Action execution: carrying out a resolved [`Action`] against the editor.
//!
//! Once the input orchestration (`app/input.rs`) — or a menu, a mouse
//! click, a plugin, a macro — has resolved an [`Action`], this is where
//! the editor performs it. Executing an action is inherently high-level
//! `Editor` behaviour (it drives buffers, windows, prompts, plugins), so
//! unlike the decision layers in `crate::input::router` these are methods;
//! the split from `app/input.rs` keeps *deciding what a key means*
//! separate from *doing what the action says*.

use super::*;
use crate::view::confirm::{Choice, Confirm, Tone};
use anyhow::Result as AnyhowResult;
use crossterm::event::KeyModifiers as KM;
use fresh_i18n::t;

impl Editor {
    /// Change the current workspace's trust level, persist it, and report it.
    /// The new policy applies live at the next authority-routed spawn (the
    /// guarding spawners read the level on every spawn) — there is NO editor
    /// restart here, deliberately: a rebuild would reset every other
    /// orchestrator session's buffers/layout (see the body). Trust-gated work
    /// re-triggers via the `trust_changed` hook instead (e.g. env-manager
    /// re-activates a now-trusted env). Already-correct selections (e.g.
    /// confirming the current level) only persist the decision.
    pub(crate) fn set_workspace_trust_level(
        &mut self,
        level: crate::services::workspace_trust::TrustLevel,
    ) {
        use crate::services::workspace_trust::TrustLevel;
        // Trust is a per-window gate: each `Window` owns its own authority +
        // `WorkspaceTrust` (issue #2280), and the guarding spawners read the
        // level live at spawn time. Writing the new level here is the whole
        // change — `set_level` itself documents "no rebuild required". The
        // next authority-routed spawn (LSP, terminal command, task, formatter,
        // plugin `spawnProcess`) honours the new level automatically.
        //
        // We deliberately do NOT `request_restart` here: that tears down and
        // rebuilds the *entire* editor — every orchestrator session window,
        // not just this one — which discarded other sessions' buffers and
        // reset the layout when toggling a single session's trust (the
        // trust-level-modal reset bug).
        let trust = &self.authority().workspace_trust;
        trust.set_level(level);
        let msg = match level {
            TrustLevel::Trusted => t!("trust.now_trusted"),
            TrustLevel::Restricted => t!("trust.now_restricted"),
            TrustLevel::Blocked => t!("trust.now_blocked"),
        }
        .to_string();
        self.active_window_mut().status_message = Some(msg);

        // Refresh the plugin-visible state snapshot so `editor.workspaceTrustLevel()`
        // reflects the new level, then notify plugins. The `trust_changed` hook
        // lets trust-gated work re-trigger inline — env-manager re-activates a
        // now-trusted env without a window switch — and it is the single signal
        // every trust-change path (modal confirm, status pill, plugin action)
        // funnels through, since they all route here. Deliberately a hook and a
        // snapshot refresh, NOT a `request_restart`: a rebuild would reset every
        // other session's buffers/layout (see the note above).
        #[cfg(feature = "plugins")]
        {
            self.update_plugin_state_snapshot();
            self.plugin_manager.read().unwrap().run_hook(
                "trust_changed",
                crate::services::plugins::hooks::HookArgs::TrustChanged {
                    level: level.as_str().to_string(),
                },
            );
        }
    }

    fn handle_paste(&mut self) -> AnyhowResult<()> {
        if self.active_window_mut().key_context
            == crate::input::keybindings::KeyContext::FileExplorer
        {
            self.file_explorer_paste();
            return Ok(());
        }
        // Focused widget Text wins over the buffer paste
        // path, and bypasses `is_editing_disabled`. Line
        // endings get normalised to LF before insertion
        // (multi-line `TextEdit` stores plain `\n`;
        // single-line strips them).
        let buffer_id = self.active_buffer();
        if let Some(panel_id) = self.focused_text_widget_panel_for_buffer(buffer_id) {
            if let Some(text) = self.clipboard.paste() {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                self.handle_widget_insert_str(&panel_id, &normalized);
                self.set_status_message(t!("clipboard.pasted").to_string());
            }
            return Ok(());
        }
        if self.active_window().is_editing_disabled() {
            self.set_status_message(t!("buffer.editing_disabled").to_string());
            return Ok(());
        }
        Ok(self.paste())
    }

    fn handle_copy(&mut self) -> AnyhowResult<()> {
        // Editor-level popups take precedence over everything, including the file explorer.
        let popup = self
            .global_popups
            .top()
            .or_else(|| self.active_state().popups.top());
        if let Some(popup) = popup {
            if popup.has_selection() {
                if let Some(text) = popup.get_selected_text() {
                    self.clipboard.copy(text);
                    self.set_status_message(t!("clipboard.copied").to_string());
                    return Ok(());
                }
            }
        }
        if self.active_window_mut().key_context
            == crate::input::keybindings::KeyContext::FileExplorer
        {
            self.active_window_mut().file_explorer_copy();
            return Ok(());
        }
        // A focused widget Text input on the active buffer
        // wins over the underlying buffer's copy path. The
        // widget's selection lives in its TextEdit; this
        // bypasses `is_editing_disabled` because widget
        // inputs are independent of the underlying virtual
        // buffer's read-only-ness.
        let buffer_id = self.active_buffer();
        if let Some(panel_id) = self.focused_text_widget_panel_for_buffer(buffer_id) {
            if self.handle_widget_copy(&panel_id) {
                self.set_status_message(t!("clipboard.copied").to_string());
                return Ok(());
            }
        }
        // Check if active buffer is a composite buffer
        if self.active_window().is_composite_buffer(buffer_id) {
            if let Some(_handled) = self.handle_composite_action(buffer_id, &Action::Copy) {
                return Ok(());
            }
        }
        // Copying the selection completes a drag-to-select gesture on
        // a live terminal grid: the split was only parked in implicit
        // (drag-initiated) scrollback so the selection could exist,
        // and the copy is the gesture's natural end — resume the live
        // grid. Explicit scrollback visits (Ctrl+Space, Shift+PageUp,
        // wheel) are never resumed by a copy: the user chose a stable
        // reading view and yanking it to the bottom would lose their
        // place.
        let resume_terminal = {
            let win = self.active_window();
            let split = win.effective_active_split();
            win.split_terminal_drag_scrollback(split, buffer_id)
                && win
                    .buffers
                    .splits()
                    .and_then(|(_, vs)| vs.get(&split))
                    .is_some_and(|vs| {
                        vs.cursors
                            .iter()
                            .any(|(_, c)| c.selection_range().is_some() || c.has_block_selection())
                    })
        };
        self.copy_selection();
        if resume_terminal {
            self.enter_terminal_mode();
            self.set_status_message("Copied - terminal resumed".to_string());
        }
        Ok(())
    }

    /// Save the active buffer, prompting for a path or an external file conflict.
    fn handle_dispatch_save(&mut self) {
        // Check if buffer has a file path - if not, redirect to SaveAs
        if self.active_state().buffer.file_path().is_none() {
            self.start_prompt_with_initial_text(
                t!("file.save_as_prompt").to_string(),
                PromptType::SaveFileAs,
                String::new(),
            );
            self.init_file_open_state();
        } else if self.check_save_conflict().is_some() {
            // Check if file was modified externally since we opened/saved it
            let body = t!("file.file_changed_prompt").to_string();
            let confirm = Confirm::new(
                t!("dialog.title.file_changed").into_owned(),
                body.clone(),
                vec![
                    Choice::new(
                        t!("dialog.btn.overwrite").into_owned(),
                        "o",
                        Tone::Destructive,
                    ),
                    crate::app::confirm_dialog::cancel(),
                ],
            )
            .detail(
                self.active_state()
                    .buffer
                    .file_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            );
            self.start_confirm_prompt(body, PromptType::ConfirmSaveConflict, confirm);
        } else if let Err(e) = self.save() {
            let msg = format!("{}", e);
            self.active_window_mut().status_message =
                Some(t!("file.save_failed", error = &msg).to_string());
        }
    }

    /// Open the save-as prompt with the current file path as the suggestion.
    fn handle_dispatch_save_as(&mut self) {
        // Get current filename as default suggestion
        let current_path = self
            .active_state()
            .buffer
            .file_path()
            .map(|p| {
                // Make path relative to working_dir if possible
                p.strip_prefix(self.working_dir())
                    .unwrap_or(p)
                    .to_string_lossy()
                    .to_string()
            })
            .unwrap_or_default();
        self.start_prompt_with_initial_text(
            t!("file.save_as_prompt").to_string(),
            PromptType::SaveFileAs,
            current_path,
        );
        self.init_file_open_state();
    }

    /// Save all modified buffers and report the number saved or failed.
    fn handle_dispatch_save_all(&mut self) {
        let msg = match self.save_all() {
            Ok((saved, failed)) => {
                if failed > 0 {
                    t!(
                        "status.save_all_partial",
                        saved = saved.to_string(),
                        failed = failed.to_string()
                    )
                    .to_string()
                } else if saved == 0 {
                    t!("status.save_all_none").to_string()
                } else {
                    t!("status.save_all", count = saved.to_string()).to_string()
                }
            }
            Err(e) => t!("file.save_failed", error = &format!("{}", e)).to_string(),
        };
        self.active_window_mut().status_message = Some(msg);
    }

    /// Open the go-to-line prompt, or offer to scan an unindexed buffer.
    fn handle_dispatch_goto_line(&mut self) {
        let has_line_index = self.active_buffer_has_line_index();
        if has_line_index {
            self.start_prompt(
                t!("file.goto_line_prompt").to_string(),
                PromptType::GotoLine,
            );
        } else {
            self.start_goto_line_scan_confirm();
        }
    }

    /// Revert the active file, confirming first when it has unsaved changes.
    fn handle_dispatch_revert(&mut self) {
        // Check if buffer has unsaved changes - prompt for confirmation
        if self.active_state().buffer.is_modified() {
            let body = t!("prompt.revert_confirm").to_string();
            let confirm = Confirm::new(
                t!("dialog.title.revert").into_owned(),
                body.clone(),
                vec![
                    Choice::new(
                        t!("dialog.btn.revert").into_owned(),
                        t!("prompt.key.revert").into_owned(),
                        Tone::Destructive,
                    ),
                    crate::app::confirm_dialog::cancel(),
                ],
            )
            .detail(
                self.active_state()
                    .buffer
                    .file_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            )
            // A destructive lead opens on the retreat.
            .selecting(1);
            self.start_confirm_prompt(body, PromptType::ConfirmRevert, confirm);
        } else {
            // No local changes, just revert
            if let Err(e) = self.revert_file() {
                self.set_status_message(
                    t!("error.failed_to_revert", error = e.to_string()).to_string(),
                );
            }
        }
    }

    /// Show the update prompt or output appropriate to the current update phase.
    fn handle_dispatch_update_fresh(&mut self) {
        // Once an update is running or finished, the indicator's job is
        // to surface the update terminal, not to re-offer the update —
        // except at the two terminal states that leave something for
        // the user to act on.
        use crate::services::release_checker::SelfUpdatePhase;
        match self.self_update_phase() {
            // Failed: retry / show-log / cancel.
            SelfUpdatePhase::Failed => self.show_update_failed_popup(),
            // Action required: the update ran cleanly but left a command
            // to run. Surface *that*, rather than re-offering an update
            // that has already been downloaded and verified.
            SelfUpdatePhase::ActionRequired => self.show_update_action_required_popup(),
            SelfUpdatePhase::Running | SelfUpdatePhase::Succeeded => self.show_self_update_output(),
            SelfUpdatePhase::Idle => {
                if !self.config().self_update {
                    self.set_status_message(t!("update.disabled").to_string());
                } else if !self.is_update_available() {
                    self.set_status_message(t!("update.up_to_date").to_string());
                } else {
                    // An update is available — offer it through a popup
                    // built from the resolved update plan, so the
                    // confirmation says how this copy was installed and
                    // what confirming will actually do.
                    let version = self.latest_version().unwrap_or("").to_string();
                    self.show_update_popup(&version);
                }
            }
        }
    }

    /// Format the active buffer when editing is enabled and report errors.
    fn handle_dispatch_format_buffer(&mut self) {
        if self.refuse_if_editing_disabled() {
            return;
        }
        if let Err(e) = self.format_buffer() {
            self.set_status_message(t!("error.format_failed", error = e.to_string()).to_string());
        }
    }

    /// Trim trailing whitespace and report whether the buffer changed.
    fn handle_dispatch_trim_trailing_whitespace(&mut self) {
        if self.refuse_if_editing_disabled() {
            return;
        }
        match self.trim_trailing_whitespace() {
            Ok(true) => {
                self.set_status_message(t!("whitespace.trimmed").to_string());
            }
            Ok(false) => {
                self.set_status_message(t!("whitespace.no_trailing").to_string());
            }
            Err(e) => {
                self.set_status_message(t!("error.trim_whitespace_failed", error = e).to_string());
            }
        }
    }

    /// Ensure the active buffer ends with a newline and report the result.
    fn handle_dispatch_ensure_final_newline(&mut self) {
        if self.refuse_if_editing_disabled() {
            return;
        }
        match self.ensure_final_newline() {
            Ok(true) => {
                self.set_status_message(t!("whitespace.newline_added").to_string());
            }
            Ok(false) => {
                self.set_status_message(t!("whitespace.already_has_newline").to_string());
            }
            Err(e) => {
                self.set_status_message(t!("error.ensure_newline_failed", error = e).to_string());
            }
        }
    }

    /// Cut from the file explorer, focused text widget, or editable buffer.
    fn handle_dispatch_cut(&mut self) {
        if self.active_window_mut().key_context
            == crate::input::keybindings::KeyContext::FileExplorer
        {
            self.active_window_mut().file_explorer_cut();
            return;
        }
        // Focused widget Text wins over the buffer cut path,
        // and bypasses `is_editing_disabled` — widget inputs
        // are independent of the underlying virtual buffer.
        let buffer_id = self.active_buffer();
        if let Some(panel_id) = self.focused_text_widget_panel_for_buffer(buffer_id) {
            if self.handle_widget_cut(&panel_id) {
                return;
            }
        }
        if self.active_window().is_editing_disabled() {
            self.set_status_message(t!("buffer.editing_disabled").to_string());
            return;
        }
        self.cut_selection();
    }

    /// Select all text in the focused widget or active buffer.
    fn handle_dispatch_select_all(&mut self) -> AnyhowResult<()> {
        // Focused widget Text wins over the buffer's
        // select-all. SelectAll on the buffer is then
        // handled by the default `apply_action_as_events`
        // catch-all path below.
        let buffer_id = self.active_buffer();
        if let Some(panel_id) = self.focused_text_widget_panel_for_buffer(buffer_id) {
            self.handle_widget_select_all(&panel_id);
            return Ok(());
        }
        self.apply_action_as_events(Action::SelectAll)?;
        Ok(())
    }

    /// Toggle the command palette through Quick Open.
    fn handle_dispatch_command_palette(&mut self) {
        // CommandPalette now delegates to QuickOpen (which starts with ">" prefix
        // for command mode). Toggle if already open.
        if self.close_quick_open_if_open() {
            return;
        }
        self.start_quick_open();
    }

    /// Focus the utility dock, or leave it when it is already focused.
    fn handle_dispatch_toggle_utility_dock(&mut self) {
        use crate::view::split::SplitRole;
        if let Some(dock_leaf) = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .find_leaf_by_role(SplitRole::UtilityDock)
        {
            let active = self
                .windows
                .get(&self.active_window)
                .and_then(|w| w.buffers.splits())
                .map(|(mgr, _)| mgr)
                .expect("active window must have a populated split layout")
                .active_split();
            if active == dock_leaf {
                // Already focused — no editor-leaf history yet,
                // so just cycle to the next leaf via the
                // existing Alt+] command. Phase 7 will track a
                // proper "previous editor split" pointer.
                self.next_split();
            } else {
                self.windows
                    .get_mut(&self.active_window)
                    .and_then(|w| w.split_manager_mut())
                    .expect("active window must have a populated split layout")
                    .set_active_split(dock_leaf);
            }
        } else {
            self.set_status_message(
                "No Utility Dock open — invoke a dock-aware utility (Diagnostics, Search/Replace, …)"
                    .to_string(),
            );
        }
    }

    /// Cycle the search provider while the Live Grep overlay is open.
    fn handle_dispatch_cycle_live_grep_provider(&mut self) -> AnyhowResult<()> {
        // Only meaningful while the Live Grep overlay is open. Detect via prompt state —
        // both `PromptType::LiveGrep` (Resume's pre-seeded overlay) and
        // `Plugin{custom_type:"live-grep"}` (the live-running plugin's prompt) qualify.
        let in_live_grep = self
            .active_window()
            .prompt
            .as_ref()
            .map(|p| match &p.prompt_type {
                PromptType::LiveGrep => true,
                PromptType::Plugin { custom_type } => custom_type == "live-grep",
                _ => false,
            })
            .unwrap_or(false);
        if !in_live_grep {
            self.set_status_message(
                "Cycle Live Grep provider only works inside Live Grep".to_string(),
            );
            return Ok(());
        }
        self.handle_action(Action::PluginAction("live_grep_cycle_provider".to_string()))?;
        Ok(())
    }

    /// Toggle the global line-wrap setting and update unpinned split views.
    fn handle_dispatch_toggle_line_wrap(&mut self) {
        let new_value = !self.config.editor.line_wrap;
        self.config_mut().editor.line_wrap = new_value;
        // `resolve_line_wrap_for_buffer` below reads
        // `Window::config()`, which holds a *separate* `Arc<Config>`
        // clone from the Editor's. Without this sync the resolve
        // would return the pre-toggle value and we'd write the
        // *old* line-wrap state back into the viewport — silently
        // no-op'ing the toggle while still flipping the status
        // message. See `Editor::config_mut` for the broader rule.
        self.sync_windows_config();

        // Update all viewports to reflect the new line wrap setting,
        // respecting per-language overrides
        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();
        let leaf_ids: Vec<_> = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, vs)| vs)
            .expect("active window must have a populated split layout")
            .keys()
            .copied()
            .collect();
        for leaf_id in leaf_ids {
            let buffer_id = self
                .split_manager_mut()
                .get_buffer_id(leaf_id.into())
                .unwrap_or(BufferId(0));
            let effective_wrap = self.active_window().resolve_line_wrap_for_buffer(buffer_id);
            let wrap_column = self
                .active_window()
                .resolve_wrap_column_for_buffer(buffer_id);
            if let Some(view_state) = self
                .windows
                .get_mut(&self.active_window)
                .and_then(|w| w.split_view_states_mut())
                .expect("active window must have a populated split layout")
                .get_mut(&leaf_id)
            {
                // The active split's own pin is dropped — the user is
                // expressing a global intent on the view in front of
                // them. Every other pinned split keeps its choice: a
                // global default must not silently un-pin work the
                // user did elsewhere (same rule as the highlight
                // toggles below).
                if leaf_id == active_split {
                    view_state.line_wrap_override = None;
                }
                if view_state.line_wrap_override.is_none() {
                    view_state.viewport.line_wrap_enabled = effective_wrap;
                    view_state.viewport.wrap_indent = self.config.editor.wrap_indent;
                    view_state.viewport.wrap_column = wrap_column;
                }
            }
        }

        // `editor.line_wrap` is an editor-wide default, so an
        // unsuffixed toggle saves it to the user config layer — see the
        // scope convention on `COMMANDS` in `input/commands.rs`. The
        // per-buffer variant is "Toggle Line Wrap (Current Buffer)".
        self.persist_config_change(crate::config_keys::EDITOR_LINE_WRAP, new_value);

        let state = if self.config.editor.line_wrap {
            t!("view.state_enabled").to_string()
        } else {
            t!("view.state_disabled").to_string()
        };
        self.set_status_message(t!("view.line_wrap_state", state = state).to_string());
    }

    /// Toggle current-line highlighting while preserving other splits' overrides.
    fn handle_dispatch_toggle_current_line_highlight(&mut self) {
        let new_value = !self.config.editor.highlight_current_line;
        self.config_mut().editor.highlight_current_line = new_value;
        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();

        // Update all splits
        let leaf_ids: Vec<_> = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, vs)| vs)
            .expect("active window must have a populated split layout")
            .keys()
            .copied()
            .collect();
        for leaf_id in leaf_ids {
            if let Some(view_state) = self
                .windows
                .get_mut(&self.active_window)
                .and_then(|w| w.split_view_states_mut())
                .expect("active window must have a populated split layout")
                .get_mut(&leaf_id)
            {
                // The active split's own pin is dropped just below —
                // the user is expressing a global intent on the view in
                // front of them. Every other pinned buffer keeps its
                // choice; a global default must not silently un-pin
                // work the user did elsewhere.
                if leaf_id == active_split {
                    view_state.highlight_current_line_override = None;
                }
                if view_state.highlight_current_line_override.is_none() {
                    view_state.highlight_current_line = self.config.editor.highlight_current_line;
                }
            }
        }

        self.persist_config_change(crate::config_keys::EDITOR_HIGHLIGHT_CURRENT_LINE, new_value);

        let state = if self.config.editor.highlight_current_line {
            t!("view.state_enabled").to_string()
        } else {
            t!("view.state_disabled").to_string()
        };
        self.set_status_message(t!("view.current_line_highlight_state", state = state).to_string());
    }

    /// Toggle occurrence highlighting while preserving other buffers' overrides.
    fn handle_dispatch_toggle_occurrence_highlight(&mut self) {
        let new_value = !self.config.editor.highlight_occurrences;
        self.config_mut().editor.highlight_occurrences = new_value;
        let active_buffer = self.active_buffer();

        // Update all open buffers. A buffer the user pinned with the
        // "(Current Buffer)" variant keeps its choice — except the
        // active one, whose pin is cleared first as a global intent.
        if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&active_buffer)
        {
            state.buffer_settings.highlight_occurrences_override = None;
        }
        for window in self.windows.values_mut() {
            for (_, state) in &mut window.buffers {
                if state
                    .buffer_settings
                    .highlight_occurrences_override
                    .is_some()
                {
                    continue;
                }
                state.reference_highlight_overlay.enabled = new_value;
                if !new_value {
                    state
                        .reference_highlight_overlay
                        .clear(&mut state.overlays, &mut state.marker_list);
                }
            }
        }

        self.persist_config_change(crate::config_keys::EDITOR_HIGHLIGHT_OCCURRENCES, new_value);

        let state = if new_value {
            t!("view.state_enabled").to_string()
        } else {
            t!("view.state_disabled").to_string()
        };
        self.set_status_message(t!("view.occurrence_highlight_state", state = state).to_string());
    }

    /// Toggle the active buffer's read-only state and report it.
    fn handle_dispatch_toggle_read_only(&mut self) {
        let buffer_id = self.active_buffer();
        let is_now_read_only = self
            .active_window()
            .buffer_metadata
            .get(&buffer_id)
            .map(|m| !m.read_only)
            .unwrap_or(false);
        self.active_window_mut()
            .mark_buffer_read_only(buffer_id, is_now_read_only);

        let state_str = if is_now_read_only {
            t!("view.state_enabled").to_string()
        } else {
            t!("view.state_disabled").to_string()
        };
        self.set_status_message(t!("view.read_only_state", state = state_str).to_string());
    }

    /// Open the page-width prompt with the active split's current width.
    fn handle_dispatch_set_page_width(&mut self) {
        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();
        let current = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, vs)| vs)
            .expect("active window must have a populated split layout")
            .get(&active_split)
            .and_then(|v| v.compose_width.map(|w| w.to_string()))
            .unwrap_or_default();
        self.start_prompt_with_initial_text(
            "Page width (empty = viewport): ".to_string(),
            PromptType::SetPageWidth,
            current,
        );
    }

    /// Open the background-file prompt with the current background path.
    fn handle_dispatch_set_background(&mut self) {
        let default_path = self
            .ansi_background_path
            .as_ref()
            .and_then(|p| {
                p.strip_prefix(self.working_dir())
                    .ok()
                    .map(|rel| rel.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| DEFAULT_BACKGROUND_FILE.to_string());

        self.start_prompt_with_initial_text(
            "Background file: ".to_string(),
            PromptType::SetBackgroundFile,
            default_path,
        );
    }

    /// Open the background-blend prompt with the current blend amount.
    fn handle_dispatch_set_background_blend(&mut self) {
        let default_amount = format!("{:.2}", self.background_fade);
        self.start_prompt_with_initial_text(
            "Background blend (0-1): ".to_string(),
            PromptType::SetBackgroundBlend,
            default_amount,
        );
    }

    /// Confirm an active search prompt, or open a new search prompt.
    fn handle_dispatch_search(&mut self) {
        // If already in a search-related prompt, Ctrl+F acts like Enter (confirm search)
        let is_search_prompt = self.active_window().prompt.as_ref().is_some_and(|p| {
            matches!(
                p.prompt_type,
                PromptType::Search | PromptType::ReplaceSearch | PromptType::QueryReplaceSearch
            )
        });

        if is_search_prompt {
            self.confirm_prompt();
        } else {
            self.start_search_prompt(
                t!("file.search_prompt").to_string(),
                PromptType::Search,
                false,
            );
        }
    }

    /// Open the replace prompt when editing is enabled.
    fn handle_dispatch_replace(&mut self) {
        if self.refuse_if_editing_disabled() {
            return;
        }
        // Use same flow as query-replace, just with confirm_each defaulting to false
        self.start_search_prompt(
            t!("file.replace_prompt").to_string(),
            PromptType::ReplaceSearch,
            false,
        );
    }

    /// Open the query-replace prompt with confirmation enabled.
    fn handle_dispatch_query_replace(&mut self) {
        if self.refuse_if_editing_disabled() {
            return;
        }
        // Enable confirm mode by default for query-replace
        self.active_window_mut().search_confirm_each = true;
        self.start_search_prompt(
            "Query replace: ".to_string(),
            PromptType::QueryReplaceSearch,
            false,
        );
    }

    /// Scroll the active split's tab bar to the left.
    fn handle_dispatch_scroll_tabs_left(&mut self) {
        let active_split_id = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();
        if let Some(view_state) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_view_states_mut())
            .expect("active window must have a populated split layout")
            .get_mut(&active_split_id)
        {
            view_state.tab_scroll_offset = view_state.tab_scroll_offset.saturating_sub(5);
            self.set_status_message(t!("status.scrolled_tabs_left").to_string());
        }
    }

    /// Scroll the active split's tab bar to the right.
    fn handle_dispatch_scroll_tabs_right(&mut self) {
        let active_split_id = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();
        if let Some(view_state) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_view_states_mut())
            .expect("active window must have a populated split layout")
            .get_mut(&active_split_id)
        {
            view_state.tab_scroll_offset = view_state.tab_scroll_offset.saturating_add(5);
            self.set_status_message(t!("status.scrolled_tabs_right").to_string());
        }
    }

    /// Open the tab-size prompt with the active buffer's current tab size.
    fn handle_dispatch_set_tab_size(&mut self) {
        let current = self
            .buffers()
            .get(&self.active_buffer())
            .map(|s| s.buffer_settings.tab_size.to_string())
            .unwrap_or_else(|| "4".to_string());
        self.start_prompt_with_initial_text(
            "Tab size: ".to_string(),
            PromptType::SetTabSize,
            current,
        );
    }

    /// Toggle the active buffer between tab and space indentation.
    fn handle_dispatch_toggle_indentation_style(&mut self) {
        let __buffer_id = self.active_buffer();
        if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&__buffer_id)
        {
            let new_value = !state.buffer_settings.use_tabs;
            state.buffer_settings.use_tabs = new_value;
            // Record the explicit override so a later `apply_config`
            // (config reload, Set Language, save-time detection) can't
            // re-stamp the language default over the user's choice —
            // and so it can be persisted per file.
            state.buffer_settings.use_tabs_override = Some(new_value);
            let status = if state.buffer_settings.use_tabs {
                "Indentation: Tabs"
            } else {
                "Indentation: Spaces"
            };
            self.set_status_message(status.to_string());
        }
    }

    /// Toggle whitespace indicators using the configured space markers.
    fn handle_dispatch_toggle_whitespace_indicators(&mut self) -> AnyhowResult<()> {
        let __buffer_id = self.active_buffer();
        // Resolve the buffer's configured visibility up front: turning
        // the master toggle back on shows every space indicator plus
        // the tab and line-ending indicators the user actually
        // configured, rather than the hard-coded default. Fixes #2579.
        let configured = self.configured_whitespace_visibility(__buffer_id);
        if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&__buffer_id)
        {
            state.buffer_settings.whitespace.toggle_all(configured);
            let visible = state.buffer_settings.whitespace.any_visible();
            state.buffer_settings.whitespace_override = Some(visible);
            // The master toggle answers "any indicators at all?", so
            // it subsumes a finer tab pin: "hide whitespace
            // indicators" hiding everything *except* pinned arrows
            // would make the master appear broken.
            state.buffer_settings.tab_indicators_override = None;
            let status = if visible {
                t!("toggle.whitespace_indicators_shown")
            } else {
                t!("toggle.whitespace_indicators_hidden")
            };
            self.set_status_message(status.to_string());
        }
        Ok(())
    }

    /// Toggle tab indicators using the configured tab markers.
    fn handle_dispatch_toggle_tab_indicators(&mut self) {
        let __buffer_id = self.active_buffer();
        if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&__buffer_id)
        {
            // Only the tab-arrow trio: the command's description has
            // always promised "tab arrow indicators (→)", but it used
            // to share the master toggle-all with Whitespace
            // Indicators and flipped the space dots too.
            let ws = &mut state.buffer_settings.whitespace;
            let new_value = !(ws.tabs_leading || ws.tabs_inner || ws.tabs_trailing);
            ws.tabs_leading = new_value;
            ws.tabs_inner = new_value;
            ws.tabs_trailing = new_value;
            state.buffer_settings.tab_indicators_override = Some(new_value);
            let status = if new_value {
                t!("toggle.tab_indicators_shown")
            } else {
                t!("toggle.tab_indicators_hidden")
            };
            self.set_status_message(status.to_string());
        }
    }

    /// Toggle focus on the floating dock, opening it through the plugin if needed.
    fn handle_dispatch_toggle_dock_focus(&mut self) -> AnyhowResult<()> {
        // Bounce keyboard focus between the editor/explorer area and
        // the orchestrator dock. `dock` is `Some` whenever the dock is
        // mounted (focused or merely visible-but-blurred); the helpers
        // flip `focused` and fire the matching `focus`/`blur`
        // widget_event so the plugin's mirror stays in sync.
        match self.dock.as_ref().map(|d| d.focused) {
            Some(true) => self.blur_floating_panel(super::PanelSlot::Dock),
            Some(false) => self.refocus_floating_panel(super::PanelSlot::Dock),
            // Dock hidden: hand off to the orchestrator plugin's
            // show-dock command so one key both opens and focuses it.
            None => {
                return self
                    .handle_action(Action::PluginAction("orchestrator_dock_toggle".to_string()));
            }
        }
        Ok(())
    }

    /// Clear secondary cursors and selection state in the active view.
    fn handle_dispatch_remove_secondary_cursors(&mut self) {
        // Convert action to events and apply them
        if let Some(events) = self
            .active_window_mut()
            .action_to_events(Action::RemoveSecondaryCursors)
        {
            // Wrap in batch for atomic undo
            let batch = Event::Batch {
                events: events.clone(),
                description: "Remove secondary cursors".to_string(),
            };
            self.active_event_log_mut().append(batch.clone());
            self.apply_event_to_active_buffer(&batch);

            // Ensure the primary cursor is visible after removing secondary cursors
            let active_split = self
                .windows
                .get(&self.active_window)
                .and_then(|w| w.buffers.splits())
                .map(|(mgr, _)| mgr)
                .expect("active window must have a populated split layout")
                .active_split();
            let active_buffer = self.active_buffer();
            self.active_window_mut()
                .ensure_cursor_visible_for_split(active_buffer, active_split);
        }
    }

    /// Move to the logical line start in composite views, or apply Smart Home.
    fn handle_dispatch_smart_home(&mut self) {
        // In composite (diff) views, use LineStart movement
        let buffer_id = self.active_buffer();
        if self.active_window().is_composite_buffer(buffer_id) {
            if let Some(_handled) = self.handle_composite_action(buffer_id, &Action::SmartHome) {
                return;
            }
        }
        self.smart_home();
    }

    /// Toggle case-sensitive search and refresh the current matches.
    fn handle_dispatch_toggle_search_case_sensitive(&mut self) {
        self.active_window_mut().search_case_sensitive =
            !self.active_window().search_case_sensitive;
        let state = if self.active_window().search_case_sensitive {
            "enabled"
        } else {
            "disabled"
        };
        self.set_status_message(t!("search.case_sensitive_state", state = state).to_string());
        self.refresh_active_search();
    }

    /// Toggle whole-word search and refresh the current matches.
    fn handle_dispatch_toggle_search_whole_word(&mut self) {
        self.active_window_mut().search_whole_word = !self.active_window().search_whole_word;
        let state = if self.active_window().search_whole_word {
            "enabled"
        } else {
            "disabled"
        };
        self.set_status_message(t!("search.whole_word_state", state = state).to_string());
        self.refresh_active_search();
    }

    /// Toggle regular-expression search and refresh the current matches.
    fn handle_dispatch_toggle_search_regex(&mut self) {
        self.active_window_mut().search_use_regex = !self.active_window().search_use_regex;
        let state = if self.active_window().search_use_regex {
            "enabled"
        } else {
            "disabled"
        };
        self.set_status_message(t!("search.regex_state", state = state).to_string());
        self.refresh_active_search();
    }

    /// Toggle confirmation for each replacement in the active prompt.
    fn handle_dispatch_toggle_search_confirm_each(&mut self) {
        self.active_window_mut().search_confirm_each = !self.active_window().search_confirm_each;
        let state = if self.active_window().search_confirm_each {
            "enabled"
        } else {
            "disabled"
        };
        self.set_status_message(t!("search.confirm_each_state", state = state).to_string());
    }

    /// Delete backward, using bulk edits for multiple cursors.
    fn handle_dispatch_delete_backward(&mut self) {
        if self.active_window().is_editing_disabled() {
            self.set_status_message(t!("buffer.editing_disabled").to_string());
            return;
        }
        // Normal backspace handling
        if let Some(events) = self
            .active_window_mut()
            .action_to_events(Action::DeleteBackward)
        {
            if events.len() > 1 {
                // Multi-cursor: use optimized bulk edit (O(n) instead of O(n²))
                let description = "Delete backward".to_string();
                if let Some(applied) = self.apply_events_as_bulk_edit(events, description) {
                    self.active_event_log_mut().append(applied);
                }
            } else {
                for event in events {
                    self.active_event_log_mut().append(event.clone());
                    self.apply_event_to_active_buffer(&event);
                }
            }
        }
    }

    /// Load the active buffer as a plugin and configure its development workspace.
    fn handle_dispatch_load_plugin_from_buffer(&mut self) {
        #[cfg(feature = "plugins")]
        {
            let buffer_id = self.active_buffer();
            let state = self.active_state();
            let buffer = &state.buffer;
            let total = buffer.total_bytes();
            let content = String::from_utf8_lossy(&buffer.slice_bytes(0..total)).to_string();

            // Determine if TypeScript from file extension, default to TS
            let is_ts = buffer
                .file_path()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .map(|e| e == "ts" || e == "tsx")
                .unwrap_or(true);

            // Derive plugin name from buffer filename
            let name = buffer
                .file_path()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "buffer-plugin".to_string());

            let load_result = self
                .plugin_manager
                .read()
                .unwrap()
                .load_plugin_from_source(&content, &name, is_ts);
            match load_result {
                Ok(()) => {
                    self.set_status_message(format!("Plugin '{}' loaded from buffer", name));
                }
                Err(e) => {
                    self.set_status_message(format!("Failed to load plugin: {}", e));
                    tracing::error!("LoadPluginFromBuffer error: {}", e);
                }
            }

            // Set up plugin dev workspace for LSP support
            self.setup_plugin_dev_lsp(buffer_id, &content);
        }
        #[cfg(not(feature = "plugins"))]
        {
            self.set_status_message(
                "Plugins not available (compiled without plugin support)".to_string(),
            );
        }
    }

    /// Reload init.ts and notify plugins that initialization has completed.
    fn handle_dispatch_init_reload(&mut self) {
        // Same code path as auto-load: read init.ts and push it
        // through the existing plugin pipeline. The runtime's
        // hot-reload semantics drop prior commands / handlers /
        // event subs / settings before the new source runs.
        self.load_init_script(true);
        // Re-fire plugins_loaded so handlers expecting a "fresh"
        // post-load environment (M2) see it.
        self.fire_plugins_loaded_hook();
    }

    /// Create init.ts if needed, refresh plugin declarations, and open it for editing.
    fn handle_dispatch_init_edit(&mut self) {
        // Ensure the file exists (create from template if absent),
        // then open it in the editor so users can edit + reload.
        let config_dir = self.dir_context.config_dir.clone();
        match crate::init_script::ensure_starter(&config_dir) {
            Ok(path) => {
                // Regenerate `types/plugins.d.ts` from the live plugin
                // set. It's written once at editor startup, but any
                // plugin loaded/reloaded/unloaded since then would
                // leave the aggregate stale (or missing, in builds
                // where the plugins feature was off at boot but the
                // user has since enabled a plugin). The user's
                // tsconfig.json lists this file in `files`, so a
                // stale copy is exactly when `getPluginApi("foo")`
                // loses its typed overload.
                let declarations = self.plugin_manager.read().unwrap().plugin_declarations();
                crate::init_script::write_plugin_declarations(&config_dir, &declarations);
                match self.open_file(&path) {
                    Ok(_) => {
                        self.set_status_message(format!("init.ts: {}", path.display()));
                    }
                    Err(e) => {
                        self.set_status_message(format!("init.ts: open failed: {e}"));
                    }
                }
            }
            Err(e) => {
                self.set_status_message(format!("init.ts: create failed: {e}"));
            }
        }
    }

    /// Check init.ts and report the first error or the number of warnings.
    fn handle_dispatch_init_check(&mut self) {
        // Run the same parse check as `fresh --cmd init check` but
        // surface results in the status bar.
        let report = crate::init_script::check(&self.dir_context.config_dir);
        if report.ok && report.diagnostics.is_empty() {
            self.set_status_message("init.ts: ok".into());
        } else if !report.ok {
            let first = report
                .diagnostics
                .first()
                .map(|d| format!("{}:{}: {}", d.line, d.column, d.message))
                .unwrap_or_else(|| "unknown error".into());
            self.set_status_message(format!(
                "init.ts: {} error(s) — first: {first}",
                report.diagnostics.len()
            ));
        } else {
            self.set_status_message(format!("init.ts: {} warning(s)", report.diagnostics.len()));
        }
    }

    /// Focus the active terminal session and enter terminal mode.
    fn handle_dispatch_focus_terminal(&mut self) {
        // If viewing a terminal buffer, drop the focused split into live
        // mode (clears its scrollback edge, re-enables PTY input,
        // truncates the stale screen tail, resizes).
        if self
            .active_window()
            .is_terminal_buffer(self.active_buffer())
        {
            self.enter_terminal_mode();
            self.set_status_message(t!("status.terminal_mode_enabled").to_string());
        }
    }

    /// Toggle keyboard capture for the focused live terminal.
    fn handle_dispatch_toggle_keyboard_capture(&mut self) {
        // Toggle keyboard capture mode in terminal
        if self.active_window().focused_terminal_live() {
            self.active_window_mut().keyboard_capture = !self.active_window_mut().keyboard_capture;
            if self.active_window_mut().keyboard_capture {
                self.set_status_message(
                    "Keyboard capture ON - all keys go to terminal (F9 to toggle)".to_string(),
                );
            } else {
                self.set_status_message(
                    "Keyboard capture OFF - UI bindings active (F9 to toggle)".to_string(),
                );
            }
        }
    }

    /// Paste clipboard text into the terminal as one normalized batch.
    fn handle_dispatch_terminal_paste(&mut self) {
        // Paste clipboard contents into terminal as a single batch.
        // Same normalization and bracketing as the terminal route in
        // `paste_text`: CRLF/CR → LF, then wrapped in the paste
        // markers when the child asked for DECSET 2004.
        if self.active_window().focused_terminal_live() {
            if let Some(text) = self.clipboard.paste() {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                self.active_window_mut().send_terminal_paste(&normalized);
            }
        }
    }

    /// Close the settings editor, confirming when changes are unsaved.
    fn handle_dispatch_close_settings(&mut self) {
        // Check if there are unsaved changes
        let has_changes = self
            .settings_state
            .as_ref()
            .is_some_and(|s| s.has_changes());
        if has_changes {
            // Show confirmation dialog
            if let Some(ref mut state) = self.settings_state {
                state.show_confirm_dialog();
            }
        } else {
            self.close_settings(false);
        }
    }

    /// Cycle settings focus through categories, settings, and the footer.
    fn handle_dispatch_settings_toggle_focus(&mut self) {
        use crate::view::settings::state::{FocusPanel, FocusTarget};
        if let Some(ref mut state) = self.settings_state {
            let next = match state.focus_panel() {
                FocusPanel::Categories => FocusTarget::Card(state.selected_item),
                FocusPanel::Settings => FocusTarget::Footer(0),
                FocusPanel::Footer => FocusTarget::Categories,
            };
            state.focus_on(next);
        }
    }

    /// Confirm the active prompt and execute any resulting action.
    fn handle_dispatch_prompt_confirm(&mut self) -> AnyhowResult<()> {
        if let Some((input, prompt_type, selected_index)) = self.confirm_prompt() {
            use super::prompt_actions::PromptResult;
            match self.handle_prompt_confirm_input(input, prompt_type, selected_index) {
                PromptResult::ExecuteAction(action) => {
                    return self.handle_action(action);
                }
                PromptResult::EarlyReturn => {
                    return Ok(());
                }
                PromptResult::Done => {}
            }
        }
        Ok(())
    }

    /// Copy the prompt selection, or all prompt text when nothing is selected.
    fn handle_dispatch_prompt_copy(&mut self) {
        if let Some(prompt) = &self.active_window_mut().prompt {
            let text = prompt.selected_text().unwrap_or_else(|| prompt.get_text());
            if !text.is_empty() {
                self.clipboard.copy(text);
                self.set_status_message(t!("clipboard.copied").to_string());
            }
        }
    }

    /// Cut the prompt selection or all prompt text, then refresh suggestions.
    fn handle_dispatch_prompt_cut(&mut self) {
        if let Some(prompt) = &self.active_window_mut().prompt {
            let text = prompt.selected_text().unwrap_or_else(|| prompt.get_text());
            if !text.is_empty() {
                self.clipboard.copy(text);
            }
        }
        if let Some(prompt) = self.active_window_mut().prompt.as_mut() {
            if prompt.has_selection() {
                prompt.delete_selection();
            } else {
                prompt.clear();
            }
        }
        self.set_status_message(t!("clipboard.cut").to_string());
        self.update_prompt_suggestions();
    }

    /// Paste clipboard text into the active prompt and synchronize its input state.
    fn handle_dispatch_prompt_paste(&mut self) {
        if let Some(text) = self.clipboard.paste() {
            if let Some(prompt) = self.active_window_mut().prompt.as_mut() {
                prompt.insert_str(&text);
            }
            self.update_prompt_suggestions();
        }
    }

    /// Handle an action (for normal mode and command execution).
    /// Used by the app module internally and by the GUI module for native menu dispatch.
    pub(crate) fn handle_action(&mut self, action: Action) -> AnyhowResult<()> {
        use crate::input::keybindings::Action;

        // Actions are the funnel for command-driven UI mutation (palette,

        // **An action is a write the description reads.** Most reach here
        // through a key or a press, which mark the tree stale themselves
        // when they finish; the ones that do not — a plugin command, a test
        // driving the editor by actions, an async completion — change what
        // the tree says (a prompt opens, a popup closes, focus moves) with
        // no frame in between. The next input, and the next `get_key_context`,
        // lay the tree out from the facts first (`shell_description_stale`).
        self.shell_description_stale = true;

        // Record action to macro if recording
        self.record_macro_action(&action);

        // Reset dabbrev cycling session on any non-dabbrev action.
        if !matches!(action, Action::DabbrevExpand) {
            self.reset_dabbrev_state();
        }

        // Enter on a line that points somewhere (`editor.setLineTargets`)
        // follows it, the same as clicking it. Intercepted here rather than
        // inside the newline handler so the behaviour is identical whether
        // the buffer is editable or not — an index built by a script is
        // usually a plain file, and typing a newline into it is never what
        // pressing Enter on an entry meant.
        #[cfg(feature = "plugins")]
        if matches!(action, Action::InsertNewline) {
            let buffer_id = self.active_buffer();
            if let Some(line) = self.cursor_line_in_active_buffer() {
                if let Some(target) = self.line_target_at(buffer_id, line) {
                    let source = self.active_split_id();
                    self.follow_line_target(target, source);
                    return Ok(());
                }
            }
        }

        // **A page's window is not a buffer's viewport.** Scrolling a pane
        // that shows a described page has to move the tree's own window;
        // `Event::Scroll` would move the mirror's, which nothing draws. Every
        // other action goes on to mean what it always meant — this is the one
        // place a page reinterprets the editor's vocabulary, and it
        // reinterprets two entries of it.
        if self.page_takes_action(&action) {
            return Ok(());
        }

        match action {
            Action::Quit => {
                self.quit();
            }
            Action::ForceQuit => {
                self.should_quit = true;
            }
            Action::CopyToNewTab => {
                self.handle_copy()?;
                if let Some(text) = self.clipboard.paste_internal() {
                    self.new_buffer();
                    self.paste_text(text);
                }
            }
            Action::Detach => {
                self.should_detach = true;
            }
            Action::WorkspaceTrustTrust => {
                self.set_workspace_trust_level(
                    crate::services::workspace_trust::TrustLevel::Trusted,
                );
            }
            Action::WorkspaceTrustRestrict => {
                self.set_workspace_trust_level(
                    crate::services::workspace_trust::TrustLevel::Restricted,
                );
            }
            Action::WorkspaceTrustBlock => {
                self.set_workspace_trust_level(
                    crate::services::workspace_trust::TrustLevel::Blocked,
                );
            }
            Action::WorkspaceTrustPrompt => {
                // Voluntarily-opened: cancellable (Esc / Cancel just closes).
                self.show_workspace_trust_popup(true);
            }
            Action::Save => {
                self.handle_dispatch_save();
            }
            Action::SaveAs => {
                self.handle_dispatch_save_as();
            }
            Action::SaveAll => {
                self.handle_dispatch_save_all();
            }
            Action::Open => {
                self.start_prompt(t!("file.open_prompt").to_string(), PromptType::OpenFile);
                self.prefill_open_file_prompt();
                self.init_file_open_state();
            }
            Action::SwitchProject => {
                self.start_prompt(
                    t!("file.switch_project_prompt").to_string(),
                    PromptType::SwitchProject,
                );
                self.init_folder_open_state();
            }
            Action::GotoLine => {
                self.handle_dispatch_goto_line();
            }
            Action::ScanLineIndex => {
                self.start_incremental_line_scan(false);
            }
            Action::New => {
                self.new_buffer();
            }
            Action::Close | Action::CloseTab => {
                // Both Close and CloseTab use close_tab() which handles:
                // - Closing the split if this is the last buffer and there are other splits
                // - Prompting for unsaved changes
                // - Properly closing the buffer
                self.close_tab();
            }
            Action::Revert => {
                self.handle_dispatch_revert();
            }
            Action::ToggleAutoRevert => {
                self.toggle_auto_revert();
            }
            Action::OpenUpdateLog => {
                self.show_self_update_output();
            }
            Action::UpdateFresh => {
                self.handle_dispatch_update_fresh();
            }
            Action::FormatBuffer => {
                self.handle_dispatch_format_buffer();
            }
            Action::TrimTrailingWhitespace => {
                self.handle_dispatch_trim_trailing_whitespace();
            }
            Action::EnsureFinalNewline => {
                self.handle_dispatch_ensure_final_newline();
            }
            Action::Copy => {
                self.handle_copy()?;
            }
            Action::CopyWithTheme(theme) => {
                self.copy_selection_with_theme(&theme);
            }
            Action::CopyFilePath => {
                self.copy_active_buffer_path(false);
            }
            Action::CopyRelativeFilePath => {
                self.copy_active_buffer_path(true);
            }
            Action::Cut => {
                self.handle_dispatch_cut();
            }
            Action::Paste => {
                self.handle_paste()?;
            }
            Action::SelectAll => {
                self.handle_dispatch_select_all()?;
            }
            Action::YankWordForward => {
                self.yank_word_forward();
            }
            Action::YankWordBackward => {
                self.yank_word_backward();
            }
            Action::YankToLineEnd => {
                self.yank_to_line_end();
            }
            Action::YankToLineStart => {
                self.yank_to_line_start();
            }
            Action::YankViWordEnd => {
                self.yank_vi_word_end();
            }
            Action::Undo => {
                self.handle_undo();
            }
            Action::Redo => {
                self.handle_redo();
            }
            Action::ShowHelp => {
                self.ensure_help_panel_mode_registered();
                self.active_window_mut().open_help_manual();
            }
            Action::ShowKeyboardShortcuts => {
                self.ensure_help_panel_mode_registered();
                self.active_window_mut().open_keyboard_shortcuts();
            }
            Action::ShowWarnings => {
                self.show_warnings_popup();
            }
            Action::ShowStatusLog => {
                self.open_status_log();
            }
            Action::ShowLspStatus => {
                self.show_lsp_status_popup();
            }
            Action::ShowRemoteIndicatorMenu => {
                self.show_remote_indicator_popup();
            }
            Action::ShowReadOnlyMenu => {
                self.show_read_only_popup();
            }
            Action::ClearWarnings => {
                self.active_window_mut().clear_warnings();
            }
            Action::CommandPalette => {
                self.handle_dispatch_command_palette();
            }
            Action::QuickOpen => {
                if self.close_quick_open_if_open() {
                    return Ok(());
                }
                self.start_quick_open();
            }
            Action::QuickOpenBuffers => {
                if self.close_quick_open_if_open() {
                    return Ok(());
                }
                self.start_quick_open_with_prefix("#");
            }
            Action::QuickOpenFiles => {
                if self.close_quick_open_if_open() {
                    return Ok(());
                }
                self.start_quick_open_with_prefix("");
            }
            Action::OpenLiveGrep => {
                self.handle_action(Action::PluginAction("start_live_grep".to_string()))?;
            }
            Action::ResumeLiveGrep => {
                self.handle_action(Action::PluginAction("resume_live_grep".to_string()))?;
            }
            Action::ToggleUtilityDock => {
                self.handle_dispatch_toggle_utility_dock();
            }
            Action::CycleLiveGrepProvider => {
                self.handle_dispatch_cycle_live_grep_provider()?;
            }
            Action::OpenTerminalInDock => {
                self.handle_open_terminal_in_dock()?;
            }
            Action::ToggleLineWrap => {
                self.handle_dispatch_toggle_line_wrap();
            }
            Action::ToggleCurrentLineHighlight => {
                self.handle_dispatch_toggle_current_line_highlight();
            }
            Action::ToggleOccurrenceHighlight => {
                self.handle_dispatch_toggle_occurrence_highlight();
            }
            Action::ToggleReadOnly => {
                self.handle_dispatch_toggle_read_only();
            }
            Action::TogglePageView => {
                self.active_window_mut().handle_toggle_page_view();
            }
            Action::SetPageWidth => {
                self.handle_dispatch_set_page_width();
            }
            Action::SetBackground => {
                self.handle_dispatch_set_background();
            }
            Action::SetBackgroundBlend => {
                self.handle_dispatch_set_background_blend();
            }
            Action::LspCompletion => {
                self.request_completion();
            }
            Action::DabbrevExpand => {
                if self.refuse_if_editing_disabled() {
                    return Ok(());
                }
                self.dabbrev_expand();
            }
            Action::LspGotoDefinition => {
                self.request_goto_definition()?;
            }
            Action::LspRename => {
                self.start_rename()?;
            }
            Action::LspHover => {
                self.request_hover()?;
            }
            Action::LspReferences => {
                self.request_references()?;
            }
            Action::LspImplementation => {
                self.request_implementation()?;
            }
            Action::LspSignatureHelp => {
                self.request_signature_help();
            }
            Action::LspCodeActions => {
                self.request_code_actions()?;
            }
            Action::LspRestart => {
                self.handle_lsp_restart();
            }
            Action::LspStop => {
                self.handle_lsp_stop();
            }
            Action::LspToggleForBuffer => {
                self.handle_lsp_toggle_for_buffer();
            }
            Action::ToggleInlayHints => {
                self.toggle_inlay_hints();
            }
            Action::DumpConfig => {
                self.dump_config();
            }
            Action::DumpUiTree => {
                self.dump_ui_tree();
            }
            Action::RedrawScreen => {
                self.request_full_redraw();
            }
            Action::SelectTheme => {
                self.start_select_theme_prompt();
            }
            Action::InspectThemeAtCursor => {
                self.inspect_theme_at_cursor();
            }
            Action::SelectKeybindingMap => {
                self.start_select_keybinding_map_prompt();
            }
            Action::SelectCursorStyle => {
                self.start_select_cursor_style_prompt();
            }
            Action::SelectLocale => {
                self.start_select_locale_prompt();
            }
            Action::Search => {
                self.handle_dispatch_search();
            }
            Action::Replace => {
                self.handle_dispatch_replace();
            }
            Action::QueryReplace => {
                self.handle_dispatch_query_replace();
            }
            Action::FindInSelection => {
                self.start_search_prompt(
                    t!("file.search_prompt").to_string(),
                    PromptType::Search,
                    true,
                );
            }
            Action::FindNext => {
                self.find_next();
            }
            Action::FindPrevious => {
                self.find_previous();
            }
            Action::FindSelectionNext => {
                self.find_selection_next();
            }
            Action::FindSelectionPrevious => {
                self.find_selection_previous();
            }
            Action::ClearSearch => {
                self.active_window_mut().clear_search_highlights();
            }
            Action::AddCursorNextMatch => {
                self.add_cursor_at_next_match();
            }
            Action::AddCursorAbove => {
                self.add_cursor_above();
            }
            Action::AddCursorBelow => {
                self.add_cursor_below();
            }
            Action::AddCursorsToLineEnds => {
                self.add_cursors_to_line_ends();
            }
            Action::NextBuffer => {
                self.next_buffer();
            }
            Action::PrevBuffer => {
                self.prev_buffer();
            }
            Action::SwitchToPreviousTab => {
                self.switch_to_previous_tab();
            }
            Action::SwitchToTabByName => {
                self.start_switch_to_tab_prompt();
            }

            // Tab scrolling (manual scroll - don't auto-adjust)
            Action::ScrollTabsLeft => {
                self.handle_dispatch_scroll_tabs_left();
            }
            Action::ScrollTabsRight => {
                self.handle_dispatch_scroll_tabs_right();
            }
            Action::NavigateBack => {
                self.navigate_back();
            }
            Action::NavigateForward => {
                self.navigate_forward();
            }
            Action::SplitHorizontal => {
                self.split_pane_horizontal();
            }
            Action::SplitVertical => {
                self.split_pane_vertical();
            }
            Action::CloseSplit => {
                self.close_active_split();
            }
            Action::NextSplit => {
                self.next_split();
            }
            Action::PrevSplit => {
                self.prev_split();
            }
            Action::NextPane => {
                self.next_pane();
            }
            Action::PrevPane => {
                self.prev_pane();
            }
            Action::NextWindow => {
                self.next_window();
            }
            Action::PrevWindow => {
                self.prev_window();
            }
            Action::ExtractTabToNewWorkspace => {
                let buffer_id = self.active_buffer();
                self.extract_tab_to_new_workspace(buffer_id);
            }
            Action::IncreaseSplitSize => {
                self.adjust_split_size(0.05);
            }
            Action::DecreaseSplitSize => {
                self.adjust_split_size(-0.05);
            }
            Action::ToggleMaximizeSplit => {
                self.toggle_maximize_split();
            }
            Action::ToggleFileExplorer => {
                self.toggle_file_explorer();
            }
            Action::ToggleFileExplorerSide => {
                self.toggle_file_explorer_side();
            }
            Action::ToggleMenuBar => {
                self.toggle_menu_bar();
            }
            Action::ToggleTabBar => {
                self.toggle_tab_bar();
            }
            Action::ToggleStatusBar => {
                self.toggle_status_bar();
            }
            Action::TogglePromptLine => {
                self.toggle_prompt_line();
            }
            Action::ToggleVerticalScrollbar => {
                self.toggle_vertical_scrollbar();
            }
            Action::ToggleHorizontalScrollbar => {
                self.toggle_horizontal_scrollbar();
            }
            Action::ToggleLineNumbers => {
                self.toggle_line_numbers();
            }
            Action::ToggleLineNumbersCurrentBuffer => {
                self.toggle_line_numbers_current_buffer();
            }
            Action::ToggleLineWrapCurrentBuffer => {
                self.toggle_line_wrap_current_buffer();
            }
            Action::ToggleVirtualSpaceCurrentBuffer => {
                self.toggle_virtual_space_current_buffer();
            }
            Action::ToggleIndentationGuideCurrentBuffer => {
                self.toggle_indentation_guide_current_buffer();
            }
            Action::ToggleFoldIndicatorsCurrentBuffer => {
                self.toggle_fold_indicators_current_buffer();
            }
            Action::ToggleCurrentLineHighlightCurrentBuffer => {
                self.toggle_current_line_highlight_current_buffer();
            }
            Action::ToggleOccurrenceHighlightCurrentBuffer => {
                self.toggle_occurrence_highlight_current_buffer();
            }
            Action::TriggerWaveAnimation => {
                self.trigger_wave_animation();
            }
            Action::ToggleScrollSync => {
                self.active_window_mut().toggle_scroll_sync();
            }
            Action::ToggleMouseCapture => {
                self.toggle_mouse_capture();
            }
            Action::ToggleMouseHover => {
                self.toggle_mouse_hover();
            }
            Action::ToggleDebugHighlights => {
                self.active_window_mut().toggle_debug_highlights();
            }
            // Rulers
            Action::AddRuler => {
                self.start_prompt(t!("rulers.add_prompt").to_string(), PromptType::AddRuler);
            }
            Action::RemoveRuler => {
                self.start_remove_ruler_prompt();
            }
            // Buffer settings
            Action::SetTabSize => {
                self.handle_dispatch_set_tab_size();
            }
            Action::SetLineEnding => {
                self.start_set_line_ending_prompt();
            }
            Action::SetEncoding => {
                self.start_set_encoding_prompt();
            }
            Action::ReloadWithEncoding => {
                self.start_reload_with_encoding_prompt();
            }
            Action::SetLanguage => {
                self.start_set_language_prompt();
            }
            Action::ToggleIndentationStyle => {
                self.handle_dispatch_toggle_indentation_style();
            }
            Action::ToggleWhitespaceIndicators => {
                self.handle_dispatch_toggle_whitespace_indicators()?;
            }
            Action::ToggleTabIndicators => {
                self.handle_dispatch_toggle_tab_indicators();
            }
            Action::ResetBufferSettings => {
                self.reset_buffer_settings();
            }
            Action::FocusFileExplorer => {
                self.focus_file_explorer();
            }
            Action::FocusEditor => {
                self.active_window_mut().focus_editor();
            }
            Action::FocusNextSidebarSection => {
                self.focus_next_sidebar_section();
            }
            Action::ToggleDockFocus => {
                self.handle_dispatch_toggle_dock_focus()?;
            }
            Action::FileExplorerUp => {
                self.file_explorer_navigate_up();
            }
            Action::FileExplorerDown => {
                self.file_explorer_navigate_down();
            }
            Action::FileExplorerPageUp => {
                self.file_explorer_page_up();
            }
            Action::FileExplorerPageDown => {
                self.file_explorer_page_down();
            }
            Action::FileExplorerExpand => {
                self.file_explorer_toggle_expand();
            }
            Action::FileExplorerCollapse => {
                self.file_explorer_collapse();
            }
            Action::FileExplorerOpen => {
                self.file_explorer_open_file()?;
            }
            Action::FileExplorerRefresh => {
                self.file_explorer_refresh();
            }
            Action::FileExplorerNewFile => {
                self.file_explorer_new_file();
            }
            Action::FileExplorerNewDirectory => {
                self.file_explorer_new_directory();
            }
            Action::FileExplorerDelete => {
                self.file_explorer_delete();
            }
            Action::FileExplorerRename => {
                self.file_explorer_rename();
            }
            Action::FileExplorerToggleHidden => {
                self.file_explorer_toggle_hidden();
            }
            Action::FileExplorerToggleGitignored => {
                self.file_explorer_toggle_gitignored();
            }
            Action::FileExplorerSearchClear => {
                self.active_window_mut().file_explorer_search_clear();
            }
            Action::FileExplorerSearchBackspace => {
                self.active_window_mut().file_explorer_search_pop_char();
            }
            Action::FileExplorerCopy => {
                self.active_window_mut().file_explorer_copy();
            }
            Action::FileExplorerCut => {
                self.active_window_mut().file_explorer_cut();
            }
            Action::FileExplorerPaste => {
                self.file_explorer_paste();
            }
            Action::FileExplorerDuplicate => {
                self.file_explorer_duplicate();
            }
            Action::FileExplorerCopyFullPath => {
                self.file_explorer_copy_path(false);
            }
            Action::FileExplorerCopyRelativePath => {
                self.file_explorer_copy_path(true);
            }
            Action::FileExplorerExtendSelectionUp => {
                self.active_window_mut().file_explorer_extend_selection_up();
            }
            Action::FileExplorerExtendSelectionDown => {
                self.active_window_mut()
                    .file_explorer_extend_selection_down();
            }
            Action::FileExplorerToggleSelect => {
                self.active_window_mut().file_explorer_toggle_select();
            }
            Action::FileExplorerSelectAll => {
                self.active_window_mut().file_explorer_select_all();
            }
            Action::RemoveSecondaryCursors => {
                self.handle_dispatch_remove_secondary_cursors();
            }

            // Menu navigation actions
            Action::MenuActivate => {
                self.handle_menu_activate();
            }
            Action::MenuClose => {
                self.handle_menu_close();
            }
            Action::MenuLeft => {
                self.handle_menu_left();
            }
            Action::MenuRight => {
                self.handle_menu_right();
            }
            Action::MenuUp => {
                self.handle_menu_up();
            }
            Action::MenuDown => {
                self.handle_menu_down();
            }
            Action::MenuExecute => {
                if let Some(action) = self.handle_menu_execute() {
                    return self.handle_action(action);
                }
            }
            Action::MenuOpen(menu_name) => {
                // No mnemonics check here: with `menu_bar_mnemonics` off the
                // resolver suppresses the Alt+letter mnemonic bindings, so a
                // MenuOpen that reaches dispatch came from a live binding
                // (or a menu-bar click) and must open the menu. Gating here
                // instead used to turn Alt+F into a dead key: the mnemonic
                // still won resolution, then dispatch dropped it (#2941).
                self.handle_menu_open(&menu_name);
            }

            Action::SwitchKeybindingMap(map_name) => {
                // Delegate to the shared helper so the menu path persists the
                // choice to the user config (issue #474), matching the
                // command-palette path. This handler previously duplicated the
                // switch logic but skipped persistence, so the keybinding style
                // reset to the default on the next launch.
                self.apply_keybinding_map(&map_name);
            }

            Action::SmartHome => {
                self.handle_dispatch_smart_home();
            }
            Action::ToggleComment => {
                self.toggle_comment();
            }
            Action::ToggleFold => {
                self.active_window_mut().toggle_fold_at_cursor();
            }
            Action::GoToMatchingBracket => {
                self.goto_matching_bracket();
            }
            Action::JumpToNextError => {
                self.jump_to_next_error();
            }
            Action::JumpToPreviousError => {
                self.jump_to_previous_error();
            }
            Action::SetBookmark(key) => {
                self.active_window_mut().set_bookmark(key);
            }
            Action::JumpToBookmark(key) => {
                self.jump_to_bookmark(key);
            }
            Action::ClearBookmark(key) => {
                self.active_window_mut().clear_bookmark(key);
            }
            Action::ListBookmarks => {
                self.active_window_mut().list_bookmarks();
            }
            Action::ToggleSearchCaseSensitive if !self.active_prompt_has_search_options() => {}
            Action::ToggleSearchWholeWord if !self.active_prompt_has_search_options() => {}
            Action::ToggleSearchRegex if !self.active_prompt_has_search_options() => {}
            Action::ToggleSearchCaseSensitive => {
                self.handle_dispatch_toggle_search_case_sensitive();
            }
            Action::ToggleSearchWholeWord => {
                self.handle_dispatch_toggle_search_whole_word();
            }
            Action::ToggleSearchRegex => {
                self.handle_dispatch_toggle_search_regex();
            }
            Action::ToggleSearchConfirmEach => {
                self.handle_dispatch_toggle_search_confirm_each();
            }
            Action::FileBrowserToggleHidden => {
                // Toggle hidden files in file browser (handled via file_open_toggle_hidden)
                self.file_open_toggle_hidden();
            }
            Action::StartMacroRecording => {
                // This is a no-op; use ToggleMacroRecording instead
                self.set_status_message(
                    "Use Ctrl+Shift+R to start recording (will prompt for register)".to_string(),
                );
            }
            Action::StopMacroRecording => {
                self.stop_macro_recording();
            }
            Action::PlayMacro(key) => {
                self.play_macro(key);
            }
            Action::ToggleMacroRecording(key) => {
                self.toggle_macro_recording(key);
            }
            Action::ShowMacro(key) => {
                self.show_macro_in_buffer(key);
            }
            Action::ListMacros => {
                self.list_macros_in_buffer();
            }
            Action::PromptRecordMacro => {
                self.start_prompt("Record macro (0-9): ".to_string(), PromptType::RecordMacro);
            }
            Action::PromptPlayMacro => {
                self.start_prompt("Play macro (0-9): ".to_string(), PromptType::PlayMacro);
            }
            Action::PlayLastMacro => {
                if let Some(key) = self.active_window_mut().macros.last_register() {
                    self.play_macro(key);
                } else {
                    self.set_status_message(t!("status.no_macro_recorded").to_string());
                }
            }
            Action::PromptSaveMacroToInit => {
                self.start_prompt(
                    "Save macro to init.ts (0-9): ".to_string(),
                    PromptType::SaveMacroToInit,
                );
            }
            Action::PromptPromoteMacro => {
                self.start_prompt(
                    "Promote macro to command (0-9): ".to_string(),
                    PromptType::PromoteMacro,
                );
            }
            Action::PromptSetBookmark => {
                self.start_prompt("Set bookmark (0-9): ".to_string(), PromptType::SetBookmark);
            }
            Action::PromptJumpToBookmark => {
                self.start_prompt(
                    "Jump to bookmark (0-9): ".to_string(),
                    PromptType::JumpToBookmark,
                );
            }
            Action::CompositeNextHunk => {
                let buf = self.active_buffer();
                self.active_window_mut().composite_next_hunk_active(buf);
            }
            Action::CompositePrevHunk => {
                let buf = self.active_buffer();
                self.active_window_mut().composite_prev_hunk_active(buf);
            }
            Action::None => {}
            Action::DeleteBackward => {
                self.handle_dispatch_delete_backward();
            }
            Action::PluginAction(action_name) => {
                tracing::debug!("handle_action: PluginAction('{}')", action_name);
                // Execute the plugin callback via TypeScript plugin thread
                // Use non-blocking version to avoid deadlock with async plugin ops
                #[cfg(feature = "plugins")]
                {
                    let result = self.plugin_manager.read().unwrap().execute_action_async(
                        &action_name,
                        None,
                        None,
                    );
                    if let Some(result) = result {
                        match result {
                            Ok(receiver) => {
                                // Store pending action for processing in main loop
                                self.pending_plugin_actions
                                    .push((action_name.clone(), receiver));
                            }
                            Err(e) => {
                                self.set_status_message(
                                    t!("view.plugin_error", error = e.to_string()).to_string(),
                                );
                                tracing::error!("Plugin action error: {}", e);
                            }
                        }
                    } else {
                        self.set_status_message(
                            t!("status.plugin_manager_unavailable").to_string(),
                        );
                    }
                }
                #[cfg(not(feature = "plugins"))]
                {
                    let _ = action_name;
                    self.set_status_message(
                        "Plugins not available (compiled without plugin support)".to_string(),
                    );
                }
            }
            Action::LoadPluginFromBuffer => {
                self.handle_dispatch_load_plugin_from_buffer();
            }
            Action::InitReload => {
                self.handle_dispatch_init_reload();
            }
            Action::InitEdit => {
                self.handle_dispatch_init_edit();
            }
            Action::InitCheck => {
                self.handle_dispatch_init_check();
            }
            Action::OpenTerminal => {
                self.open_terminal();
            }
            Action::OpenTerminalRight => {
                self.open_terminal_split(crate::model::event::SplitDirection::Vertical);
            }
            Action::OpenTerminalBelow => {
                self.open_terminal_split(crate::model::event::SplitDirection::Horizontal);
            }
            Action::CloseTerminal => {
                self.close_terminal();
            }
            Action::RestartTerminal => {
                self.restart_terminal();
            }
            Action::FocusTerminal => {
                self.handle_dispatch_focus_terminal();
            }
            Action::TerminalEscape => {
                // Drop the focused live terminal split into read-only scrollback.
                if self.active_window().focused_terminal_live() {
                    self.enter_terminal_scrollback();
                    self.set_status_message(t!("status.terminal_mode_disabled").to_string());
                }
            }
            Action::ToggleKeyboardCapture => {
                self.handle_dispatch_toggle_keyboard_capture();
            }
            Action::TerminalPaste => {
                self.handle_dispatch_terminal_paste();
            }
            Action::SendSelectionToTerminal => {
                self.send_selection_to_terminal();
            }
            Action::ShellCommand => {
                // Run shell command on buffer/selection, output to new buffer
                self.start_shell_command_prompt(false);
            }
            Action::ShellCommandReplace => {
                if self.refuse_if_editing_disabled() {
                    return Ok(());
                }
                // Run shell command on buffer/selection, replace content
                self.start_shell_command_prompt(true);
            }
            Action::OpenSettings => {
                self.open_settings();
            }
            Action::CloseSettings => {
                self.handle_dispatch_close_settings();
            }
            Action::SettingsSave => {
                self.save_settings();
            }
            Action::SettingsReset => {
                if let Some(ref mut state) = self.settings_state {
                    state.reset_current_to_default();
                }
            }
            Action::SettingsInherit => {
                if let Some(ref mut state) = self.settings_state {
                    state.set_current_to_null();
                }
            }
            // The next *panel* — coarser than the ring's Tab, which steps
            // control to control; a bound key for jumping between the tree,
            // the body and the footer.
            Action::SettingsToggleFocus => {
                self.handle_dispatch_settings_toggle_focus();
            }
            Action::SettingsActivate => {
                self.settings_activate_current();
            }
            Action::SettingsSearch => {
                if let Some(ref mut state) = self.settings_state {
                    state.start_search();
                }
            }
            Action::SettingsHelp => {
                if let Some(ref mut state) = self.settings_state {
                    state.toggle_help();
                }
            }
            Action::SettingsIncrement => {
                self.settings_increment_current();
            }
            Action::SettingsDecrement => {
                self.settings_decrement_current();
            }
            Action::CalibrateInput => {
                self.open_calibration_wizard();
            }
            Action::EventDebug => {
                self.active_window_mut().open_event_debug();
            }
            Action::SuspendProcess => {
                self.request_suspend();
            }
            Action::OpenKeybindingEditor => {
                self.open_keybinding_editor();
            }
            Action::PromptConfirm => {
                self.handle_dispatch_prompt_confirm()?;
            }
            Action::PromptConfirmWithText(ref text) => {
                // For macro playback: set the prompt text before confirming
                if let Some(ref mut prompt) = self.active_window_mut().prompt {
                    prompt.set_input(text.clone());
                    self.update_prompt_suggestions();
                }
                if let Some((input, prompt_type, selected_index)) = self.confirm_prompt() {
                    use super::prompt_actions::PromptResult;
                    match self.handle_prompt_confirm_input(input, prompt_type, selected_index) {
                        PromptResult::ExecuteAction(action) => {
                            return self.handle_action(action);
                        }
                        PromptResult::EarlyReturn => {
                            return Ok(());
                        }
                        PromptResult::Done => {}
                    }
                }
            }
            Action::PopupConfirm => {
                use super::popup_actions::PopupConfirmResult;
                if let PopupConfirmResult::EarlyReturn = self.handle_popup_confirm() {
                    return Ok(());
                }
            }
            Action::PopupCancel => {
                self.handle_popup_cancel();
            }
            Action::PopupFocus => {
                self.handle_popup_focus();
            }
            Action::CompletionAccept => {
                use super::popup_actions::PopupConfirmResult;
                if let PopupConfirmResult::EarlyReturn = self.handle_popup_confirm() {
                    return Ok(());
                }
            }
            Action::CompletionDismiss => {
                self.handle_popup_cancel();
            }
            Action::InsertChar(c) => {
                if self.is_prompting() {
                    return self.handle_insert_char_prompt(c);
                } else if self.active_window_mut().key_context == KeyContext::FileExplorer {
                    self.active_window_mut().file_explorer_search_push_char(c);
                } else {
                    self.handle_insert_char_editor(c)?;
                }
            }
            // Prompt clipboard actions
            Action::PromptCopy => {
                self.handle_dispatch_prompt_copy();
            }
            Action::PromptCut => {
                self.handle_dispatch_prompt_cut();
            }
            Action::PromptPaste => {
                self.handle_dispatch_prompt_paste();
            }

            // Prompt navigation and editing.
            //
            // These are the actions the `prompt` section of every keymap binds.
            // The prompt owns its editing logic in `Prompt`'s `InputHandler`
            // (suggestion sync, theme preview, plugin selection hooks, …), and
            // the handler runs *before* keybinding resolution — so a key it
            // handles itself never reaches its binding, and until now a key it
            // ignored resolved to an action with no dispatch arm at all and did
            // nothing. That left every `prompt` binding on a non-default key
            // silently dead (Emacs `C-b`/`C-n`/`C-g`, say).
            //
            // Each action is defined as "the same thing this key does", and
            // re-enters the prompt's handler with that key, so there is one
            // implementation of each operation rather than a second copy here
            // that would drift.
            Action::PromptCancel => {
                self.dispatch_prompt_key_for_action(KeyCode::Esc, KM::NONE);
            }
            Action::PromptBackspace => {
                self.dispatch_prompt_key_for_action(KeyCode::Backspace, KM::NONE);
            }
            Action::PromptDelete => {
                self.dispatch_prompt_key_for_action(KeyCode::Delete, KM::NONE);
            }
            Action::PromptMoveLeft => {
                self.dispatch_prompt_key_for_action(KeyCode::Left, KM::NONE);
            }
            Action::PromptMoveRight => {
                self.dispatch_prompt_key_for_action(KeyCode::Right, KM::NONE);
            }
            Action::PromptMoveStart => {
                self.dispatch_prompt_key_for_action(KeyCode::Home, KM::NONE);
            }
            Action::PromptMoveEnd => {
                self.dispatch_prompt_key_for_action(KeyCode::End, KM::NONE);
            }
            Action::PromptSelectPrev => {
                self.dispatch_prompt_key_for_action(KeyCode::Up, KM::NONE);
            }
            Action::PromptSelectNext => {
                self.dispatch_prompt_key_for_action(KeyCode::Down, KM::NONE);
            }
            Action::PromptPageUp => {
                self.dispatch_prompt_key_for_action(KeyCode::PageUp, KM::NONE);
            }
            Action::PromptPageDown => {
                self.dispatch_prompt_key_for_action(KeyCode::PageDown, KM::NONE);
            }
            Action::PromptAcceptSuggestion => {
                self.dispatch_prompt_key_for_action(KeyCode::Tab, KM::NONE);
            }
            Action::PromptMoveWordLeft => {
                self.dispatch_prompt_key_for_action(KeyCode::Left, KM::CONTROL);
            }
            Action::PromptMoveWordRight => {
                self.dispatch_prompt_key_for_action(KeyCode::Right, KM::CONTROL);
            }
            Action::PromptDeleteWordBackward => {
                self.dispatch_prompt_key_for_action(KeyCode::Backspace, KM::CONTROL);
            }
            Action::PromptDeleteWordForward => {
                self.dispatch_prompt_key_for_action(KeyCode::Delete, KM::CONTROL);
            }
            Action::PromptDeleteToLineEnd => {
                self.dispatch_prompt_key_for_action(KeyCode::Char('k'), KM::CONTROL);
            }
            Action::PromptSelectAll => {
                self.dispatch_prompt_key_for_action(KeyCode::Char('a'), KM::CONTROL);
            }
            Action::PromptMoveLeftSelecting => {
                self.dispatch_prompt_key_for_action(KeyCode::Left, KM::SHIFT);
            }
            Action::PromptMoveRightSelecting => {
                self.dispatch_prompt_key_for_action(KeyCode::Right, KM::SHIFT);
            }
            Action::PromptMoveHomeSelecting => {
                self.dispatch_prompt_key_for_action(KeyCode::Home, KM::SHIFT);
            }
            Action::PromptMoveEndSelecting => {
                self.dispatch_prompt_key_for_action(KeyCode::End, KM::SHIFT);
            }
            Action::PromptSelectWordLeft => {
                self.dispatch_prompt_key_for_action(KeyCode::Left, KM::CONTROL | KM::SHIFT);
            }
            Action::PromptSelectWordRight => {
                self.dispatch_prompt_key_for_action(KeyCode::Right, KM::CONTROL | KM::SHIFT);
            }
            _ => {
                // TODO: Why do we have this catch-all? It seems like actions should either:
                // 1. Be handled explicitly above (like InsertChar, PopupConfirm, etc.)
                // 2. Or be converted to events consistently
                // This catch-all makes it unclear which actions go through event conversion
                // vs. direct handling. Consider making this explicit or removing the pattern.
                self.apply_action_as_events(action)?;
            }
        }

        Ok(())
    }

    /// If the Quick Open prompt is currently open, cancel it and return `true`.
    /// All four Quick Open variants (CommandPalette, QuickOpen, QuickOpenBuffers,
    /// QuickOpenFiles) toggle off when invoked while the picker is already visible.
    fn close_quick_open_if_open(&mut self) -> bool {
        if let Some(prompt) = &self.active_window_mut().prompt {
            if prompt.prompt_type == PromptType::QuickOpen {
                self.cancel_prompt();
                return true;
            }
        }
        false
    }

    /// Re-run the active search after a search-option flag is toggled.
    /// If a search prompt is open, updates incremental highlights from the
    /// prompt's current input. Otherwise re-executes the last completed search.
    fn refresh_active_search(&mut self) {
        if let Some(prompt) = &self.active_window_mut().prompt {
            if matches!(
                prompt.prompt_type,
                PromptType::Search | PromptType::ReplaceSearch | PromptType::QueryReplaceSearch
            ) {
                let query = prompt.input_str().to_string();
                // Drop the committed matches: they were collected under the
                // old flags, and F3/Shift+F3 now step through them while the
                // bar is open (issue #2111). Clearing makes the next press
                // re-run the search under the new flags.
                self.active_window_mut().search_state = None;
                self.update_search_highlights(&query);
            }
        } else if let Some(search_state) = &self.active_window().search_state {
            let query = search_state.query.clone();
            self.perform_search(&query);
        }
    }

    /// Open a terminal in the utility dock, creating the dock split if none exists yet.
    fn handle_open_terminal_in_dock(&mut self) -> AnyhowResult<()> {
        use crate::model::event::SplitDirection;
        use crate::view::split::SplitRole;

        if let Some(dock_leaf) = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .find_leaf_by_role(SplitRole::UtilityDock)
        {
            // Existing dock — focus it and let the regular open_terminal path attach a new tab.
            self.windows
                .get_mut(&self.active_window)
                .and_then(|w| w.split_manager_mut())
                .expect("active window must have a populated split layout")
                .set_active_split(dock_leaf);
            self.open_terminal();
            return Ok(());
        }

        // No dock yet. Spawn the PTY first so we have a real terminal buffer to seed the new
        // dock leaf with — otherwise the leaf would carry the user's previously-active buffer
        // as a placeholder and that buffer would linger as a phantom tab in the dock.
        let Some(terminal_id) = self.spawn_terminal_session() else {
            return Ok(());
        };
        let buffer_id = self.create_terminal_buffer_detached(terminal_id);

        // Split at the root so the dock spans the full width below any pre-existing side-by-side panes.
        let new_leaf = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_manager_mut())
            .expect("active window must have a populated split layout")
            .split_root_positioned(SplitDirection::Horizontal, buffer_id, 0.7, false)
            .map_err(|e| {
                self.set_status_message(format!("Failed to create dock for terminal: {}", e));
            });
        let Ok(new_leaf) = new_leaf else {
            return Ok(());
        };

        let mut view_state = crate::view::split::SplitViewState::with_buffer(
            self.terminal_width,
            self.terminal_height,
            buffer_id,
        );
        // Terminal-dedicated splits never show line numbers or current-line highlight.
        // (Mirrors the plugin-terminal split setup in `create_plugin_terminal`.)
        view_state.apply_config_defaults(crate::view::split::ViewConfigDefaults {
            line_numbers: false,
            highlight_current_line: false,
            line_wrap: self.active_window().resolve_line_wrap_for_buffer(buffer_id),
            wrap_indent: self.config.editor.wrap_indent,
            wrap_column: self
                .active_window()
                .resolve_wrap_column_for_buffer(buffer_id),
            rulers: self.config.editor.rulers.clone(),
            scroll_offset: 0,
        });
        // Terminals don't wrap — keep escape sequences intact.
        view_state.viewport.line_wrap_enabled = false;

        self.windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_view_states_mut())
            .expect("active window must have a populated split layout")
            .insert(new_leaf, view_state);
        self.windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_manager_mut())
            .expect("active window must have a populated split layout")
            .set_leaf_role(new_leaf, Some(SplitRole::UtilityDock));
        self.windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_manager_mut())
            .expect("active window must have a populated split layout")
            .set_active_split(new_leaf);

        // Mirror open_terminal's post-attach bookkeeping. The buffer was
        // created via `create_terminal_buffer_detached` (empty scrollback set),
        // so it is live in this split; focus the terminal pane.
        self.active_window_mut().key_context = crate::input::keybindings::KeyContext::Terminal;
        // The dock leaf was just split off; size its PTY to the pane the
        // grid gives it now, not the pane list of the last frame.
        self.resize_visible_terminals();

        let exit_key = self
            .keybindings
            .read()
            .unwrap()
            .find_keybinding_for_action(
                "terminal_escape",
                crate::input::keybindings::KeyContext::Terminal,
            )
            .unwrap_or_else(|| "Ctrl+Space".to_string());
        self.set_status_message(
            fresh_i18n::t!("terminal.opened", id = terminal_id.0, exit_key = exit_key).to_string(),
        );
        tracing::info!(
            "Opened terminal {:?} into new dock leaf {:?} (buffer {:?})",
            terminal_id,
            new_leaf,
            buffer_id
        );
        Ok(())
    }

    /// Run one of the `prompt_*` actions by re-entering the prompt's own
    /// input handler with the key that means the same thing.
    ///
    /// The prompt keeps its editing logic in `Prompt`'s `InputHandler` and runs
    /// it ahead of keybinding resolution, so an action arm that reimplemented
    /// (say) suggestion navigation would be a second copy of the sync, theme
    /// preview and plugin-notification behaviour, free to drift from the one
    /// the arrow keys use. Delegating keeps exactly one implementation.
    ///
    /// A no-op when no prompt is open — an action bound in the `prompt`
    /// context can still be reached from elsewhere.
    fn dispatch_prompt_key_for_action(
        &mut self,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) {
        use crate::input::handler::{InputContext, InputHandler};
        let mut ctx = InputContext::new();
        let event = crossterm::event::KeyEvent::new(code, modifiers);
        let handled = match self.active_window_mut().prompt.as_mut() {
            Some(prompt) => {
                prompt.handle_key_event(&event, &mut ctx);
                true
            }
            None => false,
        };
        if handled {
            self.process_deferred_actions(ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::*;
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use crate::model::filesystem::StdFileSystem;
    use crate::view::color_support::ColorCapability;
    use std::sync::Arc;

    fn test_editor() -> (Editor, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let editor = Editor::for_test(
            Config::default(),
            80,
            24,
            Some(temp.path().to_path_buf()),
            DirectoryContext::for_testing(temp.path()),
            ColorCapability::TrueColor,
            Arc::new(StdFileSystem),
            None,
            None,
            false,
            false,
            false,
        )
        .unwrap();
        (editor, temp)
    }
    #[test]
    fn handle_copy_test() {
        let cases = [
            ("partial selection", "hello world", Some(0), 5, "hello"),
            ("whole buffer", "hello", Some(0), 5, "hello"),
            ("backward selection", "hello", Some(5), 0, "hello"),
            ("multiline selection", "one\ntwo", Some(0), 7, "one\ntwo"),
            ("unicode selection", "café", Some(0), 5, "café"),
            ("no selection", "hello", None, 2, "hello"),
            ("empty buffer", "", None, 0, "old clipboard"),
        ];

        for (name, source, anchor, position, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.set_clipboard_for_test("old clipboard".into());
            editor.active_state_mut().buffer.insert(0, source);

            let cursor = editor.active_cursors_mut().primary_mut();
            cursor.anchor = anchor;
            cursor.position = position;

            editor.handle_copy().unwrap();

            assert_eq!(editor.clipboard_content_for_test(), expected, "{name}");
            let actual = editor.active_state_mut().get_text_range(0, source.len());
            assert_eq!(actual, source, "{name}: source must remain unchanged");
        }
    }

    #[test]
    fn handle_paste_test() {
        // name, source, anchor, cursor position, clipboard, expected buffer
        let cases = [
            ("empty buffer", "", None, 0, "hello", "hello"),
            ("insert at start", "world", None, 0, "hello ", "hello world"),
            ("insert at end", "hello", None, 5, " world", "hello world"),
            ("insert in middle", "ac", None, 1, "b", "abc"),
            (
                "replace selection",
                "hello world",
                Some(6),
                11,
                "Rust",
                "hello Rust",
            ),
            (
                "backward selection",
                "hello world",
                Some(11),
                6,
                "Rust",
                "hello Rust",
            ),
            ("empty clipboard", "hello", None, 5, "", "hello"),
            (
                "empty paste with selection",
                "hello",
                Some(0),
                5,
                "",
                "hello",
            ),
            ("normalize newlines", "", None, 0, "a\r\nb\rc", "a\nb\nc"),
            ("unicode paste", "Hi ", None, 3, "café", "Hi café"),
        ];

        for (name, source, anchor, position, clipboard, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, source);
            editor.set_clipboard_for_test(clipboard.into());

            let cursor = editor.active_cursors_mut().primary_mut();
            cursor.anchor = anchor;
            cursor.position = position;

            editor.handle_paste().unwrap();

            let len = editor.active_state().buffer.len();
            let actual = editor.active_state_mut().get_text_range(0, len);
            assert_eq!(actual, expected, "{name}");
        }
    }

    #[test]
    fn handle_dispatch_save_test() {
        let cases = [("unnamed buffer", false), ("named buffer", true)];
        for (name, named) in cases {
            let (mut editor, temp) = test_editor();
            editor.new_buffer();
            let path = temp.path().join("save.txt");
            if named {
                std::fs::write(&path, "before").unwrap();
                editor.open_file(&path).unwrap();
            }
            editor.active_state_mut().buffer.insert(0, "after");

            editor.handle_dispatch_save();

            if named {
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    "afterbefore",
                    "{name}"
                );
                assert!(!editor.active_state().buffer.is_modified(), "{name}");
            } else {
                assert!(
                    matches!(
                        editor.active_window().prompt.as_ref().unwrap().prompt_type,
                        PromptType::SaveFileAs
                    ),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn handle_dispatch_save_as_test() {
        let cases = [
            ("unnamed buffer", None, ""),
            ("named buffer", Some("note.txt"), "note.txt"),
        ];
        for (name, filename, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            if let Some(filename) = filename {
                let path = editor.working_dir().join(filename);
                editor.active_state_mut().buffer.set_file_path(path);
            }

            editor.handle_dispatch_save_as();

            let prompt = editor.active_window().prompt.as_ref().unwrap();
            assert!(
                matches!(prompt.prompt_type, PromptType::SaveFileAs),
                "{name}"
            );
            assert_eq!(prompt.get_text(), expected, "{name}");
        }
    }

    #[test]
    fn handle_dispatch_save_all_test() {
        let cases = [("no changes", false), ("modified file", true)];
        for (name, modified) in cases {
            let (mut editor, temp) = test_editor();
            let path = temp.path().join("all.txt");
            std::fs::write(&path, "before").unwrap();
            editor.open_file(&path).unwrap();
            if modified {
                editor.active_state_mut().buffer.insert(0, "after");
            }

            editor.handle_dispatch_save_all();

            let expected = if modified { "afterbefore" } else { "before" };
            assert_eq!(std::fs::read_to_string(path).unwrap(), expected, "{name}");
            assert!(!editor.active_state().buffer.is_modified(), "{name}");
            let expected_status = if modified {
                t!("status.save_all", count = "1").to_string()
            } else {
                t!("status.save_all_none").to_string()
            };
            assert_eq!(
                editor.active_window().status_message.as_deref(),
                Some(expected_status.as_str()),
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_revert_test() {
        let cases = [("clean file", false), ("unsaved changes", true)];
        for (name, modified) in cases {
            let (mut editor, temp) = test_editor();
            let path = temp.path().join("revert.txt");
            std::fs::write(&path, "original").unwrap();
            editor.open_file(&path).unwrap();
            if modified {
                editor.active_state_mut().buffer.insert(0, "changed ");
            }

            editor.handle_dispatch_revert();

            if modified {
                assert!(
                    matches!(
                        editor.active_window().prompt.as_ref().unwrap().prompt_type,
                        PromptType::ConfirmRevert
                    ),
                    "{name}"
                );
                assert!(editor.active_state().buffer.is_modified(), "{name}");
            } else {
                assert!(editor.active_window().prompt.is_none(), "{name}");
                assert_eq!(
                    editor.active_state_mut().get_text_range(0, 8),
                    "original",
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn handle_dispatch_goto_line_test() {
        let cases = [("empty buffer", ""), ("multiple lines", "one\ntwo\nthree")];
        for (name, source) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, source);

            editor.handle_dispatch_goto_line();

            assert!(
                matches!(
                    editor.active_window().prompt.as_ref().unwrap().prompt_type,
                    PromptType::GotoLine
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_update_fresh_test() {
        let cases = [
            ("updates disabled", false, "update.disabled"),
            ("no update available", true, "update.up_to_date"),
        ];
        for (name, enabled, message) in cases {
            let (mut editor, _temp) = test_editor();
            editor.config_mut().self_update = enabled;

            editor.handle_dispatch_update_fresh();

            assert_eq!(
                editor.active_window().status_message.as_deref(),
                Some(t!(message).as_ref()),
                "{name}"
            );
            assert!(editor.active_window().prompt.is_none(), "{name}");
        }
    }

    #[test]
    fn handle_dispatch_read_only_test() {
        let cases: [(&str, fn(&mut Editor)); 7] = [
            ("format", Editor::handle_dispatch_format_buffer),
            (
                "trim whitespace",
                Editor::handle_dispatch_trim_trailing_whitespace,
            ),
            (
                "final newline",
                Editor::handle_dispatch_ensure_final_newline,
            ),
            ("cut", Editor::handle_dispatch_cut),
            ("delete backward", Editor::handle_dispatch_delete_backward),
            ("replace", Editor::handle_dispatch_replace),
            ("query replace", Editor::handle_dispatch_query_replace),
        ];
        for (name, handler) in cases {
            let (mut editor, _temp) = test_editor();
            let id = editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, "text  ");
            editor.active_cursors_mut().primary_mut().position = 6;
            editor.active_window_mut().mark_buffer_read_only(id, true);

            handler(&mut editor);

            assert_eq!(
                editor.active_state_mut().get_text_range(0, 6),
                "text  ",
                "{name}"
            );
            assert!(editor.active_window().prompt.is_none(), "{name}");
            assert_eq!(
                editor.active_window().status_message.as_deref(),
                Some(t!("buffer.editing_disabled").as_ref()),
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_trim_trailing_whitespace_test() {
        let cases = [
            ("trailing spaces", "one  \ntwo \n", "one\ntwo\n"),
            ("clean text", "one\ntwo", "one\ntwo"),
            ("empty buffer", "", ""),
        ];
        for (name, source, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, source);

            editor.handle_dispatch_trim_trailing_whitespace();

            let len = editor.active_state().buffer.len();
            assert_eq!(
                editor.active_state_mut().get_text_range(0, len),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_ensure_final_newline_test() {
        let cases = [
            ("missing newline", "one", "one\n"),
            ("existing newline", "one\n", "one\n"),
            ("empty buffer", "", ""),
        ];
        for (name, source, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, source);

            editor.handle_dispatch_ensure_final_newline();

            let len = editor.active_state().buffer.len();
            assert_eq!(
                editor.active_state_mut().get_text_range(0, len),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_cut_test() {
        let cases = [
            ("forward selection", Some(0), 5),
            ("backward selection", Some(5), 0),
        ];
        for (name, anchor, position) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.set_clipboard_for_test("old".into());
            editor.active_state_mut().buffer.insert(0, "hello world");
            let cursor = editor.active_cursors_mut().primary_mut();
            cursor.anchor = anchor;
            cursor.position = position;

            editor.handle_dispatch_cut();

            assert_eq!(editor.clipboard_content_for_test(), "hello", "{name}");
            assert_eq!(
                editor.active_state_mut().get_text_range(0, 6),
                " world",
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_select_all_test() {
        let cases = [
            ("empty buffer", ""),
            ("single line", "hello"),
            ("multiple lines", "one\ntwo"),
        ];
        for (name, source) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, source);

            editor.handle_dispatch_select_all().unwrap();

            let cursor = editor.active_cursors().primary();
            assert_eq!(cursor.position, source.len(), "{name}");
            assert_eq!(cursor.anchor, Some(0), "{name}");
        }
    }

    #[test]
    fn handle_dispatch_delete_backward_test() {
        let cases = [
            ("start of buffer", "abc", 0, "abc"),
            ("end of buffer", "abc", 3, "ab"),
            ("middle of buffer", "abc", 2, "ac"),
        ];
        for (name, source, position, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, source);
            editor.active_cursors_mut().primary_mut().position = position;

            editor.handle_dispatch_delete_backward();

            let len = editor.active_state().buffer.len();
            assert_eq!(
                editor.active_state_mut().get_text_range(0, len),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_smart_home_test() {
        let cases = [
            ("end of indented line", 7, 2),
            ("first non-whitespace", 2, 0),
            ("line start", 0, 2),
        ];
        for (name, position, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, "  hello");
            editor.active_cursors_mut().primary_mut().position = position;

            editor.handle_dispatch_smart_home();

            assert_eq!(
                editor.active_cursors().primary().position,
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_remove_secondary_cursors_test() {
        let cases = [("one cursor", 0), ("two cursors", 1), ("three cursors", 2)];
        for (name, secondary) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, "hello");
            for position in 1..=secondary {
                editor
                    .active_cursors_mut()
                    .add(crate::model::cursor::Cursor::new(position));
            }

            editor.handle_dispatch_remove_secondary_cursors();

            assert_eq!(editor.active_cursors().iter().count(), 1, "{name}");
            assert_eq!(
                editor.active_state_mut().get_text_range(0, 5),
                "hello",
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_command_palette_test() {
        let cases = [("closed palette", false), ("open palette", true)];
        for (name, already_open) in cases {
            let (mut editor, _temp) = test_editor();
            if already_open {
                editor.start_quick_open();
            }

            editor.handle_dispatch_command_palette();

            assert_eq!(
                editor.active_window().prompt.is_some(),
                !already_open,
                "{name}"
            );
            if !already_open {
                assert_eq!(
                    editor.active_window().prompt.as_ref().unwrap().get_text(),
                    ">",
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn handle_dispatch_toggle_utility_dock_test() {
        use crate::view::split::SplitRole;
        let cases = [("missing dock", false), ("existing dock", true)];
        for (name, has_dock) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            let original = editor.active_split_id();
            if has_dock {
                editor.split_pane_horizontal();
                let dock = editor.active_split_id();
                editor
                    .split_manager_mut()
                    .set_leaf_role(dock, Some(SplitRole::UtilityDock));
                editor.split_manager_mut().set_active_split(original);

                editor.handle_dispatch_toggle_utility_dock();

                assert_eq!(editor.active_split_id(), dock, "{name}");
                editor.handle_dispatch_toggle_utility_dock();
                assert_eq!(
                    editor.active_split_id(),
                    original,
                    "{name}: leave focused dock"
                );
            } else {
                editor.handle_dispatch_toggle_utility_dock();
                assert_eq!(editor.active_split_id(), original, "{name}");
                assert!(
                    editor
                        .active_window()
                        .status_message
                        .as_ref()
                        .unwrap()
                        .starts_with("No Utility Dock open"),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn handle_dispatch_cycle_live_grep_provider_test() {
        let cases = [
            ("no prompt", None),
            ("unrelated prompt", Some(PromptType::GotoLine)),
        ];
        for (name, prompt_type) in cases {
            let (mut editor, _temp) = test_editor();
            if let Some(prompt_type) = prompt_type {
                editor.start_prompt("prompt".into(), prompt_type);
            }

            editor.handle_dispatch_cycle_live_grep_provider().unwrap();

            assert_eq!(
                editor.active_window().status_message.as_deref(),
                Some("Cycle Live Grep provider only works inside Live Grep"),
                "{name}"
            );
            assert!(editor.pending_plugin_actions.is_empty(), "{name}");
        }
    }

    #[test]
    fn handle_dispatch_view_toggles_test() {
        for initial in [false, true] {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.config_mut().editor.line_wrap = initial;
            editor.config_mut().editor.highlight_current_line = initial;
            editor.config_mut().editor.highlight_occurrences = initial;
            editor.sync_windows_config();

            editor.handle_dispatch_toggle_line_wrap();
            editor.handle_dispatch_toggle_current_line_highlight();
            editor.handle_dispatch_toggle_occurrence_highlight();

            assert_eq!(editor.config.editor.line_wrap, !initial);
            assert_eq!(editor.config.editor.highlight_current_line, !initial);
            assert_eq!(editor.config.editor.highlight_occurrences, !initial);
            let split = editor.active_split_id();
            let view = editor
                .active_window()
                .buffers
                .split_view_states()
                .unwrap()
                .get(&split)
                .unwrap();
            assert_eq!(view.viewport.line_wrap_enabled, !initial);
            assert_eq!(view.highlight_current_line, !initial);
            assert_eq!(
                editor.active_state().reference_highlight_overlay.enabled,
                !initial
            );
        }
    }

    #[test]
    fn handle_dispatch_toggle_read_only_test() {
        for initial in [false, true] {
            let (mut editor, _temp) = test_editor();
            let id = editor.new_buffer();
            editor
                .active_window_mut()
                .mark_buffer_read_only(id, initial);

            editor.handle_dispatch_toggle_read_only();

            assert_eq!(
                editor
                    .active_window()
                    .buffer_metadata
                    .get(&id)
                    .unwrap()
                    .read_only,
                !initial
            );
        }
    }

    #[test]
    fn handle_dispatch_prompt_defaults_test() {
        let cases: [(&str, fn(&mut Editor), PromptType, &str); 4] = [
            (
                "page width",
                Editor::handle_dispatch_set_page_width,
                PromptType::SetPageWidth,
                "80",
            ),
            (
                "background",
                Editor::handle_dispatch_set_background,
                PromptType::SetBackgroundFile,
                "background.ansi",
            ),
            (
                "background blend",
                Editor::handle_dispatch_set_background_blend,
                PromptType::SetBackgroundBlend,
                "0.25",
            ),
            (
                "tab size",
                Editor::handle_dispatch_set_tab_size,
                PromptType::SetTabSize,
                "8",
            ),
        ];
        for (name, handler, expected_type, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            let split = editor.active_split_id();
            editor
                .active_window_mut()
                .buffers
                .split_view_states_mut()
                .unwrap()
                .get_mut(&split)
                .unwrap()
                .compose_width = Some(80);
            editor.ansi_background_path = Some(editor.working_dir().join("background.ansi"));
            editor.background_fade = 0.25;
            editor.active_state_mut().buffer_settings.tab_size = 8;

            handler(&mut editor);

            let prompt = editor.active_window().prompt.as_ref().unwrap();
            assert_eq!(
                std::mem::discriminant(&prompt.prompt_type),
                std::mem::discriminant(&expected_type),
                "{name}"
            );
            assert_eq!(prompt.get_text(), expected, "{name}");
        }
    }

    #[test]
    fn handle_dispatch_search_prompts_test() {
        let cases: [(&str, fn(&mut Editor), PromptType, bool); 3] = [
            (
                "search",
                Editor::handle_dispatch_search,
                PromptType::Search,
                false,
            ),
            (
                "replace",
                Editor::handle_dispatch_replace,
                PromptType::ReplaceSearch,
                false,
            ),
            (
                "query replace",
                Editor::handle_dispatch_query_replace,
                PromptType::QueryReplaceSearch,
                true,
            ),
        ];
        for (name, handler, expected_type, confirm_each) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();

            handler(&mut editor);

            let prompt = editor.active_window().prompt.as_ref().unwrap();
            assert_eq!(
                std::mem::discriminant(&prompt.prompt_type),
                std::mem::discriminant(&expected_type),
                "{name}"
            );
            assert_eq!(
                editor.active_window().search_confirm_each,
                confirm_each,
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_scroll_tabs_test() {
        let cases: [(&str, fn(&mut Editor), usize, usize); 4] = [
            (
                "left at zero",
                Editor::handle_dispatch_scroll_tabs_left,
                0,
                0,
            ),
            (
                "left clamps",
                Editor::handle_dispatch_scroll_tabs_left,
                3,
                0,
            ),
            (
                "left decrements",
                Editor::handle_dispatch_scroll_tabs_left,
                12,
                7,
            ),
            (
                "right increments",
                Editor::handle_dispatch_scroll_tabs_right,
                12,
                17,
            ),
        ];
        for (name, handler, initial, expected) in cases {
            let (mut editor, _temp) = test_editor();
            let split = editor.active_split_id();
            editor
                .active_window_mut()
                .buffers
                .split_view_states_mut()
                .unwrap()
                .get_mut(&split)
                .unwrap()
                .tab_scroll_offset = initial;

            handler(&mut editor);

            assert_eq!(
                editor
                    .active_window()
                    .buffers
                    .split_view_states()
                    .unwrap()
                    .get(&split)
                    .unwrap()
                    .tab_scroll_offset,
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_buffer_settings_test() {
        for initial in [false, true] {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer_settings.use_tabs = initial;

            editor.handle_dispatch_toggle_indentation_style();

            assert_eq!(editor.active_state().buffer_settings.use_tabs, !initial);
            assert_eq!(
                editor.active_state().buffer_settings.use_tabs_override,
                Some(!initial)
            );
            let ws = &mut editor.active_state_mut().buffer_settings.whitespace;
            ws.tabs_leading = initial;
            ws.tabs_inner = initial;
            ws.tabs_trailing = initial;
            let spaces_before = ws.spaces_leading;

            editor.handle_dispatch_toggle_tab_indicators();

            let ws = &editor.active_state().buffer_settings.whitespace;
            assert_eq!(ws.tabs_leading, !initial);
            assert_eq!(ws.tabs_inner, !initial);
            assert_eq!(ws.tabs_trailing, !initial);
            assert_eq!(ws.spaces_leading, spaces_before);
            assert_eq!(
                editor
                    .active_state()
                    .buffer_settings
                    .tab_indicators_override,
                Some(!initial)
            );

            editor
                .handle_dispatch_toggle_whitespace_indicators()
                .unwrap();

            let settings = &editor.active_state().buffer_settings;
            assert_eq!(
                settings.whitespace_override,
                Some(settings.whitespace.any_visible())
            );
            assert_eq!(settings.tab_indicators_override, None);
        }
    }

    #[test]
    fn handle_dispatch_toggle_dock_focus_test() {
        for initial in [false, true] {
            let (mut editor, _temp) = test_editor();
            editor.dock = Some(FloatingWidgetState {
                panel_key: crate::widgets::PanelKey::new("test", 1),
                width_pct: 30,
                height_pct: 100,
                placement: PanelPlacement::LeftDock,
                focused: initial,
                mode: None,
                scrollbar_zone_hovered: false,
                scrollbar_flash_until: None,
                fullscreen: false,
                focus_marker: false,
                label_align: Default::default(),
                title: None,
                closable: false,
                hovered_widget_key: String::new(),
                hovered_item_key: String::new(),
                hovered_popup_row: String::new(),
            });

            editor.handle_dispatch_toggle_dock_focus().unwrap();

            assert_eq!(editor.dock.as_ref().unwrap().focused, !initial);
        }
    }

    #[test]
    fn handle_dispatch_search_options_test() {
        for initial in [false, true] {
            let (mut editor, _temp) = test_editor();
            editor.active_window_mut().search_case_sensitive = initial;
            editor.active_window_mut().search_whole_word = initial;
            editor.active_window_mut().search_use_regex = initial;
            editor.active_window_mut().search_confirm_each = initial;

            editor.handle_dispatch_toggle_search_case_sensitive();
            editor.handle_dispatch_toggle_search_whole_word();
            editor.handle_dispatch_toggle_search_regex();
            editor.handle_dispatch_toggle_search_confirm_each();

            assert_eq!(editor.active_window().search_case_sensitive, !initial);
            assert_eq!(editor.active_window().search_whole_word, !initial);
            assert_eq!(editor.active_window().search_use_regex, !initial);
            assert_eq!(editor.active_window().search_confirm_each, !initial);
        }
    }

    #[test]
    fn handle_dispatch_load_plugin_from_buffer_test() {
        let cases = [
            ("invalid TypeScript", "const = ;"),
            ("unterminated string", "const value = \""),
        ];
        for (name, source) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.active_state_mut().buffer.insert(0, source);

            editor.handle_dispatch_load_plugin_from_buffer();

            let status = editor.active_window().status_message.as_ref().unwrap();
            #[cfg(feature = "plugins")]
            assert!(
                status.starts_with("Failed to load plugin:"),
                "{name}: {status}"
            );
            #[cfg(not(feature = "plugins"))]
            assert!(status.contains("Plugins not available"), "{name}: {status}");
            assert_eq!(
                editor.active_state_mut().get_text_range(0, source.len()),
                source,
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_init_edit_test() {
        let cases = [
            ("new init file", None),
            ("existing init file", Some("// existing init\n")),
        ];
        for (name, source) in cases {
            let (mut editor, _temp) = test_editor();
            let path = editor.dir_context.config_dir.join("init.ts");
            if let Some(source) = source {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, source).unwrap();
            }

            editor.handle_dispatch_init_edit();

            assert!(path.exists(), "{name}");
            assert_eq!(
                editor
                    .active_state()
                    .buffer
                    .file_path()
                    .unwrap()
                    .canonicalize()
                    .unwrap(),
                path.canonicalize().unwrap(),
                "{name}"
            );
            if let Some(source) = source {
                assert_eq!(std::fs::read_to_string(path).unwrap(), source, "{name}");
            }
        }
    }

    #[test]
    fn handle_dispatch_init_check_test() {
        let cases = [
            ("valid script", "const value = 1;", true),
            ("invalid script", "const = ;", false),
        ];
        for (name, source, valid) in cases {
            let (mut editor, _temp) = test_editor();
            let path = editor.dir_context.config_dir.join("init.ts");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();

            editor.handle_dispatch_init_check();

            let status = editor.active_window().status_message.as_ref().unwrap();
            #[cfg(feature = "plugins")]
            assert_eq!(status == "init.ts: ok", valid, "{name}: {status}");
            #[cfg(not(feature = "plugins"))]
            {
                let _ = valid;
                assert!(status.starts_with("init.ts:"), "{name}: {status}");
            }
        }
    }

    #[test]
    fn handle_dispatch_init_reload_test() {
        let cases = [
            ("missing script", None),
            ("invalid script", Some("const = ;")),
        ];
        for (name, source) in cases {
            let (mut editor, _temp) = test_editor();
            let id = editor.active_buffer();
            let path = editor.dir_context.config_dir.join("init.ts");
            if let Some(source) = source {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, source).unwrap();
            }

            editor.handle_dispatch_init_reload();

            assert_eq!(editor.active_buffer(), id, "{name}");
            assert_eq!(path.exists(), source.is_some(), "{name}");
            if let Some(source) = source {
                assert_eq!(std::fs::read_to_string(path).unwrap(), source, "{name}");
            }
        }
    }

    #[test]
    fn handle_dispatch_terminal_without_session_test() {
        let cases: [(&str, fn(&mut Editor)); 3] = [
            ("focus", Editor::handle_dispatch_focus_terminal),
            (
                "keyboard capture",
                Editor::handle_dispatch_toggle_keyboard_capture,
            ),
            ("paste", Editor::handle_dispatch_terminal_paste),
        ];
        for (name, handler) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            editor.set_clipboard_for_test("clipboard text".into());
            editor.active_state_mut().buffer.insert(0, "buffer text");
            let context = editor.active_window().key_context.clone();
            let capture = editor.active_window().keyboard_capture;

            handler(&mut editor);

            assert_eq!(editor.active_window().key_context, context, "{name}");
            assert_eq!(editor.active_window().keyboard_capture, capture, "{name}");
            assert_eq!(
                editor.active_state_mut().get_text_range(0, 11),
                "buffer text",
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_close_settings_test() {
        for dirty in [false, true] {
            let (mut editor, _temp) = test_editor();
            editor.open_settings();
            if dirty {
                editor
                    .settings_state
                    .as_mut()
                    .unwrap()
                    .set_pending_change("editor.tab_size", serde_json::json!(8));
            }

            editor.handle_dispatch_close_settings();

            let settings = editor.settings_state.as_ref().unwrap();
            assert_eq!(settings.visible, dirty);
            assert_eq!(settings.showing_confirm_dialog, dirty);
            assert_eq!(settings.has_changes(), dirty);
        }
    }

    #[test]
    fn handle_dispatch_settings_toggle_focus_test() {
        use crate::view::settings::state::FocusPanel;
        let cases = [
            ("categories to settings", 0, FocusPanel::Settings),
            ("settings to footer", 1, FocusPanel::Footer),
            ("footer to categories", 2, FocusPanel::Categories),
        ];
        for (name, steps, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.open_settings();
            for _ in 0..steps {
                editor.handle_dispatch_settings_toggle_focus();
            }

            editor.handle_dispatch_settings_toggle_focus();

            assert_eq!(
                editor.settings_state.as_ref().unwrap().focus_panel(),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn handle_dispatch_prompt_confirm_test() {
        let cases = [("no prompt", None), ("tab size prompt", Some("8"))];
        for (name, input) in cases {
            let (mut editor, _temp) = test_editor();
            editor.new_buffer();
            if let Some(input) = input {
                editor.start_prompt_with_initial_text(
                    "Tab size:".into(),
                    PromptType::SetTabSize,
                    input.into(),
                );
            }

            editor.handle_dispatch_prompt_confirm().unwrap();

            assert!(editor.active_window().prompt.is_none(), "{name}");
            if input.is_some() {
                assert_eq!(editor.active_state().buffer_settings.tab_size, 8, "{name}");
            }
        }
    }

    #[test]
    fn handle_dispatch_prompt_copy_cut_test() {
        let cases = [
            ("all text", None, "hello", ""),
            ("selection", Some((0, 2)), "he", "llo"),
            ("backward selection", Some((2, 0)), "he", "llo"),
        ];
        for (name, selection, copied, remaining) in cases {
            for cut in [false, true] {
                let (mut editor, _temp) = test_editor();
                editor.set_clipboard_for_test("old".into());
                editor.start_prompt_with_initial_text(
                    "Line:".into(),
                    PromptType::GotoLine,
                    "hello".into(),
                );
                let prompt = editor.active_window_mut().prompt.as_mut().unwrap();
                prompt.clear_selection();
                if let Some((anchor, cursor)) = selection {
                    prompt.select_range(anchor, cursor);
                }

                if cut {
                    editor.handle_dispatch_prompt_cut();
                } else {
                    editor.handle_dispatch_prompt_copy();
                }

                assert_eq!(
                    editor.clipboard_content_for_test(),
                    copied,
                    "{name}, cut={cut}"
                );
                assert_eq!(
                    editor.active_window().prompt.as_ref().unwrap().get_text(),
                    if cut { remaining } else { "hello" },
                    "{name}, cut={cut}"
                );
            }
        }
    }

    #[test]
    fn handle_dispatch_prompt_paste_test() {
        let cases = [
            ("empty input", "", "hello", "hello"),
            ("append", "12", "3", "123"),
            ("empty clipboard", "12", "", "12"),
        ];
        for (name, source, clipboard, expected) in cases {
            let (mut editor, _temp) = test_editor();
            editor.set_clipboard_for_test(clipboard.into());
            editor.start_prompt_with_initial_text(
                "Line:".into(),
                PromptType::GotoLine,
                source.into(),
            );
            editor
                .active_window_mut()
                .prompt
                .as_mut()
                .unwrap()
                .clear_selection();

            editor.handle_dispatch_prompt_paste();

            assert_eq!(
                editor.active_window().prompt.as_ref().unwrap().get_text(),
                expected,
                "{name}"
            );
        }
    }
}
