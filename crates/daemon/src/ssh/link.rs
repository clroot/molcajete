//! Links in the terminal: OSC 8 hyperlinks kept on the cells, and plain
//! `http(s)://` and `mailto:` URLs found in the text, like the URL hints
//! of other terminals. Only these schemes are offered to the client.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use molcajete_protocol::InstructionBuilder;

/// Instruction telling the client which link is under the pointer: the
/// target URL, or no argument when there is none. Stock guacd never sends
/// it, and clients that do not know it ignore it.
pub const OPCODE: &str = "link";

/// Schemes a link may have to be offered to the client.
const SCHEMES: [&str; 3] = ["https://", "http://", "mailto:"];

/// Lines a wrapped line is followed for at most, in each direction.
const MAX_WRAPPED_LINES: i32 = 16;

/// A link and the cells it covers (grid points, inclusive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub start: Point,
    pub end: Point,
}

pub fn instruction(link: Option<&Link>) -> String {
    let builder = InstructionBuilder::new(OPCODE);
    match link {
        Some(link) => builder.str_arg(&link.url).finish(),
        None => builder.finish(),
    }
}

/// Whether a URL may be opened by the client.
pub fn is_allowed(url: &str) -> bool {
    SCHEMES.iter().any(|scheme| {
        url.len() > scheme.len()
            && url.as_bytes()[..scheme.len()].eq_ignore_ascii_case(scheme.as_bytes())
    })
}

/// The link at a grid point, if any.
pub fn link_at<T: EventListener>(term: &Term<T>, point: Point) -> Option<Link> {
    let chars = logical_line(term, point.line);
    // The character drawn in this cell (a wide character's spacer belongs
    // to the character before it).
    let index = chars.iter().rposition(|c| c.point <= point)?;

    if let Some(uri) = &chars[index].hyperlink {
        if !is_allowed(uri) {
            return None;
        }
        let same = |c: &LineChar| c.hyperlink.as_ref() == Some(uri);
        let first = chars[..index]
            .iter()
            .rposition(|c| !same(c))
            .map_or(0, |i| i + 1);
        let last = chars[index..]
            .iter()
            .position(|c| !same(c))
            .map_or(chars.len(), |i| index + i)
            - 1;
        return Some(Link {
            url: uri.clone(),
            start: chars[first].point,
            end: chars[last].point,
        });
    }

    let text: Vec<char> = chars.iter().map(|c| c.c).collect();
    let range = find_urls(&text).into_iter().find(|r| r.contains(&index))?;
    Some(Link {
        url: text[range.clone()].iter().collect(),
        start: chars[range.start].point,
        end: chars[range.end - 1].point,
    })
}

/// One character of a logical line and the cell it starts in.
struct LineChar {
    c: char,
    point: Point,
    hyperlink: Option<String>,
}

/// The characters of the line containing `line`, joined with the lines it
/// wraps into and from.
fn logical_line<T: EventListener>(term: &Term<T>, line: Line) -> Vec<LineChar> {
    let grid = term.grid();
    let last_col = Column(grid.columns() - 1);
    let wraps = |line: Line| grid[line][last_col].flags.contains(Flags::WRAPLINE);
    let mut first = line;
    while first > grid.topmost_line() && line.0 - first.0 < MAX_WRAPPED_LINES && wraps(first - 1) {
        first -= 1;
    }
    let mut last = line;
    while last < grid.bottommost_line() && last.0 - line.0 < MAX_WRAPPED_LINES && wraps(last) {
        last += 1;
    }

    let mut chars = Vec::new();
    for l in first.0..=last.0 {
        let row = &grid[Line(l)];
        for col in 0..grid.columns() {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            chars.push(LineChar {
                c: cell.c,
                point: Point::new(Line(l), Column(col)),
                hyperlink: cell.hyperlink().map(|h| h.uri().to_owned()),
            });
        }
    }
    chars
}

/// Characters that end a URL (besides whitespace and control characters).
fn ends_url(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^' | '⟨' | '⟩' | '\0'
        )
}

/// Ranges of the `http(s)://` and `mailto:` URLs in a text.
fn find_urls(text: &[char]) -> Vec<std::ops::Range<usize>> {
    let mut urls = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let at_word_start = i == 0 || !text[i - 1].is_alphanumeric();
        let scheme = SCHEMES.iter().find(|scheme| {
            let len = scheme.len();
            i + len <= text.len()
                && text[i..i + len]
                    .iter()
                    .zip(scheme.chars())
                    .all(|(a, b)| a.eq_ignore_ascii_case(&b))
        });
        let Some(scheme) = scheme.filter(|_| at_word_start) else {
            i += 1;
            continue;
        };
        let mut end = i + scheme.len();
        while end < text.len() && !ends_url(text[end]) {
            end += 1;
        }
        end = trim_end(&text[i..end]) + i;
        if end > i + scheme.len() {
            urls.push(i..end);
        }
        i = end.max(i + 1);
    }
    urls
}

