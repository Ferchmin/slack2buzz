//! Slack mrkdwn → Markdown normalisation.
//!
//! This module is the fidelity-critical part of `parse`. Everything else
//! reshuffles JSON; this rewrites human text, and every bug here is a
//! permanently wrong archive. It is therefore pure (no I/O, no clock) and
//! carries the bulk of the crate's unit tests.
//!
//! # Two ordering rules that are easy to get backwards
//!
//! **Refs before entities.** Slack escapes exactly three characters in message
//! text — `&` → `&amp;`, `<` → `&lt;`, `>` → `&gt;` — and it does *not* escape
//! the angle brackets that delimit its own refs (`<@U1>`, `<http://x|y>`). So
//! on the wire, an unescaped `<` is always a ref delimiter and `&lt;` is always
//! literal user text. Parsing refs first, while that distinction still exists,
//! is what makes `a &lt;@U1&gt; b` come out as the literal text `a <@U1> b`
//! instead of a bogus mention. Decode entities first and the distinction is
//! gone for good.
//!
//! **Code before everything.** Text inside a fence or an inline span must not
//! have its emphasis or refs rewritten — a code sample containing `*ptr` or
//! `<@U1>` means those characters literally. Entity decoding *does* still apply
//! inside code, because Slack escapes there too.

use std::collections::HashMap;

/// Resolves Slack ids to the names used in normalised text.
///
/// Both maps are consulted only for refs that omit their own label. Slack
/// usually includes a label (`<#C1|general>`), but older exports often do not.
#[derive(Debug, Default)]
pub struct Resolver {
    /// Slack user id → display name, without the `@`.
    pub users: HashMap<String, String>,
    /// Slack channel id → channel name, without the `#`.
    pub channels: HashMap<String, String>,
}

impl Resolver {
    fn user(&self, id: &str) -> String {
        self.users
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    fn channel(&self, id: &str) -> String {
        self.channels
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }
}

/// Result of normalising one message's text.
#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    pub text: String,
    /// Slack user ids mentioned, deduplicated, in order of first appearance.
    pub mentions: Vec<String>,
}

/// Normalise one Slack message body.
pub fn normalize(input: &str, resolver: &Resolver) -> Normalized {
    let mut mentions = Vec::new();
    let mut out = String::with_capacity(input.len());

    for segment in segment_code(input) {
        match segment {
            Segment::Code(raw) => {
                // Code keeps its literal shape; only entities are decoded.
                out.push_str(&decode_entities(raw));
            }
            Segment::Text(raw) => {
                let refs_expanded = expand_refs(raw, resolver, &mut mentions);
                let emphasised = convert_emphasis(&refs_expanded);
                out.push_str(&decode_entities(&emphasised));
            }
        }
    }

    Normalized {
        text: out,
        mentions,
    }
}

// ── code segmentation ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Segment<'a> {
    /// Includes its delimiters, so reassembly is lossless.
    Code(&'a str),
    Text(&'a str),
}

/// Split into alternating code and non-code runs.
///
/// Triple backticks win over single, and an unterminated delimiter is treated
/// as literal text rather than swallowing the rest of the message — a stray
/// backtick is far more common than a genuinely unclosed fence, and silently
/// eating the remainder of someone's message is the worse failure.
fn segment_code(input: &str) -> Vec<Segment<'_>> {
    let bytes = input.as_bytes();
    let mut segments = Vec::new();
    let mut text_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }

        let fence = if input[i..].starts_with("```") { 3 } else { 1 };
        let delim = &input[i..i + fence];

        let Some(rel_end) = input[i + fence..].find(delim) else {
            // Unterminated — leave it as text.
            i += fence;
            continue;
        };
        let end = i + fence + rel_end + fence;

        if text_start < i {
            segments.push(Segment::Text(&input[text_start..i]));
        }
        segments.push(Segment::Code(&input[i..end]));
        text_start = end;
        i = end;
    }

    if text_start < input.len() {
        segments.push(Segment::Text(&input[text_start..]));
    }
    segments
}

// ── refs ─────────────────────────────────────────────────────────────────────

/// Rewrite every `<...>` ref. Refs never nest, so a flat scan for the next
/// `>` is exact.
fn expand_refs(input: &str, resolver: &Resolver, mentions: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];

        let Some(close) = after.find('>') else {
            // No closing bracket: a literal, unescaped '<'. Keep it.
            out.push_str(&rest[open..]);
            return out;
        };

        out.push_str(&expand_one_ref(&after[..close], resolver, mentions));
        rest = &after[close + 1..];
    }

    out.push_str(rest);
    out
}

