// Builder-generated Android soft-keyboard (IME) bridge (shared module).
//
// A bare GameActivity has no editable view, so Android's InputMethodManager cannot reliably show
// the soft keyboard and never delivers typed text. The generated Activity (see
// templates/android/SafActivity.java) installs a tiny, invisible but focusable `EditText` whose real
// InputConnection pushes the *whole* text state (`selection_start|selection_end|text`) into a queue
// on every edit. This module drains that queue once per frame, diffs the new state against the last
// one, and emits Backspace/Delete key events plus a Commit — never `DeleteSurrounding`, which egui
// TextEdit may reorder after the Commit and so delete the wrong character.
//
// ARTCRAFT_ANDROID_IME_BRIDGE_V5_DIFF
pub mod ime {
    use android_activity::AndroidApp;
    use jni::{
        JavaVM, jni_sig, jni_str,
        objects::{JObject, JString},
    };
    use std::sync::Arc;

    /// Emitted at startup so a device log (and the packaged `.so` strings) can confirm this shell —
    /// including the soft-keyboard bridge — is the one running.
    pub const IME_MARKER: &str = "ARTCRAFT_ANDROID_IME_BRIDGE_V5_DIFF";

    pub struct ImeBridge {
        app: AndroidApp,
        vm: Arc<JavaVM>,
        /// Set once the Activity reports it has no `imePoll` (an Activity that never adopted the IME
        /// host view), so the per-frame poll stops paying for a call that can only fail.
        unsupported: bool,
        /// Toàn bộ text IME đang giữ (đồng bộ với EditText bên Java).
        ime_text: String,
        /// Selection hiện tại, tính bằng UTF-16 offset (Android native).
        ime_selection: (usize, usize),
    }

