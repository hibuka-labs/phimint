//! Reply-language anchor: keep the user's language in the system prompt.
//!
//! The model's visible output drifts to English under a long coding session —
//! the system prompt, the tool schemas and every tool result are English, so
//! the context's language field pulls the reply away from the user's own
//! words (session 20261116: a Chinese conversation answered in English from
//! the second turn on). A one-shot rule in the static prompt decays for the
//! same reason; this anchor is re-derived before **every** LLM call from the
//! conversation itself, so it cannot go stale and costs zero state.
//!
//! Shape of the mechanism (the `SkillCatalogRefreshMiddleware` contract —
//! current-state statement, refreshed in place, never accumulated):
//!
//! - **Detect** — [`detect_from_messages`] walks the conversation backwards to
//!   the newest *real* user message (skipping `synthetic` machine input and
//!   `ephemeral` skill bodies — both are English and neither is the user's
//!   words) and runs the deterministic [`detect`] script heuristic on it. An
//!   undetectable message ("ok", `cargo test`) falls further back instead of
//!   answering, so a short message inherits the last language that spoke.
//! - **Anchor** — a `<reply-language>…</reply-language>` region appended to
//!   the system message. The tag pair is the region boundary: it does not
//!   participate in the `\n\n## ` heading surgery the skills catalog and the
//!   browser tool-face do, and it cannot collide with a `## Language` heading
//!   the user's own CLAUDE.md might carry.
//! - **Refresh** — [`refresh_language_section`] replaces the region in place
//!   (or appends it once). Same detection → same bytes → the provider-side
//!   prompt cache keeps its prefix; only a real language change moves bytes.
//!
//! Detection is pure code — Unicode script ranges plus a function-word vote
//! for the Latin languages. No model call, no language library, microseconds.
//! The wording is a fact ("the user writes in X") rather than a request, and
//! the language's own script rides along (`Chinese (中文)`) — the most direct
//! signal a model can get.

use async_trait::async_trait;
use phi_agent::{AgentResult, ChatMessage, Middleware, PreLlmCtx};

/// Opening tag of the anchor region, lead-in included (the `\n\n` separator
/// callers prepend, same convention as the catalog and browser anchors).
const REGION_START: &str = "\n\n<reply-language>\n";
/// Closing tag — the region end anchor. A forged earlier copy cannot stop
/// the search: the region is found from the START tag, and the END tag after
/// it is the only one that closes this region.
const REGION_END: &str = "\n</reply-language>";

/// The languages the anchor can name outright. Latin-script languages beyond
/// these fall through to the mirror sentence (the model reads the message);
/// scripts beyond Cyrillic fall through the same way. `en` carries no native
/// gloss — the English name already is the native form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Lang {
    Zh,
    En,
    Ja,
    Ko,
    Ru,
    Fr,
    De,
    Es,
    Pt,
}

impl Lang {
    /// The English name of the language (the anchor's primary noun).
    fn name(self) -> &'static str {
        match self {
            Lang::Zh => "Chinese",
            Lang::En => "English",
            Lang::Ja => "Japanese",
            Lang::Ko => "Korean",
            Lang::Ru => "Russian",
            Lang::Fr => "French",
            Lang::De => "German",
            Lang::Es => "Spanish",
            Lang::Pt => "Portuguese",
        }
    }

    /// The language written in its own script, `None` when the English name
    /// already is the native form (`English (English)` reads as a stutter).
    fn native(self) -> Option<&'static str> {
        match self {
            Lang::Zh => Some("中文"),
            Lang::En => None,
            Lang::Ja => Some("日本語"),
            Lang::Ko => Some("한국어"),
            Lang::Ru => Some("русский"),
            Lang::Fr => Some("français"),
            Lang::De => Some("Deutsch"),
            Lang::Es => Some("español"),
            Lang::Pt => Some("português"),
        }
    }

    /// The anchor sentence as a fact: who the user is, what to do about it.
    fn anchor_sentence(self) -> String {
        let who = match self.native() {
            Some(native) => format!("{} ({})", self.name(), native),
            None => self.name().to_string(),
        };
        format!("The user writes in {who}. Write everything you show the user in {who}.")
    }
}