/// Length of a URL candidate without trailing punctuation and unbalanced
/// closing brackets (`(see https://a.b/c).` ends at `c`).
fn trim_end(url: &[char]) -> usize {
    let mut len = url.len();
    while len > 0 {
        let c = url[len - 1];
        let unbalanced = |open: char, close: char| {
            c == close
                && url[..len].iter().filter(|&&x| x == close).count()
                    > url[..len].iter().filter(|&&x| x == open).count()
        };
        if matches!(c, '.' | ',' | ':' | ';' | '!' | '?' | '\'')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
        {
            len -= 1;
        } else {
            break;
        }
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::terminal::{Size, Terminal};

    fn urls(text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        find_urls(&chars)
            .into_iter()
            .map(|r| chars[r].iter().collect())
            .collect()
    }

    #[test]
    fn finds_urls_in_text() {
        assert_eq!(
            urls("see https://example.com/a?b=c#d and http://x.y."),
            ["https://example.com/a?b=c#d", "http://x.y"]
        );
        assert_eq!(
            urls("(https://en.wikipedia.org/wiki/Foo_(bar))."),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
        assert_eq!(urls("<mailto:me@example.com>"), ["mailto:me@example.com"]);
        assert_eq!(urls("HTTPS://EXAMPLE.COM"), ["HTTPS://EXAMPLE.COM"]);
        assert_eq!(urls("https://한국.kr/경로"), ["https://한국.kr/경로"]);
        // Not a scheme of its own, nothing after it, or another scheme.
        assert!(urls("xhttps://example.com").is_empty());
        assert!(urls("https:// and").is_empty());
        assert!(urls("file:///etc/passwd javascript:alert(1)").is_empty());
    }

    #[test]
    fn allows_only_web_and_mail_schemes() {
        assert!(is_allowed("https://example.com"));
        assert!(is_allowed("HTTP://example.com"));
        assert!(is_allowed("mailto:me@example.com"));
        assert!(!is_allowed("https://"));
        assert!(!is_allowed("javascript:alert(1)"));
        assert!(!is_allowed("file:///etc/passwd"));
        assert!(!is_allowed("ssh://host"));
    }

    fn terminal(cols: usize, rows: usize, output: &str) -> Terminal {
        let mut t = Terminal::new(Size { cols, rows }, 100);
        t.feed(output.as_bytes());
        t
    }

    fn at(t: &Terminal, col: usize, line: i32) -> Option<Link> {
        link_at(t.term(), Point::new(Line(line), Column(col)))
    }

    #[test]
    fn plain_urls_in_the_grid() {
        let t = terminal(40, 3, "go to https://example.com/x now");
        let link = at(&t, 8, 0).unwrap();
        assert_eq!(link.url, "https://example.com/x");
        assert_eq!(link.start, Point::new(Line(0), Column(6)));
        assert_eq!(link.end, Point::new(Line(0), Column(26)));
        assert_eq!(at(&t, 6, 0).unwrap().url, "https://example.com/x");
        assert_eq!(at(&t, 26, 0).unwrap().url, "https://example.com/x");
        assert_eq!(at(&t, 5, 0), None);
        assert_eq!(at(&t, 27, 0), None);
    }

    #[test]
    fn urls_across_wrapped_lines_and_wide_characters() {
        let t = terminal(12, 3, "한 https://example.com/abc");
        // "한" takes columns 0-1; the URL starts at column 3 and wraps.
        let link = at(&t, 2, 1).unwrap();
        assert_eq!(link.url, "https://example.com/abc");
        assert_eq!(link.start, Point::new(Line(0), Column(3)));
        assert_eq!(link.end, Point::new(Line(2), Column(1)));
        assert_eq!(at(&t, 5, 0).unwrap(), link);
        assert_eq!(at(&t, 1, 0), None);
    }

    #[test]
    fn osc8_hyperlinks() {
        let t = terminal(
            40,
            2,
            "a \x1b]8;;https://example.com/target\x1b\\link text\x1b]8;;\x1b\\ b \
             \x1b]8;;file:///etc/passwd\x1b\\file\x1b]8;;\x1b\\",
        );
        let link = at(&t, 4, 0).unwrap();
        assert_eq!(link.url, "https://example.com/target");
        assert_eq!(link.start, Point::new(Line(0), Column(2)));
        assert_eq!(link.end, Point::new(Line(0), Column(10)));
        assert_eq!(at(&t, 11, 0), None);
        // Other schemes are not links.
        assert_eq!(at(&t, 15, 0), None);
    }

    #[test]
    fn instructions() {
        let link = Link {
            url: "https://a.b".into(),
            start: Point::default(),
            end: Point::default(),
        };
        assert_eq!(instruction(Some(&link)), "4.link,11.https://a.b;");
        assert_eq!(instruction(None), "4.link;");
    }
}