    impl ImeBridge {
        pub fn new(app: &AndroidApp) -> Self {
            // SAFETY: AndroidApp owns the live VM pointer for the lifetime of the activity.
            let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut _) };
            Self {
                app: app.clone(),
                vm: Arc::new(vm),
                unsupported: false,
                ime_text: String::new(),
                ime_selection: (0, 0),
            }
        }

        /// Translate every keyboard event the Activity has queued since the last frame into the egui
        /// IME events the focused `TextEdit` understands. Call once per frame from `raw_input_hook`.
        pub fn poll(&mut self, raw: &mut egui::RawInput) {
            if self.unsupported {
                return;
            }
            loop {
                match self.next_state() {
                    Ok(Some(state)) => self.handle_text_state(&state, raw),
                    Ok(None) => break,
                    Err(error) => {
                        log::warn!("Android soft-keyboard bridge unavailable: {error}");
                        self.unsupported = true;
                        break;
                    }
                }
            }
        }

        /// Hide the soft keyboard by calling the Activity's `imeHide`. Used when focus is
        /// surrendered during scroll/draw so the keyboard doesn't persist.
        pub fn hide_ime(&self) -> Result<(), String> {
            self.vm
                .attach_current_thread(|env| -> jni::errors::Result<()> {
                    let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
                    let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
                    match env.call_method(
                        &*activity,
                        jni_str!("imeHide"),
                        jni_sig!("()V"),
                        &[],
                    ) {
                        Ok(_) => Ok(()),
                        Err(error) => {
                            env.exception_clear();
                            Err(error)
                        }
                    }
                })
                .map_err(|error| error.to_string())
        }

        fn next_state(&self) -> Result<Option<String>, String> {
            self.vm
                .attach_current_thread(|env| -> jni::errors::Result<Option<String>> {
                    let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
                    // SAFETY: AndroidApp retains this global Activity reference; as_cast_raw borrows
                    // it without taking ownership or attempting to delete the reference.
                    let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
                    // An Activity that never adopted the IME host has no `imePoll`; invoking it raises
                    // a pending `NoSuchMethodError`. jni deliberately leaves that exception for the
                    // caller to handle, and this thread is shared with the SAF bridge, so clear it
                    // before reporting the missing method (otherwise the next unrelated JNI call sees
                    // a stale pending exception).
                    let value = match env.call_method(
                        &*activity,
                        jni_str!("imePoll"),
                        jni_sig!("()Ljava/lang/String;"),
                        &[],
                    ) {
                        Ok(value) => value.l()?,
                        Err(error) => {
                            env.exception_clear();
                            return Err(error);
                        }
                    };
                    if value.is_null() {
                        return Ok(None);
                    }
                    let value = JString::cast_local(env, value)?;
                    let value: String = value.try_to_string(env)?;
                    Ok(Some(value))
                })
                .map_err(|error| error.to_string())
        }

        /// Diff new text state vs last state, push egui events.
        ///
        /// - **Cursor đúng**: chỉ Commit thêm text (không có preedit) → cursor luôn ở cuối.
        /// - **Realtime**: mỗi phím của LabanKey gửi state mới → commit ngay → chữ hiện tức thì.
        /// - **Không double text**: Backspace/Delete key events thay `DeleteSurrounding` →
        ///   xử lý tuần tự trong TextEdit, không bị reorder sau Commit.
        /// - **Không chữ "to"**: không dùng Preedit → mọi ký tự hiển thị với font chính,
        ///   kể cả U+1EC5 (ễ), U+1EEF (ữ).
        fn handle_text_state(&mut self, state_str: &str, raw: &mut egui::RawInput) {
            let mut parts = state_str.splitn(3, '|');
            let start: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let end: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let text = parts.next().unwrap_or("").to_owned();

            let old = std::mem::replace(&mut self.ime_text, text.clone());
            let old_chars: Vec<char> = old.chars().collect();
            let new_chars: Vec<char> = text.chars().collect();

            // Common prefix.
            let prefix = old_chars
                .iter()
                .zip(&new_chars)
                .take_while(|(a, b)| a == b)
                .count();

            // Common suffix, bounded by the cursor so a trailing run of identical characters
            // (or a character that just moved) doesn't get counted as unchanged.
            let old_cursor = Self::utf16_char_offset(&old, self.ime_selection.1);
            let new_cursor = Self::utf16_char_offset(&text, end);
            let suffix_limit = old_chars
                .len()
                .saturating_sub(old_cursor)
                .min(new_chars.len().saturating_sub(new_cursor));
            let suffix = old_chars[prefix..]
                .iter()
                .rev()
                .zip(new_chars[prefix..].iter().rev())
                .take(suffix_limit)
                .take_while(|(a, b)| a == b)
                .count();

            let removed_total = old_chars.len() - prefix - suffix;
            let added: String = new_chars[prefix..new_chars.len() - suffix].iter().collect();

            log::info!(
                "IME diff: old={:?} new={:?} prefix={} suffix={} removed={} added={:?} cursor={}..{}",
                old, text, prefix, suffix, removed_total, added, start, end
            );

            // Xoá removed bằng Backspace/Delete KEY EVENTS. Không dùng
            // `ImeEvent::DeleteSurrounding` vì egui TextEdit có thể xử lý Commit trước
            // DeleteSurrounding → xoá nhầm ký tự ("thaà" thay vì "thà", "toôi" thay vì "tôi").
            if removed_total > 0 {
                let removed_before = old_cursor.saturating_sub(prefix).min(removed_total);
                let removed_after = removed_total - removed_before;
                for _ in 0..removed_before {
                    raw.events.push(egui::Event::Key {
                        key: egui::Key::Backspace,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::default(),
                    });
                    raw.events.push(egui::Event::Key {
                        key: egui::Key::Backspace,
                        physical_key: None,
                        pressed: false,
                        repeat: false,
                        modifiers: egui::Modifiers::default(),
                    });
                }
                for _ in 0..removed_after {
                    raw.events.push(egui::Event::Key {
                        key: egui::Key::Delete,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::default(),
                    });
                    raw.events.push(egui::Event::Key {
                        key: egui::Key::Delete,
                        physical_key: None,
                        pressed: false,
                        repeat: false,
                        modifiers: egui::Modifiers::default(),
                    });
                }
            }

            // Sau đó insert new text. Bỏ newline thô (Enter đi qua key event riêng).
            let added_clean: String = added
                .chars()
                .filter(|c| *c != '\n' && *c != '\r')
                .collect();
            if !added_clean.is_empty() {
                raw.events.push(egui::Event::Ime(egui::ImeEvent::Commit(added_clean)));
            }

            // Newlines trong added → Enter key presses.
            let newline_count = added.chars().filter(|c| *c == '\n').count();
            for _ in 0..newline_count.min(8) {
                raw.events.push(egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::default(),
                });
                raw.events.push(egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::default(),
                });
            }

            self.ime_selection = (start, end);
        }

        /// UTF-16 offset → char offset (Android báo offset bằng UTF-16 code units).
        fn utf16_char_offset(text: &str, offset: usize) -> usize {
            let mut units = 0;
            let mut chars = 0;
            for ch in text.chars() {
                let next = units + ch.len_utf16();
                if next > offset {
                    break;
                }
                units = next;
                chars += 1;
            }
            chars
        }
    }
}