/// What the detection concluded for this conversation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Detection {
    /// A table language: the anchor names it outright.
    Named(Lang),
    /// Letters seen, language outside the table (Greek, Arabic, Thai, …, or a
    /// Latin language the vote could not separate): mirror the user instead
    /// of pinning a guess.
    Mirror,
    /// Nothing to go on — no letters, too short, or a code-only line. The
    /// caller inherits the previous state (or ships no anchor at all).
    Unknown,
}

/// Script buckets counted by [`detect`].
#[derive(Default)]
struct Counts {
    kana: usize,
    hangul: usize,
    han: usize,
    cyrillic: usize,
    latin: usize,
    other: usize,
}

/// Unicode script of one character. Non-letters are classified first so the
/// ranges below only ever see alphabetic input (`0x00C0..=0x024F` contains
/// `×`/`÷`, the ASCII range contains `[\]^_``).
fn script(c: char) -> Option<Script> {
    if !c.is_alphabetic() {
        return None;
    }
    let u = c as u32;
    Some(match u {
        0x3040..=0x30FF => Script::Kana,   // ひらがな + カタカナ
        0xAC00..=0xD7AF => Script::Hangul, // 한글 syllables
        0x4E00..=0x9FFF | 0x3400..=0x4DBF => Script::Han, // CJK unified + ext A
        0x0400..=0x04FF => Script::Cyrillic,
        0x0041..=0x007A | 0x00C0..=0x024F => Script::Latin,
        _ => Script::Other, // Greek, Arabic, Thai, Hebrew, …
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    Kana,
    Hangul,
    Han,
    Cyrillic,
    Latin,
    Other,
}

/// Ukrainian-exclusive Cyrillic letters: their presence means the message is
/// not Russian, and the table has no Ukrainian entry — mirror instead of
/// pinning the wrong neighbour's language.
const UKRAINIAN_ONLY: [char; 4] = ['і', 'ї', 'є', 'ґ'];

/// Deterministic script detection on one message. Precedence follows what is
/// *unambiguous* first: kana never appears in Chinese, hangul never in either,
/// so a mixed script answers before the vote is even asked.
pub(crate) fn detect(text: &str) -> Detection {
    let mut counts = Counts::default();
    let mut letters = 0usize;
    for c in text.chars() {
        let Some(script) = script(c) else { continue };
        letters += 1;
        match script {
            Script::Kana => counts.kana += 1,
            Script::Hangul => counts.hangul += 1,
            Script::Han => counts.han += 1,
            Script::Cyrillic => counts.cyrillic += 1,
            Script::Latin => counts.latin += 1,
            Script::Other => counts.other += 1,
        }
    }
    if letters == 0 {
        return Detection::Unknown;
    }

    // Unambiguous scripts first. Two kana/hangul characters are decisive —
    // Chinese slang writes `bugの修复` with ONE kana, which stays Chinese
    // below the threshold and is exactly right.
    if counts.kana >= 2 {
        return Detection::Named(Lang::Ja);
    }
    if counts.hangul >= 2 {
        return Detection::Named(Lang::Ko);
    }
    // Two han is the shortest string a person would actually send in Chinese
    // ("这个bug"); an all-kanji Japanese line is the accepted false positive —
    // real Japanese carries kana and will be caught above.
    if counts.han >= 2 {
        return Detection::Named(Lang::Zh);
    }
    if counts.cyrillic >= 3 {
        return if text.chars().any(|c| UKRAINIAN_ONLY.contains(&c)) {
            Detection::Mirror
        } else {
            Detection::Named(Lang::Ru)
        };
    }
    if counts.other >= 3 {
        return Detection::Mirror;
    }
    if counts.latin >= 8 {
        return latin_vote(text);
    }
    // Too few letters to say anything ("ok", "go", "3 个") — the caller
    // inherits the previous language rather than guessing.
    Detection::Unknown
}

/// Function words for the Latin table languages. Grammar words, never content
/// words: `code`/`fix`/`file` appear in every language's technical writing
/// and would vote for English out of any German sentence.
const EN_WORDS: &[&str] = &[
    "the", "you", "your", "that", "this", "these", "those", "is", "are", "was", "were", "have",
    "has", "had", "will", "would", "can", "could", "should", "must", "not", "but", "and", "for",
    "with", "from", "about", "what", "which", "who", "when", "where", "why", "how", "please",
    "thanks", "there", "their", "they", "them", "then", "than", "just", "also", "very", "more",
    "most", "some", "any", "each", "other", "because", "before", "after", "into", "over",
];
const FR_WORDS: &[&str] = &[
    "le", "la", "les", "une", "des", "est", "sont", "vous", "nous", "pour", "dans", "avec", "que",
    "qui", "pas", "je", "tu", "il", "elle", "ce", "cette", "ces", "mais", "donc", "alors", "suis",
    "sur", "par", "au", "aux", "du", "ont", "été", "être",
];
const DE_WORDS: &[&str] = &[
    "der", "die", "das", "ist", "sind", "nicht", "und", "ich", "eine", "einen", "einem", "mit",
    "auf", "für", "auch", "aber", "zu", "den", "dem", "des", "von", "bei", "aus", "noch", "wie",
    "wenn", "schon", "mir", "dich", "wir", "ihr", "kann", "muss",
];
const ES_WORDS: &[&str] = &[
    "el", "los", "las", "una", "unos", "unas", "es", "son", "por", "con", "que", "pero", "como",
    "muy", "ya", "esto", "esta", "este", "estos", "estas", "del", "sobre", "entre", "cuando",
    "porque", "también", "más", "está", "están",
];
const PT_WORDS: &[&str] = &[
    "os", "uma", "são", "não", "para", "com", "mas", "mais", "você", "vocês", "está", "estão",
    "isso", "essa", "esse", "também", "quando", "porque", "muito", "tem", "que", "por",
];

/// One language's ballot in [`latin_vote`].
struct Ballot {
    lang: Lang,
    words: &'static [&'static str],
}

const BALLOTS: &[Ballot] = &[
    Ballot {
        lang: Lang::En,
        words: EN_WORDS,
    },
    Ballot {
        lang: Lang::Fr,
        words: FR_WORDS,
    },
    Ballot {
        lang: Lang::De,
        words: DE_WORDS,
    },
    Ballot {
        lang: Lang::Es,
        words: ES_WORDS,
    },
    Ballot {
        lang: Lang::Pt,
        words: PT_WORDS,
    },
];

/// Vote among the Latin table languages on the message's words.
///
/// Two or more hits and a strict lead names the language; a contested or
/// single-hit result mirrors (the model reads the message itself, which is
/// right for Dutch, Vietnamese, or a half-remembered French phrase); zero
/// hits means the letters are code (`cargo test`) — nothing to say, inherit.
fn latin_vote(text: &str) -> Detection {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty() && w.len() >= 2)
        .map(|w| w.to_lowercase())
        .collect();

    let scores: Vec<(Lang, usize)> = BALLOTS
        .iter()
        .map(|b| {
            let hits = words
                .iter()
                .filter(|w| b.words.contains(&w.as_str()))
                .count();
            (b.lang, hits)
        })
        .collect();

    let best = scores.iter().map(|(_, s)| *s).max().unwrap_or(0);
    if best == 0 {
        return Detection::Unknown;
    }
    let leaders: Vec<Lang> = scores
        .iter()
        .filter(|(_, s)| *s == best)
        .map(|(l, _)| *l)
        .collect();
    if best >= 2 && leaders.len() == 1 {
        Detection::Named(leaders[0])
    } else {
        Detection::Mirror
    }
}

