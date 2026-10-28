//! Voice input and output for the chat screen: microphone capture that
//! routes through Whisper, and text-to-speech playback of messages.

use std::sync::Arc;

use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};

use super::{ChatScreen, SpeechState};
use crate::ui::icons::{icon, spinner};
use crate::ui::theme;

impl ChatScreen {
    // ----- Voice: microphone to Whisper -----

    /// Mic button: start a recording, or stop it and transcribe.
    pub(super) fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        if self.transcribing || self.recording_starting {
            return;
        }
        if self.recording {
            self.finish_recording(cx);
        } else {
            self.begin_recording(cx);
        }
    }

    fn begin_recording(&mut self, cx: &mut Context<Self>) {
        self.stop_speech(cx);
        self.notice = None;
        self.recording_starting = true;
        let audio = Arc::clone(&self.audio);
        let bridge = cx.spawn(async move |this, cx| {
            let result = audio.start_recording().await;
            this.update(cx, |this, cx| {
                this.recording_starting = false;
                match result {
                    Ok(()) => this.recording = true,
                    Err(message) => this.notice = Some(message.into()),
                }
                cx.notify();
            })
            .ok();
        });
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    fn finish_recording(&mut self, cx: &mut Context<Self>) {
        self.recording = false;
        self.transcribing = true;
        cx.notify();
        let audio = Arc::clone(&self.audio);
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let bridge = cx.spawn(async move |this, cx| {
            let result = match audio.stop_recording().await {
                Ok(wav) => {
                    let task_backend = backend.clone();
                    backend
                        .spawn(async move { task_backend.transcribe_audio(&user_id, wav).await })
                        .await
                }
                .unwrap_or_else(|_| Err("Transcription was cancelled".to_string())),
                Err(message) => Err(message),
            };
            this.update(cx, |this, cx| {
                this.transcribing = false;
                match result {
                    Ok(text) if text.is_empty() => {
                        this.notice = Some("No speech was recognized".into());
                    }
                    Ok(text) => this.insert_transcript(&text, cx),
                    Err(message) => this.notice = Some(message.into()),
                }
                cx.notify();
            })
            .ok();
        });
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    /// Append a transcript to the composer, after a space when text is
    /// already there.
    fn insert_transcript(&mut self, transcript: &str, cx: &mut Context<Self>) {
        let Some(composer) = self.composer.clone() else {
            return;
        };
        composer.update(cx, |input, cx| {
            let current = input.text_ref().trim_end().to_string();
            let text = if current.is_empty() {
                transcript.to_string()
            } else {
                format!("{current} {transcript}")
            };
            input.set_text(&text, cx);
        });
    }

    // ----- Voice: text-to-speech -----

    /// Speak button: read a message aloud, or stop when it is the one
    /// already playing.
    fn toggle_speech(&mut self, item_id: String, text: String, cx: &mut Context<Self>) {
        if self
            .speech
            .as_ref()
            .is_some_and(|speech| speech.item_id == item_id)
        {
            self.stop_speech(cx);
            return;
        }
        self.speak(item_id, text, cx);
    }

    pub(super) fn stop_speech(&mut self, cx: &mut Context<Self>) {
        self.speech_generation += 1;
        self.audio.stop_playback(self.speech_generation);
        if self.speech.take().is_some() {
            cx.notify();
        }
    }

    /// Synthesize `text` chunk by chunk and queue each one as it lands,
    /// so playback starts after the first chunk instead of the last.
    fn speak(&mut self, item_id: String, text: String, cx: &mut Context<Self>) {
        self.stop_speech(cx);
        let chunks = speech_chunks(&text);
        if chunks.is_empty() {
            self.notice = Some("There is nothing to read aloud".into());
            cx.notify();
            return;
        }
        let generation = self.speech_generation;
        self.speech = Some(SpeechState {
            item_id,
            playing: false,
        });
        self.notice = None;
        cx.notify();

        let audio = Arc::clone(&self.audio);
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let voice = self.tts_voice.clone();
        let speed = self.tts_speed;
        let bridge = cx.spawn(async move |this, cx| {
            let is_current = |this: &gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                this.read_with(cx, |this, _| this.speech_generation == generation)
                    .unwrap_or(false)
            };
            for (ix, chunk) in chunks.into_iter().enumerate() {
                let task_backend = backend.clone();
                let user_id = user_id.clone();
                let voice = voice.clone();
                let synthesized = backend
                    .spawn(async move {
                        task_backend
                            .synthesize_speech(&user_id, chunk, voice, speed)
                            .await
                    })
                    .await
                    .unwrap_or_else(|_| Err("Text-to-speech was cancelled".to_string()));
                if !is_current(&this, cx) {
                    return;
                }
                let played = match synthesized {
                    Ok(wav) => audio.play(generation, wav).await,
                    Err(message) => Err(message),
                };
                if let Err(message) = played {
                    log::warn!("speech chunk {} failed: {message}", ix + 1);
                    this.update(cx, |this, cx| {
                        if this.speech_generation == generation {
                            this.speech = None;
                            this.notice = Some(message.into());
                            cx.notify();
                        }
                    })
                    .ok();
                    return;
                }
                if ix == 0 {
                    this.update(cx, |this, cx| {
                        if let Some(speech) = this.speech.as_mut()
                            && this.speech_generation == generation
                        {
                            speech.playing = true;
                            cx.notify();
                        }
                    })
                    .ok();
                }
            }
            audio.await_idle(generation).await;
            this.update(cx, |this, cx| {
                if this.speech_generation == generation && this.speech.take().is_some() {
                    cx.notify();
                }
            })
            .ok();
        });
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }
}

