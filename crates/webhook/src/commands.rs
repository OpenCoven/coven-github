//! Maintainer command protocol (issue #13).
//!
//! A mention only carries authority when it is the comment's first token
//! (command position) followed by a known verb. Mentions elsewhere in the text
//! are casual and trigger nothing; an unknown verb in command position earns a
//! clarification reply rather than work.

/// A typed maintainer intent parsed from a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run a hosted review of the pull request.
    Review,
    /// Fix the issue, or address feedback on the PR. `args` is the free text
    /// after the verb (e.g. `@cody fix: the lint is still failing`).
    Fix { args: String },
    /// Re-review the PR with greater depth.
    Deepen,
    /// Re-run the default action for this surface.
    Retry,
    /// Cancel queued work for this surface.
    Cancel,
    /// Run the Branch Gardener for this repository.
    Garden,
    /// Memory governance intents — parsed and acknowledged; persistence lands
    /// with the hosted memory contract (issue #6).
    Remember { note: String },
    Forget { note: String },
    /// Report current task state for this surface.
    Status,
    /// Verb in command position that matches nothing known.
    Unknown { verb: String },
}

/// How a comment relates to a familiar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionKind {
    /// Command-position mention with a verb.
    Command(Command),
    /// The familiar is mentioned, but not as a command — ignored.
    Casual,
    /// No mention of this familiar at all.
    None,
}

/// Parses a comment body against a familiar's bot username
/// (e.g. `coven-cody[bot]`, mentioned as `@coven-cody`).
pub fn parse_mention(body: &str, bot_username: &str) -> MentionKind {
    let handle = bot_username.trim_end_matches("[bot]");
    if handle.is_empty() {
        return MentionKind::None;
    }
    let needle = format!("@{handle}");

    if let Some(rest) = strip_command_mention(body.trim_start(), &needle) {
        let mut words = rest.trim_start().splitn(2, char::is_whitespace);
        let verb_raw = words.next().unwrap_or("");
        let args = words.next().unwrap_or("").trim().to_string();
        // Tolerate the documented `fix:` colon form on any verb.
        let verb = verb_raw.trim_end_matches(':').to_ascii_lowercase();
        let command = match verb.as_str() {
            "review" => Command::Review,
            "fix" => Command::Fix { args },
            "deepen" => Command::Deepen,
            "retry" => Command::Retry,
            "cancel" => Command::Cancel,
            "garden" => Command::Garden,
            "remember" => Command::Remember { note: args },
            "forget" => Command::Forget { note: args },
            "status" => Command::Status,
            // A bare mention with no verb carries no intent.
            "" => return MentionKind::Casual,
            other => Command::Unknown {
                verb: other.to_string(),
            },
        };
        return MentionKind::Command(command);
    }

    if mentions(body, bot_username) {
        MentionKind::Casual
    } else {
        MentionKind::None
    }
}

/// The commands a clarification reply should list.
pub const COMMAND_LIST: &str =
    "`review`, `fix`, `deepen`, `retry`, `cancel`, `garden`, `remember`, `forget`, `status`";

/// If `text` starts with the mention as a whole token, returns the remainder.
fn strip_command_mention<'a>(text: &'a str, needle: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(needle)?;
    match rest.chars().next() {
        // `@codyx`, `@cody-2`, `@cody/team` are not mentions of `@cody`.
        Some(c) if c.is_alphanumeric() || c == '-' || c == '_' || c == '/' => None,
        _ => Some(rest),
    }
}