/// The newest language that actually spoke in this conversation.
///
/// Walks backwards over **real** user messages only: `synthetic` (rotation
/// seed, tool-failure notices, child-result batches) and `ephemeral` (skill
/// bodies — English templates) are machine words, never the user's. An
/// undetectable message ("ok", a pasted command) skips to the one before it,
/// which is the inherit-the-last-language rule for free. Derived from the
/// messages alone — no hidden state to desync, and a resumed session keeps
/// its language because the trail does.
pub(crate) fn detect_from_messages(messages: &[ChatMessage]) -> Detection {
    for message in messages.iter().rev() {
        if let ChatMessage::User {
            content,
            ephemeral,
            synthetic,
            ..
        } = message
            && !*ephemeral
            && !*synthetic
        {
            let detection = detect(content);
            if detection != Detection::Unknown {
                return detection;
            }
        }
    }
    Detection::Unknown
}

/// Render the anchor region for a detection: `None` leaves the prompt alone.
fn render_region(detection: Detection) -> Option<String> {
    let sentence = match detection {
        Detection::Unknown => return None,
        Detection::Mirror => {
            "Write everything you show the user in the same language the user writes in."
                .to_string()
        }
        Detection::Named(lang) => lang.anchor_sentence(),
    };
    Some(format!("{REGION_START}{sentence}{REGION_END}"))
}