/// Hover-revealed button that reads one message aloud; stays visible and
/// turns into Stop while that message plays.
pub(super) fn speak_message_button(
    item_id: &str,
    group: &SharedString,
    text: SharedString,
    speech: Option<&SpeechState>,
    chat: gpui::WeakEntity<ChatScreen>,
) -> crate::ui::motion::HoverBackground {
    let item_id = item_id.to_string();
    let (glyph, label) = match speech {
        None => (
            icon("volume-2", px(12.), theme::text_secondary()).into_any_element(),
            "Speak",
        ),
        Some(SpeechState { playing: false, .. }) => (
            spinner(
                &format!("speak-{item_id}"),
                px(12.),
                theme::text_secondary(),
            ),
            "Preparing…",
        ),
        Some(SpeechState { playing: true, .. }) => (
            icon("square", px(12.), theme::text_secondary()).into_any_element(),
            "Stop",
        ),
    };
    let active = speech.is_some();
    div()
        .id(SharedString::from(format!("speak-message-{item_id}")))
        .flex()
        .items_center()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded(theme::RADIUS_SM)
        .text_xs()
        .text_color(gpui::rgb(theme::text_muted()))
        .opacity(if active { 1. } else { 0. })
        .group_hover(group.clone(), |style| style.opacity(1.))
        .map(|element| {
            let rest: gpui::Hsla = gpui::transparent_black();
            crate::ui::motion::hover_background(element, rest, gpui::rgb(theme::bg_elevated()))
        })
        .hover(|style| {
            style
                .text_color(gpui::rgb(theme::text_secondary()))
                .cursor_pointer()
        })
        .on_click(move |_event, _window, cx: &mut gpui::App| {
            cx.stop_propagation();
            let item_id = item_id.clone();
            let text = text.clone();
            chat.update(cx, |chat, cx| {
                chat.toggle_speech(item_id, text.to_string(), cx)
            })
            .ok();
        })
        .child(glyph)
        .child(label)
}

/// Longest chunk sent to text-to-speech, in words. Mirrors the Maple web
/// app; the model handles short passages best.
const SPEECH_CHUNK_MAX_WORDS: usize = 300;

