use crate::parse::ast::CommentOrNewline;
use bumpalo::collections::String;

/// The number of spaces to indent.
pub const INDENT: u16 = 4;

pub fn newline<'a>(buf: &mut String<'a>, indent: u16) {
    buf.push('\n');

    add_spaces(buf, indent);
}

pub fn add_spaces<'a>(buf: &mut String<'a>, spaces: u16) {
    for _ in 0..spaces {
        buf.push(' ');
    }
}

pub fn fmt_spaces<'a, I>(buf: &mut String<'a>, spaces: I, indent: u16)
where
    I: Iterator<Item = &'a CommentOrNewline<'a>>,
{
    use self::CommentOrNewline::*;

    // Only ever print two newlines back to back.
    // (Two newlines renders as one blank line.)
    let mut consecutive_newlines = 0;
    let mut iter = spaces.peekable();

    let mut encountered_comment = false;

    while let Some(space) = iter.next() {
        match space {
            Newline => {
                if !encountered_comment && (consecutive_newlines < 2) {
                    if iter.peek() == Some(&&Newline) {
                        buf.push('\n');
                    } else {
                        newline(buf, indent);
                    }

                    // Don't bother incrementing it if we're already over the limit.
                    // There's no upside, and it might eventually overflow,
                    consecutive_newlines += 1;
                }
            }
            LineComment(comment) => {
                fmt_comment(buf, comment, indent);

                encountered_comment = true;
            }
        }
    }
}

/// Similar to fmt_comments_only, but does not finish with a newline()
/// Used to format when and if statement conditions
pub fn fmt_condition_spaces<'a, I>(buf: &mut String<'a>, spaces: I, indent: u16)
where
    I: Iterator<Item = &'a CommentOrNewline<'a>>,
{
    use self::CommentOrNewline::*;

    let mut iter = spaces.peekable();

    while let Some(space) = iter.next() {
        match space {
            Newline => {}
            LineComment(comment) => {
                buf.push('#');
                buf.push_str(comment);
            }
        }
        match iter.peek() {
            None => {}
            Some(next_space) => match next_space {
                Newline => {}
                LineComment(_) => {
                    newline(buf, indent);
                }
            },
        }
    }
}

/// Like format_spaces, but remove newlines and keep only comments.
pub fn fmt_comments_only<'a, I>(buf: &mut String<'a>, spaces: I, indent: u16)
where
    I: Iterator<Item = &'a CommentOrNewline<'a>>,
{
    use self::CommentOrNewline::*;

    for space in spaces {
        match space {
            Newline => {}
            LineComment(comment) => {
                fmt_comment(buf, comment, indent);
            }
        }
    }
}

fn fmt_comment<'a>(buf: &mut String<'a>, comment: &'a str, indent: u16) {
    buf.push('#');
    buf.push_str(comment);

    newline(buf, indent);
}

pub fn is_comment<'a>(space: &'a CommentOrNewline<'a>) -> bool {
    match space {
        CommentOrNewline::Newline => false,
        CommentOrNewline::LineComment(_) => true,
    }
}
