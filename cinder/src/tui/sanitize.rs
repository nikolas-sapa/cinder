//! Peer-derived text must never reach the terminal raw: escape sequences are
//! an injection vector (THREAT-MODEL §3.4). Strip control chars and consume
//! whole CSI/OSC/DCS-family sequences before render.

fn is_bidi(c: char) -> bool {
    matches!(c,
        '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}'
        | '\u{200E}' | '\u{200F}'
        | '\u{061C}')
}

fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200B}' | '\u{FEFF}' | '\u{2060}' | '\u{180E}')
}

fn is_c1(c: char) -> bool {
    ('\u{80}'..='\u{9F}').contains(&c)
}

fn is_c1_csi(c: char) -> bool {
    c == '\u{9b}'
}

fn is_c1_string(c: char) -> bool {
    matches!(c, '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}')
}

fn skip_csi(it: &mut std::iter::Peekable<std::str::Chars>) {
    for c in it.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

fn skip_string_seq(it: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(c) = it.next() {
        match c {
            '\u{7}' | '\u{9c}' => break,
            '\u{1b}' => {
                if it.peek() == Some(&'\\') {
                    it.next();
                }
                break;
            }
            _ => {}
        }
    }
}

enum EscSeq {
    Csi,
    OscLike,
    CharSet,
    Fe,
}

fn classify_esc(next: Option<char>) -> Option<EscSeq> {
    match next? {
        '[' => Some(EscSeq::Csi),
        ']' | 'P' | 'X' | '^' | '_' => Some(EscSeq::OscLike),
        '(' | ')' | '*' | '+' => Some(EscSeq::CharSet),
        '0'..='~' => Some(EscSeq::Fe),
        _ => None,
    }
}

pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match classify_esc(it.peek().copied()) {
                Some(EscSeq::Csi) => {
                    it.next();
                    skip_csi(&mut it);
                }
                Some(EscSeq::OscLike) => {
                    it.next();
                    skip_string_seq(&mut it);
                }
                Some(EscSeq::CharSet) => {
                    it.next();
                    it.next();
                }
                Some(EscSeq::Fe) => {
                    it.next();
                }
                None => {}
            },
            c if is_c1_csi(c) => skip_csi(&mut it),
            c if is_c1_string(c) => skip_string_seq(&mut it),
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {}
            c if is_c1(c) || is_bidi(c) || is_zero_width(c) => {}
            '\u{2028}' | '\u{2029}' => out.push('\n'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_hyperlink_stripped() {
        assert_eq!(sanitize("\x1b]8;;http://evil\x07link"), "link");
        assert_eq!(
            sanitize("\x1b]8;;http://evil\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
    }

    #[test]
    fn csi_stripped() {
        assert_eq!(sanitize("\x1b[2Jabc"), "abc");
        assert_eq!(sanitize("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(sanitize("a\x1b[1;1Hb"), "ab");
        assert_eq!(sanitize("\x1b[K\x1b[6n"), "");
    }

    #[test]
    fn dcs_and_charset_stripped() {
        assert_eq!(sanitize("\x1bPq$!stuff\x1b\\ok"), "ok");
        assert_eq!(sanitize("\x1b(Bascii"), "ascii");
        assert_eq!(sanitize("x\x1bcy"), "xy");
    }

    #[test]
    fn bidi_and_zero_width_stripped() {
        assert_eq!(sanitize("\u{202e}spoof\u{202c}"), "spoof");
        assert_eq!(sanitize("a\u{2066}b\u{2069}c"), "abc");
        assert_eq!(sanitize("x\u{200b}y\u{feff}z\u{2060}"), "xyz");
        assert_eq!(sanitize("\u{061c}\u{200f}txt"), "txt");
    }

    #[test]
    fn c0_c1_stripped() {
        assert_eq!(sanitize("a\x00b\x07c\x1fd"), "abcd");
        assert_eq!(sanitize("a\x7fb\u{85}c"), "abc");
        assert_eq!(sanitize("x\u{9b}31my"), "xy");
        assert_eq!(sanitize("x\u{9d}title\u{9c}y"), "xy");
    }

    #[test]
    fn unterminated_sequences_stripped() {
        assert_eq!(sanitize("abc\x1b[2;5"), "abc");
        assert_eq!(sanitize("abc\x1b]8;;never"), "abc");
        assert_eq!(sanitize("abc\x1b"), "abc");
    }

    #[test]
    fn normal_text_untouched() {
        assert_eq!(sanitize("hello world"), "hello world");
        assert_eq!(sanitize("emoji 🫡 works"), "emoji 🫡 works");
        assert_eq!(sanitize("line1\nline2"), "line1\nline2");
        assert_eq!(sanitize("a\tb"), "a    b");
        assert_eq!(sanitize("ümläut ñ "), "ümläut ñ ");
        assert_eq!(
            sanitize("zwj 👨\u{200d}👩\u{200d}👧 kept"),
            "zwj 👨\u{200d}👩\u{200d}👧 kept"
        );
    }
}