/// Split markdown into plain-text chunks for text-to-speech: fenced code
/// and rules are dropped, inline markup is stripped, and paragraphs are
/// grouped up to [`SPEECH_CHUNK_MAX_WORDS`].
pub(crate) fn speech_chunks(text: &str) -> Vec<String> {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        let is_rule = compact.len() >= 3
            && (compact.chars().all(|c| c == '-')
                || compact.chars().all(|c| c == '*')
                || compact.chars().all(|c| c == '_'));
        let spoken = if is_rule {
            String::new()
        } else {
            strip_inline_markdown(trimmed)
        };
        if spoken.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&spoken);
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut chunk = String::new();
    let mut chunk_words = 0;
    for paragraph in paragraphs {
        let words = paragraph.split_whitespace().count();
        if chunk_words > 0 && chunk_words + words > SPEECH_CHUNK_MAX_WORDS {
            chunks.push(std::mem::take(&mut chunk));
            chunk_words = 0;
        }
        if words > SPEECH_CHUNK_MAX_WORDS {
            // One very long paragraph: split by sentence-ish boundaries.
            let mut piece = String::new();
            let mut piece_words = 0;
            for word in paragraph.split_whitespace() {
                if !piece.is_empty() {
                    piece.push(' ');
                }
                piece.push_str(word);
                piece_words += 1;
                let ends_sentence = word.ends_with(['.', '!', '?', ';', ':']);
                if piece_words >= SPEECH_CHUNK_MAX_WORDS
                    || (piece_words >= SPEECH_CHUNK_MAX_WORDS / 2 && ends_sentence)
                {
                    chunks.push(std::mem::take(&mut piece));
                    piece_words = 0;
                }
            }
            if !piece.is_empty() {
                chunk = piece;
                chunk_words = piece_words;
            }
            continue;
        }
        if !chunk.is_empty() {
            chunk.push(' ');
        }
        chunk.push_str(&paragraph);
        chunk_words += words;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    split_first_chunk(chunks)
}

/// Words in the opening chunk before the first sentence end that closes
/// it. Synthesis takes seconds per chunk, so a short opener starts
/// playback sooner while the rest is still on the way.
const SPEECH_FIRST_CHUNK_WORDS: usize = 40;

fn split_first_chunk(mut chunks: Vec<String>) -> Vec<String> {
    let Some(first) = chunks.first() else {
        return chunks;
    };
    let words: Vec<&str> = first.split_whitespace().collect();
    if words.len() <= SPEECH_FIRST_CHUNK_WORDS * 2 {
        return chunks;
    }
    let Some(split_at) = words
        .iter()
        .enumerate()
        .skip(SPEECH_FIRST_CHUNK_WORDS)
        .take(SPEECH_FIRST_CHUNK_WORDS)
        .find(|(_, word)| word.ends_with(['.', '!', '?', ';', ':']))
        .map(|(ix, _)| ix + 1)
    else {
        return chunks;
    };
    let opener = words[..split_at].join(" ");
    let rest = words[split_at..].join(" ");
    chunks[0] = rest;
    chunks.insert(0, opener);
    chunks
}