/// Locate the existing anchor region: from the start tag through the first
/// end tag after it.
fn find_region(content: &str) -> Option<(usize, usize)> {
    let start = content.find(REGION_START)?;
    let search_from = start + REGION_START.len();
    let end = content[search_from..]
        .find(REGION_END)
        .map(|rel| search_from + rel + REGION_END.len())?;
    Some((start, end))
}

/// Put the anchor region into `content`: replace the existing one in place,
/// append once when absent. Steady state — same detection, same region bytes
/// — is a fixed point, so an unchanged language keeps the prompt byte-identical
/// for provider-side caching. Content outside the region is preserved
/// byte-for-byte, and an `Unknown` detection with no region changes nothing.
pub(crate) fn refresh_language_section(content: &str, detection: Detection) -> String {
    let Some(region) = render_region(detection) else {
        return content.to_string();
    };
    match find_region(content) {
        Some((start, end)) => format!("{}{}{}", &content[..start], region, &content[end..]),
        None => format!("{content}{region}"),
    }
}

/// Per-turn language anchor (see the module docs). Stateless: the detection
/// is re-derived from `ctx.messages` on every call, so resume, rotation and
/// a baked skill prompt (which replaces the system message wholesale) all
/// heal on the next LLM call without this middleware knowing.
pub struct LanguageAnchorMiddleware;

