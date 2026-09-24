//! Just enough POSIX shell word handling to write a hook command and to
//! recognise one again — Python's `shlex.quote` / `shlex.split`, in small.

/// Quote `s` for a POSIX shell. Safe words pass through untouched, so the
/// common `/Users/me/.local/bin/heard-hook` stays readable in settings.json.
pub fn quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b',' | b'@' | b'%'
                )
        });
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r#"'"'"'"#))
    }
}

/// Split a command line into words: whitespace separates, single quotes are
/// literal, double quotes allow `\"` `\\` `\$` `` \` `` escapes, a bare
/// backslash escapes the next character. `None` for an unterminated quote —
/// such a command is never one of ours.
pub fn split(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => {
                            let n = chars.next()?;
                            if !matches!(n, '"' | '\\' | '$' | '`' | '\n') {
                                cur.push('\\');
                            }
                            cur.push(n);
                        }
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

/// Peel leading `NAME=value` assignments off a word list, as a shell would.
pub fn strip_env_prefix(words: &[String]) -> &[String] {
    let n = words.iter().take_while(|w| is_assignment(w)).count();
    &words[n..]
}

fn is_assignment(w: &str) -> bool {
    let Some((name, _)) = w.split_once('=') else {
        return false;
    };
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_round_trips_through_split() {
        for s in [
            "/Users/me/.local/bin/heard-hook",
            "/Users/me/Library/Application Support/x",
            "it's",
            "",
            "a\"b$c",
        ] {
            assert_eq!(split(&quote(s)).unwrap(), vec![s.to_owned()], "{s:?}");
        }
        assert_eq!(quote("/a/b-c"), "/a/b-c");
    }

    #[test]
    fn split_handles_env_prefix_and_quotes() {
        let w = split(r#"PYTHONDONTWRITEBYTECODE=1 PYTHONHOME='/A B' "/x y/python" -m heard.hook claude-code"#).unwrap();
        assert_eq!(
            strip_env_prefix(&w),
            ["/x y/python", "-m", "heard.hook", "claude-code"]
        );
        assert!(split("'unterminated").is_none());
    }
}
