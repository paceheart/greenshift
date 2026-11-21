//! Leiningen's `project.clj`, as greenshift reads and edits it: whether
//! its `:test-selectors` map has the `:greenshift` selector that
//! `greenshift run lein` runs a group with, and that selector merged into
//! it, keeping every other byte of the file as it was.
//!
//! The file is Clojure, which greenshift reads only as far as it needs
//! to: forms, strings, comments, character literals and reader macros
//! ([`read_forms`]), with no evaluation. The `defproject` form's top-level
//! `:test-selectors` value is either a literal map, which greenshift
//! edits, or computed (`~(…)` or anything else), which it leaves alone.

/// The selector's key in `:test-selectors`.
pub const SELECTOR_KEY: &str = ":greenshift";

/// The system property the selector reads the names file's path from.
pub const NAMES_PROPERTY: &str = "greenshift.names";

/// The selector entry as `init lein` writes it: comment lines, then the
/// key and its value, every line after the first indented from the key's
/// column (here column 0).
const ENTRY: &str = include_str!("lein/selector.clj");

/// A span of the text: byte offsets, `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// What kind of form a [`Form`] is, as far as greenshift tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `(…)`, `[…]`, `{…}`, `#{…}`, `#(…)`, `#?(…)` and the like: the
    /// opening delimiter as written, and the forms inside, discarded ones
    /// (`#_`) left out.
    Coll { open: String, children: Vec<Form> },
    /// A symbol, keyword, number or character literal.
    Token,
    /// A string or regex literal.
    Str,
    /// A form behind a prefix: `'`, `` ` ``, `~`, `~@`, `@`, `#'`, `#=`,
    /// metadata (`^meta`, kept in `meta`) or a tagged literal (`#tag`).
    Prefixed {
        prefix: String,
        meta: Option<Box<Form>>,
        inner: Box<Form>,
    },
}

/// One form read from the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub span: Span,
    pub kind: Kind,
}

impl Form {
    /// The forms inside a collection, or none.
    pub fn children(&self) -> &[Form] {
        match &self.kind {
            Kind::Coll { children, .. } => children,
            _ => &[],
        }
    }

    /// The text of the form in `text`.
    pub fn text<'t>(&self, text: &'t str) -> &'t str {
        &text[self.span.start..self.span.end]
    }

    fn is_coll(&self, delimiter: &str) -> bool {
        matches!(&self.kind, Kind::Coll { open, .. } if open == delimiter)
    }
}

/// Why `project.clj` could not be read: where, and what is wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {why}")]
pub struct ReadError {
    pub line: usize,
    pub why: String,
}

/// Read every top-level form of `text`, comments and whitespace left out.
pub fn read_forms(text: &str) -> Result<Vec<Form>, ReadError> {
    let mut reader = Reader { text, at: 0 };
    let mut forms = Vec::new();
    while let Some(form) = reader.form(None)? {
        forms.push(form);
    }
    Ok(forms)
}

struct Reader<'t> {
    text: &'t str,
    at: usize,
}

/// Bytes that end a token.
fn ends_token(b: u8) -> bool {
    b.is_ascii_whitespace() || b",()[]{}\";".contains(&b)
}

