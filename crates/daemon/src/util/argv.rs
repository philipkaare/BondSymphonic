//! Reading a command line out of a setting, the way a shell would.
//!
//! Three of the daemon's settings name a program to run rather than a single
//! path: `BS_CLAUDE_BIN`, `BS_GH_BIN` and the `command` on `pty.open`. All three
//! want the same two things out of the text -- the words a shell would have
//! split it into, and a refusal when there is no program in it at all -- and all
//! three used to spell that out for themselves, with a third of the refusals
//! missing at each site.
//!
//! Split, never run through a shell: what comes back is an argv handed straight
//! to `exec`, so nothing the text contains is interpreted as shell syntax.

/// The words of `s`, as a shell would split them.
///
/// `what` names the setting the text came from and leads the error message, so
/// the person reading it knows which knob to correct. The error is a plain
/// `String` on purpose: each caller turns it into the error kind its own
/// request answers with -- `InvalidParams` for a command the client sent,
/// `Internal` for a hook only the daemon's own environment can set.
///
/// Text with no words in it is a refusal rather than an empty argv. A setting
/// that is present but names nothing is a misconfiguration, and the one thing
/// that must never happen is for it to fall quietly back to the real program
/// the stand-in was there to replace.
pub fn split(s: &str, what: &str) -> Result<Vec<String>, String> {
    let argv = shell_words::split(s).map_err(|e| format!("{what}: {e}"))?;
    if argv.is_empty() {
        return Err(format!("{what}: no program to run"));
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_split_the_way_a_shell_splits_them() {
        assert_eq!(
            split("\"python\" \"/tmp/gh stub.py\"", "BS_GH_BIN").unwrap(),
            vec!["python".to_string(), "/tmp/gh stub.py".to_string()]
        );
        assert_eq!(
            split("npm run dev", "command").unwrap(),
            vec!["npm".to_string(), "run".to_string(), "dev".to_string()]
        );
    }

    /// Present but naming nothing is the case that must not fall back to the
    /// real program: a test that sets the hook to whitespace has said "never
    /// run the real one", and an empty argv would run it.
    #[test]
    fn text_with_no_words_in_it_is_refused_and_the_setting_is_named() {
        for empty in ["", "   ", "\t\n"] {
            let e = split(empty, "BS_CLAUDE_BIN").unwrap_err();
            assert_eq!(e, "BS_CLAUDE_BIN: no program to run", "{empty:?}");
        }
    }

    #[test]
    fn an_unbalanced_quote_is_refused_and_the_setting_is_named() {
        let e = split("\"unclosed", "command").unwrap_err();
        assert!(e.starts_with("command: "), "{e}");
    }
}