/// Expand the inside of a single ref (delimiters already stripped).
fn expand_one_ref(body: &str, resolver: &Resolver, mentions: &mut Vec<String>) -> String {
    let (target, label) = match body.find('|') {
        Some(p) => (&body[..p], Some(&body[p + 1..])),
        None => (body, None),
    };

    match target.chars().next() {
        // User mention: <@U123> or <@U123|label>
        Some('@') => {
            let id = &target[1..];
            if !id.is_empty() && !mentions.iter().any(|m| m == id) {
                mentions.push(id.to_string());
            }
            let name = match label {
                Some(l) if !l.is_empty() => l.to_string(),
                _ => resolver.user(id),
            };
            format!("@{name}")
        }
        // Channel ref: <#C123|name> or <#C123>
        Some('#') => {
            let id = &target[1..];
            let name = match label {
                Some(l) if !l.is_empty() => l.to_string(),
                _ => resolver.channel(id),
            };
            format!("#{name}")
        }
        // Special: <!here>, <!channel>, <!everyone>, <!subteam^S1|@grp>,
        // <!date^...|fallback>
        Some('!') => expand_special(&target[1..], label),
        // Otherwise a URL.
        _ => expand_link(target, label),
    }
}

fn expand_special(target: &str, label: Option<&str>) -> String {
    // `subteam^ID` and `date^...` carry their human form in the label; the
    // bare keywords (`here`, `channel`, `everyone`) are their own text.
    if let Some(label) = label {
        if !label.is_empty() {
            // Slack writes user-group labels already prefixed with '@'.
            return label.to_string();
        }
    }
    match target.split('^').next().unwrap_or(target) {
        "here" => "@here".to_string(),
        "channel" => "@channel".to_string(),
        "everyone" => "@everyone".to_string(),
        other => format!("@{other}"),
    }
}

fn expand_link(target: &str, label: Option<&str>) -> String {
    // `mailto:` and `tel:` read better bare than as a Markdown link.
    let bare = target
        .strip_prefix("mailto:")
        .or_else(|| target.strip_prefix("tel:"));

    match (bare, label) {
        // Slack duplicates the address into the label for mailto refs; a
        // Markdown link to the same string is just noise.
        (Some(addr), Some(l)) if l == addr || l.is_empty() => addr.to_string(),
        (Some(addr), Some(l)) => format!("[{l}]({addr})"),
        (Some(addr), None) => addr.to_string(),
        (None, Some(l)) if l.is_empty() || l == target => target.to_string(),
        (None, Some(l)) => format!("[{l}]({target})"),
        (None, None) => target.to_string(),
    }
}

// ── emphasis ─────────────────────────────────────────────────────────────────

/// `*bold*` → `**bold**` and `~strike~` → `~~strike~~`.
///
/// `_italic_` is already Markdown and is left untouched. A delimiter only
/// counts when it hugs non-whitespace on the inside and the span holds no
/// newline, which is close enough to Slack's own rule to avoid mangling
/// arithmetic (`2 * 3 * 4`) and snake_case identifiers.
fn convert_emphasis(input: &str) -> String {
    let with_bold = convert_delimiter(input, '*', "**");
    convert_delimiter(&with_bold, '~', "~~")
}