impl Reader<'_> {
    fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes().get(self.at).copied()
    }

    fn error(&self, at: usize, why: impl Into<String>) -> ReadError {
        ReadError {
            line: self.text[..at.min(self.text.len())].matches('\n').count() + 1,
            why: why.into(),
        }
    }

    /// Skip whitespace, commas and comments.
    fn skip_blank(&mut self) {
        while let Some(b) = self.peek() {
            if b.is_ascii_whitespace() || b == b',' {
                self.at += 1;
            } else if b == b';' {
                match self.text[self.at..].find('\n') {
                    Some(end) => self.at += end + 1,
                    None => self.at = self.text.len(),
                }
            } else {
                break;
            }
        }
    }

    /// Skip one whole character, however many bytes it takes.
    fn skip_char(&mut self) {
        if let Some(c) = self.text[self.at..].chars().next() {
            self.at += c.len_utf8();
        }
    }

    /// The next form, or `None` at `close` (consumed) or, with no `close`,
    /// at the end of the text. `close` is the closing delimiter and where
    /// its collection opened. Discarded forms (`#_`) are skipped.
    fn form(&mut self, close: Option<(u8, usize)>) -> Result<Option<Form>, ReadError> {
        loop {
            self.skip_blank();
            let start = self.at;
            let Some(b) = self.peek() else {
                return match close {
                    None => Ok(None),
                    Some((close, opened)) => Err(self.error(
                        opened,
                        format!("this form is never closed (no {:?})", close as char),
                    )),
                };
            };
            if close.is_some_and(|(close, _)| close == b) {
                self.at += 1;
                return Ok(None);
            }
            if b"([{".contains(&b) {
                self.at += 1;
                let text = self.text;
                return self.coll(start, &text[start..start + 1]).map(Some);
            }
            if b")]}".contains(&b) {
                return Err(self.error(start, format!("unexpected {:?}", b as char)));
            }
            if b == b'"' {
                self.at += 1;
                self.string(start)?;
                return Ok(Some(Form {
                    span: Span {
                        start,
                        end: self.at,
                    },
                    kind: Kind::Str,
                }));
            }
            if b == b'\\' {
                self.at += 1;
                if self.peek().is_none() {
                    return Err(self.error(start, "the text ends in a character literal"));
                }
                self.skip_char();
                self.token_rest();
                return Ok(Some(self.token(start)));
            }
            const PREFIXES: &[&[u8]] = &[b"~@", b"'", b"`", b"~", b"@"];
            if let Some(prefix) = PREFIXES
                .iter()
                .find(|p| self.bytes()[start..].starts_with(p))
            {
                self.at += prefix.len();
                return self.prefixed(start, None).map(Some);
            }
            if b == b'^' {
                self.at += 1;
                let meta = self.required(start, "metadata")?;
                return self.prefixed(start, Some(meta)).map(Some);
            }
            if b == b'#' {
                match self.dispatch(start)? {
                    Some(form) => return Ok(Some(form)),
                    None => continue,
                }
            }
            self.token_rest();
            return Ok(Some(self.token(start)));
        }
    }

    /// A form that must come next, for what `what` is.
    fn required(&mut self, start: usize, what: &str) -> Result<Form, ReadError> {
        match self.form(None)? {
            Some(form) => Ok(form),
            None => Err(self.error(start, format!("{what} is not followed by a form"))),
        }
    }

    /// The form a prefix that ran from `start` applies to.
    fn prefixed(&mut self, start: usize, meta: Option<Form>) -> Result<Form, ReadError> {
        let prefix = self.text[start..self.at].to_owned();
        let inner = self.required(start, "a prefix")?;
        Ok(Form {
            span: Span {
                start,
                end: inner.span.end,
            },
            kind: Kind::Prefixed {
                prefix,
                meta: meta.map(Box::new),
                inner: Box::new(inner),
            },
        })
    }

    /// A `#` form starting at `start`, or `None` for a discard (`#_`),
    /// whose form is skipped.
    fn dispatch(&mut self, start: usize) -> Result<Option<Form>, ReadError> {
        self.at += 1;
        let Some(next) = self.peek() else {
            return Err(self.error(start, "the text ends after #"));
        };
        match next {
            b'{' | b'(' => {
                self.at += 1;
                let text = self.text;
                self.coll(start, &text[start..self.at]).map(Some)
            }
            b'"' => {
                self.at += 1;
                self.string(start)?;
                Ok(Some(Form {
                    span: Span {
                        start,
                        end: self.at,
                    },
                    kind: Kind::Str,
                }))
            }
            b'_' => {
                self.at += 1;
                self.required(start, "#_")?;
                Ok(None)
            }
            b'\'' | b'=' => {
                self.at += 1;
                self.prefixed(start, None).map(Some)
            }
            b'#' => {
                // ##Inf, ##-Inf, ##NaN
                self.at += 1;
                self.token_rest();
                Ok(Some(self.token(start)))
            }
            b'?' => {
                // #?( and #?@(
                self.at += 1;
                if self.peek() == Some(b'@') {
                    self.at += 1;
                }
                if self.peek() != Some(b'(') {
                    return Err(self.error(start, "a reader conditional needs ("));
                }
                self.at += 1;
                let text = self.text;
                self.coll(start, &text[start..self.at]).map(Some)
            }
            b':' => {
                // #:ns{…}, #::{…}
                self.token_rest();
                if self.peek() != Some(b'{') {
                    return Err(self.error(start, "a namespaced map needs {"));
                }
                self.at += 1;
                let text = self.text;
                self.coll(start, &text[start..self.at]).map(Some)
            }
            b if !ends_token(b) => {
                // A tagged literal: #inst "…", #uuid "…", #my/tag form.
                self.token_rest();
                self.prefixed(start, None).map(Some)
            }
            _ => Err(self.error(start, "# is not followed by a form")),
        }
    }

    /// The rest of a collection whose opening delimiter `open` ran up to
    /// here from `start`.
    fn coll(&mut self, start: usize, open: &str) -> Result<Form, ReadError> {
        let close = match open.as_bytes()[open.len() - 1] {
            b'(' => b')',
            b'[' => b']',
            _ => b'}',
        };
        let open = open.to_owned();
        let mut children = Vec::new();
        while let Some(child) = self.form(Some((close, start)))? {
            children.push(child);
        }
        Ok(Form {
            span: Span {
                start,
                end: self.at,
            },
            kind: Kind::Coll { open, children },
        })
    }

    /// The rest of a string or regex literal, up to and past its closing
    /// quote.
    fn string(&mut self, start: usize) -> Result<(), ReadError> {
        loop {
            match self.peek() {
                None => return Err(self.error(start, "a string is never closed")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(());
                }
                Some(b'\\') => {
                    self.at += 1;
                    self.skip_char();
                }
                Some(_) => self.skip_char(),
            }
        }
    }

    fn token_rest(&mut self) {
        while self.peek().is_some_and(|b| !ends_token(b)) {
            self.at += 1;
        }
    }

    fn token(&self, start: usize) -> Form {
        Form {
            span: Span {
                start,
                end: self.at,
            },
            kind: Kind::Token,
        }
    }
}