/// Drop heading marks, list markers, emphasis, inline code ticks, and
/// link targets from one markdown line.
fn strip_inline_markdown(line: &str) -> String {
    let mut rest = line.trim_start_matches('#').trim_start();
    rest = rest.trim_start_matches('>').trim_start();
    if let Some(stripped) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        rest = stripped;
    } else if let Some(dot) = rest.find(". ")
        && dot <= 3
        && rest[..dot].chars().all(|c| c.is_ascii_digit())
    {
        rest = &rest[dot + 2..];
    }
    if let Some(stripped) = rest
        .strip_prefix("[ ] ")
        .or_else(|| rest.strip_prefix("[x] "))
    {
        rest = stripped;
    }

    // Links: keep the label, drop the target. Images: drop entirely.
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '!' if chars.peek() == Some(&'[') => {
                let tail: String = chars.clone().skip(1).collect();
                if let Some((_, skip)) = link_span(&tail) {
                    // The `[` and the span behind it.
                    for _ in 0..=skip {
                        chars.next();
                    }
                }
            }
            '[' => {
                let tail: String = chars.clone().collect();
                match link_span(&tail) {
                    Some((label, skip)) => {
                        out.push_str(label);
                        for _ in 0..skip {
                            chars.next();
                        }
                    }
                    None => out.push(c),
                }
            }
            '*' | '_' | '`' | '~' => {}
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A link `label](target)` at the start of `tail` (the text after `[`):
/// the label and the number of characters through the closing `)`. The
/// count is in characters, since the caller advances a char iterator.
/// `None` when the first `]` is not followed by `(`, so an unrelated
/// bracket pair earlier in the line is left alone.
fn link_span(tail: &str) -> Option<(&str, usize)> {
    let close = tail.find(']')?;
    let rest = &tail[close..];
    if !rest.starts_with("](") {
        return None;
    }
    let end = rest.find(')')?;
    Some((&tail[..close], tail[..close + end + 1].chars().count()))
}

#[cfg(test)]
mod speech_tests {
    use super::*;

    #[test]
    fn chunks_drop_code_and_markup() {
        let text = "# Title\n\nSee [the docs](https://x.y) for **bold** `code`.\n\n```rs\nfn x() {}\n```\n\n---\n\n- item one\n1. item two\n![alt](img.png)";
        assert_eq!(
            speech_chunks(text),
            vec!["Title See the docs for bold code. item one item two".to_string()]
        );
    }

    #[test]
    fn links_strip_by_character_not_byte() {
        assert_eq!(strip_inline_markdown("[café](x) rest"), "café rest");
        assert_eq!(strip_inline_markdown("![ünï](p.png) after"), "after");
        assert_eq!(
            strip_inline_markdown("see [naïve](https://x.y/ü)!"),
            "see naïve!"
        );
    }

    #[test]
    fn unrelated_brackets_are_not_links() {
        assert_eq!(strip_inline_markdown("a [b] c [d](e) f"), "a [b] c d f");
        assert_eq!(
            strip_inline_markdown("see [x] then (y)"),
            "see [x] then (y)"
        );
        assert_eq!(strip_inline_markdown("![alt] no link"), "[alt] no link");
    }

    #[test]
    fn chunks_group_paragraphs_up_to_the_word_cap() {
        let paragraph = "word ".repeat(200).trim().to_string();
        let text = format!("{paragraph}\n\n{paragraph}\n\n{paragraph}");
        let chunks = speech_chunks(&text);
        // No sentence end in the opener, so it is not split off.
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.split_whitespace().count() == 200)
        );
    }

    #[test]
    fn a_short_opener_is_split_off_at_a_sentence() {
        let sentence = "one two three four five six seven eight nine ten. ";
        let chunks = speech_chunks(&sentence.repeat(12));
        assert_eq!(chunks[0].split_whitespace().count(), 50);
        assert!(chunks[0].ends_with("ten."));
        assert_eq!(chunks[1].split_whitespace().count(), 70);
    }

    #[test]
    fn a_long_paragraph_splits_at_sentences() {
        let sentence = "one two three four five six seven eight nine ten. ";
        let text = sentence.repeat(50);
        let chunks = speech_chunks(&text);
        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.split_whitespace().count() <= SPEECH_CHUNK_MAX_WORDS)
        );
        assert!(chunks.iter().all(|chunk| chunk.ends_with('.')));
    }

    #[test]
    fn blank_text_has_no_chunks() {
        assert!(speech_chunks("```\ncode only\n```").is_empty());
        assert!(speech_chunks("   \n\n").is_empty());
    }
}