fn convert_delimiter(input: &str, delim: char, replacement: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != delim {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        // Opening delimiter must be followed by non-space.
        let opens = chars
            .get(i + 1)
            .is_some_and(|c| !c.is_whitespace() && *c != delim);
        if !opens {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        // Find a closing delimiter preceded by non-space, before any newline.
        let mut j = i + 1;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '\n' {
                break;
            }
            if chars[j] == delim && chars[j - 1] != delim && !chars[j - 1].is_whitespace() {
                close = Some(j);
                break;
            }
            j += 1;
        }

        match close {
            Some(end) => {
                out.push_str(replacement);
                out.extend(&chars[i + 1..end]);
                out.push_str(replacement);
                i = end + 1;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

// ── entities ─────────────────────────────────────────────────────────────────

/// Decode the only three entities Slack emits.
///
/// `&amp;` is decoded last by construction (single left-to-right pass), so
/// `&amp;lt;` correctly yields the literal text `&lt;` rather than `<`.
fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(r) = tail.strip_prefix("&amp;") {
            out.push('&');
            rest = r;
        } else if let Some(r) = tail.strip_prefix("&lt;") {
            out.push('<');
            rest = r;
        } else if let Some(r) = tail.strip_prefix("&gt;") {
            out.push('>');
            rest = r;
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    // A panic IS the failure report in a test; Buzz's CONTRIBUTING allows it.
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn resolver() -> Resolver {
        let mut users = HashMap::new();
        users.insert("U024BE7LH".to_string(), "pawel".to_string());
        users.insert("U0ALICE".to_string(), "alice".to_string());
        let mut channels = HashMap::new();
        channels.insert("C0GENERAL".to_string(), "general".to_string());
        Resolver { users, channels }
    }

    fn norm(input: &str) -> Normalized {
        normalize(input, &resolver())
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(norm("hello world").text, "hello world");
    }

    #[test]
    fn user_mention_resolves_and_is_recorded() {
        let n = norm("hi <@U024BE7LH>!");
        assert_eq!(n.text, "hi @pawel!");
        assert_eq!(n.mentions, vec!["U024BE7LH"]);
    }

    #[test]
    fn unknown_user_falls_back_to_id_and_still_records_the_mention() {
        let n = norm("hi <@UNOBODY>");
        assert_eq!(n.text, "hi @UNOBODY");
        assert_eq!(n.mentions, vec!["UNOBODY"]);
    }

    #[test]
    fn explicit_mention_label_wins_over_the_resolver() {
        let n = norm("<@U024BE7LH|paweł>");
        assert_eq!(n.text, "@paweł");
        assert_eq!(n.mentions, vec!["U024BE7LH"]);
    }

    #[test]
    fn mentions_are_deduplicated_in_first_appearance_order() {
        let n = norm("<@U0ALICE> <@U024BE7LH> <@U0ALICE>");
        assert_eq!(n.mentions, vec!["U0ALICE", "U024BE7LH"]);
    }

    #[test]
    fn channel_refs_use_label_then_resolver_then_id() {
        assert_eq!(norm("<#C0GENERAL|general>").text, "#general");
        assert_eq!(norm("<#C0GENERAL>").text, "#general");
        assert_eq!(norm("<#C0UNKNOWN>").text, "#C0UNKNOWN");
    }

    #[test]
    fn broadcast_keywords_become_plain_at_mentions() {
        assert_eq!(norm("<!here>").text, "@here");
        assert_eq!(norm("<!channel>").text, "@channel");
        assert_eq!(norm("<!everyone>").text, "@everyone");
    }

    #[test]
    fn user_group_ref_uses_its_label() {
        assert_eq!(norm("<!subteam^S0ENG|@eng>").text, "@eng");
    }

    #[test]
    fn links_become_markdown_only_when_labelled_differently() {
        assert_eq!(
            norm("<https://x.test|the site>").text,
            "[the site](https://x.test)"
        );
        assert_eq!(norm("<https://x.test>").text, "https://x.test");
        assert_eq!(
            norm("<https://x.test|https://x.test>").text,
            "https://x.test"
        );
    }

    #[test]
    fn mailto_refs_are_rendered_bare() {
        assert_eq!(norm("<mailto:a@b.test|a@b.test>").text, "a@b.test");
        assert_eq!(norm("<mailto:a@b.test>").text, "a@b.test");
    }

    #[test]
    fn bold_and_strike_are_widened_italic_is_left_alone() {
        assert_eq!(norm("*bold*").text, "**bold**");
        assert_eq!(norm("~gone~").text, "~~gone~~");
        assert_eq!(norm("_italic_").text, "_italic_");
    }

    #[test]
    fn arithmetic_and_identifiers_are_not_mistaken_for_emphasis() {
        assert_eq!(norm("2 * 3 * 4").text, "2 * 3 * 4");
        assert_eq!(norm("snake_case_name").text, "snake_case_name");
        assert_eq!(norm("a * b").text, "a * b");
    }

    #[test]
    fn emphasis_does_not_span_newlines() {
        assert_eq!(norm("*not\nbold*").text, "*not\nbold*");
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(norm("a &amp; b").text, "a & b");
        assert_eq!(norm("&lt;tag&gt;").text, "<tag>");
    }

    /// The ordering rule from the module docs: an escaped angle bracket is
    /// literal text and must not be parsed as a ref.
    #[test]
    fn escaped_angle_brackets_do_not_become_refs() {
        let n = norm("literally &lt;@U024BE7LH&gt; ok");
        assert_eq!(n.text, "literally <@U024BE7LH> ok");
        assert!(n.mentions.is_empty(), "no real mention was present");
    }

    #[test]
    fn double_escaped_ampersand_survives_one_decode_only() {
        assert_eq!(norm("&amp;lt;").text, "&lt;");
    }

    #[test]
    fn inline_code_is_not_rewritten() {
        let n = norm("run `*not bold*` please");
        assert_eq!(n.text, "run `*not bold*` please");
    }

    #[test]
    fn fenced_code_is_not_rewritten_but_is_unescaped() {
        let n = norm("```\nif (a &lt; b) *x*;\n```");
        assert_eq!(n.text, "```\nif (a < b) *x*;\n```");
        assert!(n.mentions.is_empty());
    }

    #[test]
    fn refs_inside_code_are_left_literal_and_not_counted_as_mentions() {
        let n = norm("`<@U024BE7LH>`");
        assert_eq!(n.text, "`<@U024BE7LH>`");
        assert!(n.mentions.is_empty());
    }

    #[test]
    fn text_around_code_is_still_normalised() {
        let n = norm("*before* `raw *x*` *after*");
        assert_eq!(n.text, "**before** `raw *x*` **after**");
    }

    #[test]
    fn unterminated_backtick_is_literal_and_does_not_eat_the_message() {
        let n = norm("a ` b *bold*");
        assert_eq!(n.text, "a ` b **bold**");
    }

    #[test]
    fn unterminated_angle_bracket_is_kept() {
        assert_eq!(norm("a < b").text, "a < b");
    }

    #[test]
    fn blockquote_marker_decodes_to_markdown_quote() {
        assert_eq!(norm("&gt; quoted").text, "> quoted");
    }
}