/// The `:test-selectors` of a project, as written in its `defproject`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selectors {
    /// No `:test-selectors` key. `after` is the end of the last form of
    /// the `defproject`, and `indent` the column of its first key.
    Absent { after: usize, indent: usize },
    /// A literal map: the map form.
    Literal(Form),
    /// Any other value, such as `~(…)`, which only evaluation can read.
    Computed,
}

/// Why `project.clj` cannot be read or merged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectError {
    #[error("project.clj cannot be read as Clojure: {0}")]
    Unreadable(#[from] ReadError),
    #[error("project.clj has no (defproject …) form")]
    NoDefproject,
    #[error("project.clj has more than one (defproject …) form")]
    TwoDefprojects,
    #[error("project.clj's defproject gives :test-selectors more than once")]
    TwoSelectorMaps,
    #[error("project.clj's :test-selectors map gives :greenshift more than once")]
    TwoGreenshiftSelectors,
    #[error(
        "project.clj's :test-selectors is computed, not a literal map, so init cannot add \
         :greenshift to it; add the entry by hand (see the README), or leave it out and \
         `greenshift run lein` uses `lein test :only`"
    )]
    Computed,
}

/// The `defproject` form of `forms`.
fn defproject<'f>(text: &str, forms: &'f [Form]) -> Result<&'f Form, ProjectError> {
    let mut found = forms.iter().filter(|form| {
        form.is_coll("(")
            && form
                .children()
                .first()
                .is_some_and(|head| head.kind == Kind::Token && head.text(text) == "defproject")
    });
    let project = found.next().ok_or(ProjectError::NoDefproject)?;
    if found.next().is_some() {
        return Err(ProjectError::TwoDefprojects);
    }
    Ok(project)
}