/// Returns true if `body` mentions the familiar's `@handle` as a whole token.
///
/// `bot_username` is the GitHub App bot login (e.g. `coven-cody[bot]`); the
/// `[bot]` suffix is dropped since mentions are written `@coven-cody`. Matching
/// is boundary-aware: `@cody` inside `@codyx`, `@cody-2`, or `email@cody` does
/// not count, and `@coven-cody/team` (a team mention) is not a bot mention.
pub fn mentions(body: &str, bot_username: &str) -> bool {
    let handle = bot_username.trim_end_matches("[bot]");
    if handle.is_empty() {
        return false;
    }
    let needle = format!("@{handle}");
    let mut offset = 0;
    while let Some(pos) = body[offset..].find(&needle) {
        let start = offset + pos;
        let end = start + needle.len();
        let before = body[..start].chars().next_back();
        let after = body[end..].chars().next();
        // The character before `@` must be a separator (or start of string),
        // and the character after the handle must not continue an identifier.
        let boundary_before = before.is_none_or(|c| !c.is_alphanumeric() && c != '@');
        let boundary_after =
            after.is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '/'));
        if boundary_before && boundary_after {
            return true;
        }
        offset = start + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "coven-cody[bot]";

    #[test]
    fn every_verb_parses_in_command_position() {
        let cases: Vec<(&str, Command)> = vec![
            ("@coven-cody review", Command::Review),
            (
                "@coven-cody fix the flaky auth test",
                Command::Fix {
                    args: "the flaky auth test".to_string(),
                },
            ),
            (
                "@coven-cody fix: the lint is still failing",
                Command::Fix {
                    args: "the lint is still failing".to_string(),
                },
            ),
            ("@coven-cody deepen", Command::Deepen),
            ("@coven-cody retry", Command::Retry),
            ("@coven-cody cancel", Command::Cancel),
            ("@coven-cody garden", Command::Garden),
            ("@coven-cody garden now", Command::Garden),
            (
                "@coven-cody remember we ship Fridays",
                Command::Remember {
                    note: "we ship Fridays".to_string(),
                },
            ),
            (
                "@coven-cody forget the Friday rule",
                Command::Forget {
                    note: "the Friday rule".to_string(),
                },
            ),
            ("@coven-cody status", Command::Status),
            ("  @coven-cody REVIEW  ", Command::Review),
        ];
        for (body, expected) in cases {
            assert_eq!(
                parse_mention(body, BOT),
                MentionKind::Command(expected),
                "body: {body:?}"
            );
        }
    }

    #[test]
    fn unknown_verb_in_command_position_is_a_typed_unknown() {
        assert_eq!(
            parse_mention("@coven-cody frobnicate the flux", BOT),
            MentionKind::Command(Command::Unknown {
                verb: "frobnicate".to_string()
            })
        );
    }

    #[test]
    fn clarification_command_list_mentions_garden() {
        assert!(COMMAND_LIST.contains("`garden`"));
    }

    #[test]
    fn casual_mentions_carry_no_command() {
        // Mid-sentence mention.
        assert_eq!(
            parse_mention("thanks @coven-cody, great work on this", BOT),
            MentionKind::Casual
        );
        // Conversational lead that is not a verb reads as Unknown (it sits in
        // command position), NOT Casual — the reply will clarify.
        assert!(matches!(
            parse_mention("@coven-cody can you take a look?", BOT),
            MentionKind::Command(Command::Unknown { .. })
        ));
        // Bare mention with no verb at all.
        assert_eq!(parse_mention("@coven-cody", BOT), MentionKind::Casual);
    }

    #[test]
    fn non_mentions_are_none() {
        assert_eq!(parse_mention("no bots here", BOT), MentionKind::None);
        assert_eq!(parse_mention("ping @coven-codyx review", BOT), MentionKind::None);
        assert_eq!(
            parse_mention("cc @coven-cody/maintainers review", BOT),
            MentionKind::None
        );
        assert_eq!(
            parse_mention("mail user@coven-cody.example", BOT),
            MentionKind::None
        );
    }

    #[test]
    fn mention_matching_is_boundary_aware() {
        assert!(mentions("hey @coven-cody can you help", BOT));
        assert!(!mentions("ping @coven-codyx instead", BOT));
        assert!(!mentions("ping @coven-cody-2 instead", BOT));
        assert!(!mentions("cc @coven-cody/maintainers", BOT));
        assert!(!mentions("mail user@coven-cody.example", BOT));
        assert!(mentions("over to you @coven-cody", BOT));
    }
}
