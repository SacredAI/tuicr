use super::*;

impl App {
    /// Lazily build the vim overlay from the buffer on first key in comment mode
    /// (no-op unless `comment_vim` is on). Seeding from the buffer covers both
    /// fresh and existing-comment edits.
    pub fn ensure_comment_vim_editor(&mut self) {
        if !self.comment_vim_enabled || self.input_mode != InputMode::Comment {
            return;
        }
        if self.comment_vim_editor.is_none() {
            self.comment_vim_editor = Some(CommentVimEditor::from_buffer(
                &self.comment_buffer,
                self.comment_cursor,
            ));
        }
    }

    /// Header indicator for the comment box as `(text, warn)`, or `None` when
    /// vim is off. `warn` is true for the cancel-confirm hint so the renderer
    /// can paint it red. Shows the in-progress `:` command-line when active,
    /// otherwise the pending-confirm hint, otherwise the mode label.
    pub fn comment_vim_mode_label(&self) -> Option<(String, bool)> {
        if !self.comment_vim_enabled || self.input_mode != InputMode::Comment {
            return None;
        }
        if let Some(cmd) = &self.comment_vim_command {
            return Some((format!(":{cmd}"), false));
        }
        match self.comment_vim_pending {
            CommentVimPending::Save => return Some(("Enter again to save".to_string(), false)),
            CommentVimPending::Cancel => {
                return Some(("Esc/q again to cancel".to_string(), true));
            }
            CommentVimPending::None => {}
        }
        Some((
            self.comment_vim_editor
                .as_ref()
                .map_or("INSERT", CommentVimEditor::label)
                .to_string(),
            false,
        ))
    }

    /// True while a `:` command-line is being entered in the comment box.
    pub fn comment_vim_command_active(&self) -> bool {
        self.comment_vim_command.is_some()
    }

    /// Plain Enter in vim Normal mode: arm Save, or save on the second
    /// consecutive press (like `:w`).
    pub fn comment_vim_enter_normal(&mut self) {
        if self.comment_vim_pending == CommentVimPending::Save {
            self.comment_vim_pending = CommentVimPending::None;
            self.submit_comment_input();
        } else {
            self.comment_vim_pending = CommentVimPending::Save;
        }
    }

    /// Esc in vim Normal mode: arm Cancel, or cancel on the second consecutive
    /// press (like `:q`). A lone Esc does nothing but show the hint.
    pub fn comment_vim_esc_normal(&mut self) {
        if self.comment_vim_pending == CommentVimPending::Cancel {
            self.comment_vim_pending = CommentVimPending::None;
            self.exit_comment_mode();
        } else {
            self.comment_vim_pending = CommentVimPending::Cancel;
        }
    }

    /// Reset the pending double-press state; called when any other key
    /// interrupts the sequence.
    pub fn comment_vim_reset_pending(&mut self) {
        self.comment_vim_pending = CommentVimPending::None;
    }

    /// Open the `:` command-line (vim Normal mode).
    pub fn start_comment_vim_command(&mut self) {
        self.comment_vim_command = Some(String::new());
    }

    /// Append a typed character to the `:` command-line.
    pub fn comment_vim_command_push(&mut self, c: char) {
        if let Some(cmd) = self.comment_vim_command.as_mut() {
            cmd.push(c);
        }
    }

    /// Backspace in the `:` command-line; backspacing past `:` closes it.
    pub fn comment_vim_command_backspace(&mut self) {
        if let Some(cmd) = self.comment_vim_command.as_mut() {
            if cmd.is_empty() {
                self.comment_vim_command = None;
            } else {
                cmd.pop();
            }
        }
    }

    /// Abandon the `:` command-line without running anything.
    pub fn comment_vim_command_cancel(&mut self) {
        self.comment_vim_command = None;
    }

    /// Execute the typed `:` command: `w`/`wq`/`x` save, `q`/`q!` cancel.
    pub fn run_comment_vim_command(&mut self) {
        let cmd = self.comment_vim_command.take().unwrap_or_default();
        match cmd.trim() {
            "w" | "wq" | "x" => self.submit_comment_input(),
            "q" | "q!" => self.exit_comment_mode(),
            "" => {}
            other => self.set_warning(format!("Not a comment command: :{other}")),
        }
    }

    /// True when the comment vim overlay exists and is in Normal mode.
    pub fn comment_vim_in_normal_mode(&self) -> bool {
        self.comment_vim_editor
            .as_ref()
            .is_some_and(CommentVimEditor::is_normal_mode)
    }

    /// Feed a key to the vim overlay and sync the result into the canonical
    /// `comment_buffer`/`comment_cursor`.
    pub fn comment_vim_feed_key(&mut self, key: crossterm::event::KeyEvent) {
        if let Some(editor) = self.comment_vim_editor.as_mut() {
            let (text, cursor) = editor.feed_key(key);
            self.comment_buffer = text;
            self.comment_cursor = cursor;
        }
    }

    /// Insert a soft tab (`comment_tab_width` spaces) into the vim overlay,
    /// used for Tab while typing in Insert mode.
    pub fn comment_vim_insert_soft_tab(&mut self) {
        let space = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(' '),
            crossterm::event::KeyModifiers::NONE,
        );
        for _ in 0..self.comment_tab_width {
            self.comment_vim_feed_key(space);
        }
    }

    /// Feed a bracketed-paste payload to the vim overlay and sync the result.
    pub fn comment_vim_feed_paste(&mut self, text: String) {
        if let Some(editor) = self.comment_vim_editor.as_mut() {
            let (text, cursor) = editor.feed_paste(text);
            self.comment_buffer = text;
            self.comment_cursor = cursor;
        }
    }

    /// Enable/disable vim editing at runtime (e.g. `:vim`); takes effect on the
    /// next comment session.
    pub fn set_comment_vim(&mut self, enabled: bool) {
        self.comment_vim_enabled = enabled;
        if !enabled {
            self.comment_vim_editor = None;
        }
        self.set_message(if enabled {
            "Vim mode enabled for the comment box"
        } else {
            "Vim mode disabled for the comment box"
        });
    }

    /// Remember that helix's `g` prefix was typed in the comment box, so the
    /// next key is read as a goto motion instead of reaching edtui.
    pub fn arm_comment_helix_goto(&mut self) {
        self.comment_helix_goto_pending = true;
    }

    /// Translate the key after a helix `g` into the edtui keys that perform the
    /// same motion, or `None` when no `g` is pending. An unrecognized key
    /// replays `g` followed by itself, so edtui's own `g` sequences still work.
    pub fn take_comment_helix_goto(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Option<Vec<crossterm::event::KeyEvent>> {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        if !std::mem::take(&mut self.comment_helix_goto_pending) {
            return None;
        }
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        Some(match key.code {
            KeyCode::Char('g') => vec![plain(KeyCode::Char('g')), plain(KeyCode::Char('g'))],
            KeyCode::Char('e') => vec![KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)],
            KeyCode::Char('h') => vec![plain(KeyCode::Home)],
            KeyCode::Char('l') => vec![plain(KeyCode::End)],
            _ => vec![plain(KeyCode::Char('g')), key],
        })
    }

    /// Toggle vim modal editing for the comment box.
    pub fn toggle_comment_vim(&mut self) {
        self.set_comment_vim(!self.comment_vim_enabled);
    }
}