/// The column of byte offset `at` in `text`: the characters before it on
/// its line.
fn column(text: &str, at: usize) -> usize {
    let line_start = text[..at].rfind('\n').map_or(0, |nl| nl + 1);
    text[line_start..at].chars().count()
}

/// The key forms and value forms of a map's (or a defproject's) children,
/// as pairs; an odd one out has no value.
fn pairs(children: &[Form]) -> impl Iterator<Item = (&Form, Option<&Form>)> {
    children.chunks(2).map(|pair| (&pair[0], pair.get(1)))
}

/// The project's `:test-selectors`.
pub fn selectors(text: &str) -> Result<Selectors, ProjectError> {
    let forms = read_forms(text)?;
    let project = defproject(text, &forms)?;
    // (defproject name version & {:as options})
    let options = project.children().get(3..).unwrap_or(&[]);
    let mut found = None;
    for (key, value) in pairs(options) {
        if key.kind == Kind::Token && key.text(text) == ":test-selectors" {
            if found.is_some() {
                return Err(ProjectError::TwoSelectorMaps);
            }
            found = Some(value);
        }
    }
    Ok(match found {
        None => {
            let last = project.children().last().expect("defproject is its head");
            let indent = options
                .first()
                .map_or(2, |first| column(text, first.span.start));
            Selectors::Absent {
                after: last.span.end,
                indent,
            }
        }
        Some(Some(map)) if map.is_coll("{") => Selectors::Literal(map.clone()),
        Some(_) => Selectors::Computed,
    })
}

/// Whether the project's literal `:test-selectors` map has a `:greenshift`
/// entry, which `greenshift run lein` runs a group with. A computed map is
/// taken not to: only evaluation could tell.
pub fn has_selector(text: &str) -> Result<bool, ProjectError> {
    Ok(match selectors(text)? {
        Selectors::Literal(map) => pairs(map.children())
            .any(|(key, _)| key.kind == Kind::Token && key.text(text) == SELECTOR_KEY),
        _ => false,
    })
}

/// The entry's comment lines and its key and value, as lines with no
/// indent of their own: the comment lines, then the key line.
fn entry_lines() -> (Vec<&'static str>, Vec<&'static str>) {
    let lines: Vec<&str> = ENTRY.lines().collect();
    let key = lines
        .iter()
        .position(|line| line.starts_with(SELECTOR_KEY))
        .expect("the entry has its key");
    (lines[..key].to_vec(), lines[key..].to_vec())
}

/// `lines` joined by newlines, every line after the first indented by
/// `indent` spaces more than written.
fn indented(lines: &[&str], indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    for (at, line) in lines.iter().enumerate() {
        if at > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(&pad);
            }
        }
        out.push_str(line);
    }
    out
}

/// The selector's value as written with its key at column `key_column`.
pub fn selector_value(key_column: usize) -> String {
    let (_, entry) = entry_lines();
    let value = indented(&entry, key_column);
    value[SELECTOR_KEY.len()..].trim_start().to_owned()
}

/// The whole entry, comments and all, with the key at `key_column` and
/// the first line starting there too (no indent of its own).
fn entry(key_column: usize) -> String {
    let (comments, key) = entry_lines();
    let all: Vec<&str> = comments.into_iter().chain(key).collect();
    indented(&all, key_column)
}

/// What merging the selector into `project.clj` comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// The map already has the entry, exactly as init writes it.
    Unchanged,
    /// The file with the entry added, or with `--force` replaced.
    Changed(String),
    /// The map has a `:greenshift` entry that differs, and `--force` was
    /// not given.
    Conflict,
}