#[async_trait]
impl Middleware for LanguageAnchorMiddleware {
    async fn on_pre_llm(&self, ctx: &mut PreLlmCtx) -> AgentResult<()> {
        let detection = detect_from_messages(&ctx.messages);
        if detection == Detection::Unknown {
            return Ok(());
        }
        match ctx.messages.first_mut() {
            Some(ChatMessage::System { content, .. }) => {
                *content = refresh_language_section(content, detection);
            }
            // Skipping silently would hide a wiring error where the anchor
            // never reaches the model — at least leave a log (the skills
            // catalog refresh has the same contract).
            _ => tracing::warn!(
                "reply-language anchor skipped: first message is not System \
                 (the detection ran but no anchor can be written)"
            ),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::SessionId;

    // ── detect: script precedence ──

    #[test]
    fn chinese_sentence_is_named() {
        assert_eq!(detect("帮我看看这个 bug"), Detection::Named(Lang::Zh));
        assert_eq!(detect("这个功能有问题"), Detection::Named(Lang::Zh));
    }

    #[test]
    fn short_chinese_is_enough() {
        // The shortest string a person would actually send — two han.
        assert_eq!(detect("这个bug"), Detection::Named(Lang::Zh));
        assert_eq!(detect("好吗"), Detection::Named(Lang::Zh));
    }

    #[test]
    fn one_kana_in_chinese_slang_stays_chinese() {
        // `bugの修复` — one kana is Chinese internet usage, not Japanese.
        assert_eq!(detect("帮我修一下bugの修复"), Detection::Named(Lang::Zh));
    }

    #[test]
    fn japanese_needs_two_kana() {
        assert_eq!(detect("このバグを直して"), Detection::Named(Lang::Ja));
        assert_eq!(detect("これはテストです"), Detection::Named(Lang::Ja));
    }

    #[test]
    fn korean_is_decisive() {
        assert_eq!(detect("이 버그를 고쳐줘"), Detection::Named(Lang::Ko));
    }

    #[test]
    fn english_wins_the_vote() {
        assert_eq!(
            detect("Can you check the parser output for me"),
            Detection::Named(Lang::En)
        );
    }

    #[test]
    fn latin_single_hit_mirrors() {
        // One hit is not a mandate: hand the call to the model.
        assert_eq!(detect("fix the thing"), Detection::Mirror);
    }

    #[test]
    fn code_only_inherits() {
        assert_eq!(detect("cargo test --all"), Detection::Unknown);
        assert_eq!(detect("git status"), Detection::Unknown);
        assert_eq!(detect("cargo build && make"), Detection::Unknown);
    }

    #[test]
    fn no_letters_inherits() {
        assert_eq!(detect("!!! ??? 123"), Detection::Unknown);
        assert_eq!(detect(""), Detection::Unknown);
        assert_eq!(detect("ok"), Detection::Unknown);
        assert_eq!(detect("go"), Detection::Unknown);
    }

    #[test]
    fn russian_and_ukrainian_split() {
        assert_eq!(detect("почини этот баг"), Detection::Named(Lang::Ru));
        // Ukrainian-exclusive letters (і): not Russian, no table entry —
        // mirror instead of pinning the wrong neighbour's language.
        assert_eq!(detect("підозрюю що там є проблема"), Detection::Mirror);
    }

    #[test]
    fn untabled_scripts_mirror() {
        assert_eq!(detect("δοκίμασε αυτόν τον κώδικα"), Detection::Mirror);
        assert_eq!(detect("هذا الخطأ في الكود"), Detection::Mirror);
    }

    #[test]
    fn french_german_spanish_portuguese_votes() {
        assert_eq!(
            detect("Est-ce que vous pouvez vérifier cela"),
            Detection::Named(Lang::Fr)
        );
        assert_eq!(
            detect("Das ist nicht richtig und muss geändert werden"),
            Detection::Named(Lang::De)
        );
        assert_eq!(
            detect("El archivo está mal y hay que corregirlo"),
            Detection::Named(Lang::Es)
        );
        assert_eq!(
            detect("Não está funcionando e você precisa verificar"),
            Detection::Named(Lang::Pt)
        );
    }

    #[test]
    fn contested_latin_mirrors() {
        // 8 letters so the vote is actually reached; "que" and "por" score
        // for both Spanish and Portuguese — no strict lead, so mirror.
        assert_eq!(detect("que por la"), Detection::Mirror);
    }

    // ── detect_from_messages: real messages only, inherit backwards ──

    fn user(content: &str) -> ChatMessage {
        ChatMessage::user(content)
    }

    fn synthetic(content: &str) -> ChatMessage {
        ChatMessage::user_synthetic(content)
    }

    fn ephemeral(content: &str) -> ChatMessage {
        ChatMessage::User {
            content: content.to_string(),
            images: Vec::new(),
            ephemeral: true,
            synthetic: false,
        }
    }

    #[test]
    fn newest_real_message_speaks() {
        let messages = vec![
            ChatMessage::system("sys"),
            user("帮我修个 bug"),
            ChatMessage::Assistant {
                content: Some("on it".into()),
                reasoning_content: None,
                thinking_signature: None,
                tool_calls: None,
            },
            user("继续"),
        ];
        assert_eq!(detect_from_messages(&messages), Detection::Named(Lang::Zh));
    }

    #[test]
    fn undetectable_message_inherits_the_last_language() {
        let messages = vec![
            ChatMessage::system("sys"),
            user("帮我看看这个"),
            user("cargo test"),
        ];
        // "cargo test" says nothing — the Chinese before it still speaks.
        assert_eq!(detect_from_messages(&messages), Detection::Named(Lang::Zh));
    }

    #[test]
    fn synthetic_and_ephemeral_never_speak() {
        // The rotation seed and a skill body are English machine words; the
        // real Chinese message behind them is what the user actually said.
        let messages = vec![
            ChatMessage::system("sys"),
            user("修复这个崩溃"),
            synthetic("Continue the task from the previous window."),
            ephemeral("Use the spec skill to plan this work."),
        ];
        assert_eq!(detect_from_messages(&messages), Detection::Named(Lang::Zh));
    }

    #[test]
    fn nothing_real_means_unknown() {
        let messages = vec![
            ChatMessage::system("sys"),
            synthetic("Background tasks finished."),
            ephemeral("Run the qa skill."),
        ];
        assert_eq!(detect_from_messages(&messages), Detection::Unknown);
    }

    // ── refresh_language_section: region surgery ──

    const BASE: &str = "You are phimint, a coding agent in a terminal.\n";

    #[test]
    fn appends_once_when_absent() {
        let out = refresh_language_section(BASE, Detection::Named(Lang::Zh));
        assert!(out.starts_with(BASE));
        assert!(out.contains("<reply-language>"));
        assert!(out.contains("Chinese (中文)"));
        // Exactly one region.
        assert_eq!(out.matches("<reply-language>").count(), 1);
    }

    #[test]
    fn same_detection_is_byte_identical() {
        let first = refresh_language_section(BASE, Detection::Named(Lang::Zh));
        let second = refresh_language_section(&first, Detection::Named(Lang::Zh));
        assert_eq!(first, second, "steady state must be a fixed point");
    }

    #[test]
    fn language_change_replaces_in_place() {
        let zh = refresh_language_section(BASE, Detection::Named(Lang::Zh));
        let en = refresh_language_section(&zh, Detection::Named(Lang::En));
        assert!(!en.contains("中文"), "old language must be gone");
        assert!(en.contains("English"));
        assert_eq!(en.matches("<reply-language>").count(), 1);
        // The base prompt is untouched around the region.
        assert!(en.starts_with(BASE));
    }

    #[test]
    fn unknown_detection_changes_nothing() {
        let zh = refresh_language_section(BASE, Detection::Named(Lang::Zh));
        let unchanged = refresh_language_section(&zh, Detection::Unknown);
        assert_eq!(zh, unchanged, "no letters → keep the anchor that spoke");
    }

    #[test]
    fn content_after_the_region_survives() {
        // Refresh appends when absent, so build the region first, then put a
        // trailing section behind it — as a later mutator would. Every
        // refresh must preserve that trailing content byte-for-byte.
        let zh = refresh_language_section(BASE, Detection::Named(Lang::Zh));
        let with_trailing = format!("{zh}\n<trailer>keep me</trailer>");
        let en = refresh_language_section(&with_trailing, Detection::Named(Lang::En));
        assert!(en.ends_with("\n<trailer>keep me</trailer>"));
        assert!(en.contains("English"));
        assert!(!en.contains("中文"));
    }

    #[test]
    fn mirror_region_wording() {
        let out = refresh_language_section(BASE, Detection::Mirror);
        assert!(out.contains(
            "Write everything you show the user in the same language the user writes in."
        ));
        assert!(!out.contains("The user writes in"));
    }

    // ── middleware: the wiring seam ──

    fn pre_llm_ctx(messages: Vec<ChatMessage>) -> PreLlmCtx {
        PreLlmCtx {
            session_id: SessionId {
                id: 1,
                external_id: None,
            },
            messages,
            tools: vec![],
            emit_fn: None,
            turn_count: 1,
            max_turns: 10,
        }
    }

    #[tokio::test]
    async fn middleware_anchors_a_chinese_conversation() {
        let mw = LanguageAnchorMiddleware;
        let mut ctx = pre_llm_ctx(vec![ChatMessage::system(BASE), user("帮我修个 bug")]);
        mw.on_pre_llm(&mut ctx).await.unwrap();
        match &ctx.messages[0] {
            ChatMessage::System { content, .. } => {
                assert!(content.contains("Chinese (中文)"));
                assert!(content.starts_with(BASE));
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn middleware_is_idempotent_across_calls() {
        let mw = LanguageAnchorMiddleware;
        let mut ctx = pre_llm_ctx(vec![ChatMessage::system(BASE), user("帮我修个 bug")]);
        mw.on_pre_llm(&mut ctx).await.unwrap();
        let after_first = match &ctx.messages[0] {
            ChatMessage::System { content, .. } => content.clone(),
            other => panic!("expected System, got {other:?}"),
        };

        // The next LLM call re-clones messages from the session — the same
        // prompt goes through the middleware again.
        let mut ctx2 = pre_llm_ctx(vec![
            ChatMessage::system(after_first.clone()),
            user("帮我修个 bug"),
        ]);
        mw.on_pre_llm(&mut ctx2).await.unwrap();
        match &ctx2.messages[0] {
            ChatMessage::System { content, .. } => {
                assert_eq!(
                    content, &after_first,
                    "repeated refresh must be a fixed point (prompt-cache safety)"
                );
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn middleware_leaves_prompt_alone_without_a_detection() {
        let mw = LanguageAnchorMiddleware;
        let mut ctx = pre_llm_ctx(vec![ChatMessage::system(BASE), user("cargo test")]);
        mw.on_pre_llm(&mut ctx).await.unwrap();
        match &ctx.messages[0] {
            ChatMessage::System { content, .. } => {
                assert_eq!(content, BASE, "no detection → byte-identical prompt");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_system_first_message_does_not_panic() {
        let mw = LanguageAnchorMiddleware;
        let mut ctx = pre_llm_ctx(vec![user("帮我修个 bug")]);
        mw.on_pre_llm(&mut ctx).await.unwrap();
        match &ctx.messages[0] {
            ChatMessage::User { content, .. } => assert_eq!(content, "帮我修个 bug"),
            other => panic!("expected User, got {other:?}"),
        }
    }
}