/// `text` with the `:greenshift` selector in its `:test-selectors` map,
/// every other byte kept: added as the map's last entry, at its keys'
/// column; with no map, a new `:test-selectors {:greenshift …}` after the
/// defproject's last option. A different `:greenshift` entry is a
/// [`Merge::Conflict`], or with `force`, has its value replaced.
pub fn merge(text: &str, force: bool) -> Result<Merge, ProjectError> {
    match selectors(text)? {
        Selectors::Computed => Err(ProjectError::Computed),
        Selectors::Absent { after, indent } => {
            let (comments, key) = entry_lines();
            let pad = " ".repeat(indent);
            let head = ":test-selectors {";
            let mut addition: String = comments
                .iter()
                .map(|line| format!("{pad}{line}\n"))
                .collect();
            addition.push_str(&format!(
                "{pad}{head}{}}}",
                indented(&key, indent + head.len())
            ));
            // insert_after indents the first line itself.
            Ok(Merge::Changed(insert_after(
                text,
                after,
                indent,
                &addition[indent..],
            )))
        }
        Selectors::Literal(map) => {
            let mut existing = None;
            for (key, value) in pairs(map.children()) {
                if key.kind == Kind::Token && key.text(text) == SELECTOR_KEY {
                    if existing.is_some() {
                        return Err(ProjectError::TwoGreenshiftSelectors);
                    }
                    existing = Some((key, value));
                }
            }
            if let Some((key, value)) = existing {
                let wanted = selector_value(column(text, key.span.start));
                let Some(value) = value else {
                    // A key with no value: put the value after it.
                    return Ok(if force {
                        Merge::Changed(splice(
                            text,
                            key.span.end,
                            key.span.end,
                            &format!(" {wanted}"),
                        ))
                    } else {
                        Merge::Conflict
                    });
                };
                return Ok(if value.text(text) == wanted {
                    Merge::Unchanged
                } else if force {
                    Merge::Changed(splice(text, value.span.start, value.span.end, &wanted))
                } else {
                    Merge::Conflict
                });
            }
            Ok(Merge::Changed(match map.children().last() {
                Some(last) => {
                    let key_column = column(text, map.children()[0].span.start);
                    insert_after(text, last.span.end, key_column, &entry(key_column))
                }
                None => {
                    // An empty map: the entry goes just inside its brace.
                    let open = map.span.start + 1;
                    splice(text, open, open, &entry(column(text, open)))
                }
            }))
        }
    }
}

/// `text` with `at..end` replaced by `with`.
fn splice(text: &str, at: usize, end: usize, with: &str) -> String {
    format!("{}{with}{}", &text[..at], &text[end..])
}

/// `text` with `addition` (whose first line is not indented, and whose
/// later lines are) put on lines of its own after the form that ends at
/// `after`, starting at column `column`. When only blanks and a comment
/// follow that form on its line, the addition goes on the next line, so
/// the comment stays with the form; otherwise straight after the form, on
/// a new line, so what followed it (a closing brace, say) follows the
/// addition.
fn insert_after(text: &str, after: usize, column: usize, addition: &str) -> String {
    let pad = " ".repeat(column);
    let rest_of_line = &text[after..];
    let line_end = rest_of_line.find('\n');
    let rest = &rest_of_line[..line_end.unwrap_or(rest_of_line.len())];
    let trimmed = rest.trim_start_matches([' ', '\t', ',']);
    if let (Some(nl), true) = (line_end, trimmed.is_empty() || trimmed.starts_with(';')) {
        let at = after + nl + 1;
        return splice(text, at, at, &format!("{pad}{addition}\n"));
    }
    splice(text, after, after, &format!("\n{pad}{addition}"))
}

/// The Clojure string literal that reads as `text`: `"` and `\` escaped,
/// and newline, carriage return and tab as `\n`, `\r` and `\t`.
pub fn clojure_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests;
