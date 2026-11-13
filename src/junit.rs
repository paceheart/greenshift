//! JUnit XML parsing.
//!
//! JUnit XML has no single standard, so this parser accepts the shape most
//! producers share and ignores everything else:
//!
//! - the root is `<testsuites>` or a lone `<testsuite>`;
//! - `<testsuite>` may nest inside `<testsuite>`;
//! - each `<testcase>` has a `name`, usually a `classname`, and optionally a
//!   `time` in (possibly fractional) seconds;
//! - a testcase's outcome comes from its `<failure>`, `<error>` and
//!   `<skipped>` children, in that order of precedence; a testcase with none
//!   of them passed. Other children (`<system-out>`, `<system-err>`,
//!   `<properties>`, nextest's `<flakyFailure>` / `<rerunFailure>`, ...) are
//!   ignored, and so is all failure, error and output text: only which
//!   elements are present matters.
//!
//! A test's identity is its suite plus its name. The suite is the testcase's
//! `classname` when it has a non-empty one, and otherwise the `name` of the
//! innermost enclosing `<testsuite>`, and empty when neither gives one.

use std::fmt;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// How a single test case ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    Passed,
    Failed,
    Error,
    Skipped,
}

impl Outcome {
    /// The text stored for this outcome.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::Error => "error",
            Outcome::Skipped => "skipped",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `<testcase>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    /// `classname`, or the enclosing suite's name when there is none.
    pub suite: String,
    pub name: String,
    pub outcome: Outcome,
    /// `time` rounded to whole milliseconds; `None` when the producer gave no time.
    pub duration_ms: Option<i64>,
}

/// Every test case in one JUnit document, in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub cases: Vec<TestCase>,
}

/// Why a document could not be read as JUnit XML.
#[derive(Debug, thiserror::Error)]
pub enum JunitError {
    /// `path` is not in the message: callers say which file they were reading.
    #[error("could not read file")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("not valid UTF-8")]
    NotUtf8,
    // quick-xml's own errors repeat their cause in both Display and
    // source(), so keep the text rather than chaining it. `xml_err` escapes
    // it as `{:?}` would, since it quotes text from the file.
    #[error("malformed XML at byte {position}: {message}")]
    Xml { position: u64, message: String },
    #[error("malformed XML at byte {position}: unrecognized entity {name:?}")]
    UnknownEntity { position: u64, name: String },
    #[error("not JUnit XML: root element is {0:?}, expected <testsuites> or <testsuite>")]
    WrongRoot(String),
    #[error("not JUnit XML: no root element")]
    Empty,
    #[error("malformed XML: unexpected end of document")]
    Truncated,
    #[error("malformed XML at byte {position}: content outside the root element")]
    OutsideRoot { position: u64 },
    #[error("malformed JUnit at byte {position}: <testcase> without a name attribute")]
    MissingName { position: u64 },
    #[error("malformed JUnit at byte {position}: <testcase name={name:?}> has time={time:?}, expected seconds")]
    BadTime {
        position: u64,
        name: String,
        time: String,
    },
}

/// Read and parse a JUnit XML file.
pub fn parse_file(path: &Path) -> Result<Report, JunitError> {
    let bytes = std::fs::read(path).map_err(|source| JunitError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let text = String::from_utf8(bytes).map_err(|_| JunitError::NotUtf8)?;
    parse_str(&text)
}

/// The outcome-deciding children seen so far in one testcase.
#[derive(Debug, Default)]
struct CaseBuilder {
    suite: String,
    name: String,
    duration_ms: Option<i64>,
    failure: bool,
    error: bool,
    skipped: bool,
}

impl CaseBuilder {
    /// Note a direct child element of the testcase.
    fn child(&mut self, tag: &[u8]) {
        match tag {
            b"failure" => self.failure = true,
            b"error" => self.error = true,
            b"skipped" => self.skipped = true,
            _ => {}
        }
    }

    fn finish(self) -> TestCase {
        let outcome = if self.failure {
            Outcome::Failed
        } else if self.error {
            Outcome::Error
        } else if self.skipped {
            Outcome::Skipped
        } else {
            Outcome::Passed
        };
        TestCase {
            suite: self.suite,
            name: self.name,
            outcome,
            duration_ms: self.duration_ms,
        }
    }
}

/// Parse a JUnit XML document.
///
/// Failure and error text is not kept: only which elements are present.
pub fn parse_str(xml: &str) -> Result<Report, JunitError> {
    let mut reader = Reader::from_str(xml);
    let mut report = Report::default();
    let mut seen_root = false;
    let mut depth: usize = 0;
    // Names of the enclosing <testsuite> elements, with the depth each opened at.
    let mut suites: Vec<(usize, String)> = Vec::new();
    // The open <testcase>, with the depth it opened at.
    let mut case: Option<(usize, CaseBuilder)> = None;

    loop {
        let position = reader.buffer_position();
        let event = reader
            .read_event()
            .map_err(|e| xml_err(reader.error_position(), e))?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                let tag = e.local_name();
                let tag = tag.as_ref();
                if seen_root && depth == 0 {
                    return Err(JunitError::OutsideRoot { position });
                }
                if !seen_root {
                    seen_root = true;
                    if tag != b"testsuites" && tag != b"testsuite" {
                        return Err(JunitError::WrongRoot(
                            String::from_utf8_lossy(tag).into_owned(),
                        ));
                    }
                }
                // The depth this element sits at, whether or not it has content.
                let at = depth + 1;
                if !empty {
                    depth = at;
                }
                match (tag, case.as_mut()) {
                    (b"testsuite", None) => {
                        if !empty {
                            let name = attr(e, b"name", position)?;
                            suites.push((at, name.unwrap_or_default()));
                        }
                    }
                    (b"testcase", None) => {
                        let builder = start_case(e, &suites, position)?;
                        if empty {
                            report.cases.push(builder.finish());
                        } else {
                            case = Some((at, builder));
                        }
                    }
                    (_, Some((case_depth, builder))) if at == *case_depth + 1 => {
                        builder.child(tag);
                    }
                    _ => {}
                }
            }
            Event::End(_) => {
                if case.as_ref().is_some_and(|(d, _)| *d == depth) {
                    let (_, builder) = case.take().expect("checked above");
                    report.cases.push(builder.finish());
                } else if suites.last().is_some_and(|(d, _)| *d == depth) {
                    suites.pop();
                }
                depth = depth.saturating_sub(1);
            }
            Event::GeneralRef(r) => {
                if depth == 0 {
                    return Err(JunitError::OutsideRoot { position });
                }
                // Text is not kept, but an undefined entity is still malformed XML.
                check_ref(&r, position)?;
            }
            Event::Text(t) if depth == 0 && !t.iter().all(u8::is_ascii_whitespace) => {
                return Err(JunitError::OutsideRoot { position });
            }
            Event::CData(_) if depth == 0 => {
                return Err(JunitError::OutsideRoot { position });
            }
            // An XML declaration or doctype is only legal in the prolog,
            // before the root starts; a PI or comment there (or in the
            // epilog, after the root ends) is ordinary XML and stays ignored
            // below.
            Event::Decl(_) | Event::DocType(_) if seen_root && depth == 0 => {
                return Err(JunitError::OutsideRoot { position });
            }
            Event::Eof => {
                if !seen_root {
                    return Err(JunitError::Empty);
                }
                if depth != 0 {
                    return Err(JunitError::Truncated);
                }
                return Ok(report);
            }
            Event::Text(_)
            | Event::CData(_)
            | Event::Decl(_)
            | Event::PI(_)
            | Event::DocType(_)
            | Event::Comment(_) => {}
        }
    }
}

/// `position` is where the reader failed for errors from `read_event`, and
/// the start of the event being examined for errors found after it (in
/// attributes or references): the reader has no position for those.
///
/// quick-xml's messages quote text from the file (tag names, entity names)
/// verbatim, so they are escaped as `{:?}` escapes the other variants'
/// file text (control and invisible characters such as bidi overrides, and
/// backslash), except that quotes are left alone, before they can reach a
/// terminal.
fn xml_err(position: u64, source: quick_xml::Error) -> JunitError {
    JunitError::Xml {
        position,
        message: escape_file_text(&source.to_string()),
    }
}

/// Escape text quoted from a file as `{:?}` would, except that quotes are
/// left alone, so it can be shown in an error message.
pub(crate) fn escape_file_text(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '"' | '\'' => out.push(c),
            _ => out.extend(c.escape_debug()),
        }
    }
    out
}

fn start_case(
    e: &BytesStart<'_>,
    suites: &[(usize, String)],
    position: u64,
) -> Result<CaseBuilder, JunitError> {
    let Some(name) = attr(e, b"name", position)?.filter(|n| !n.is_empty()) else {
        return Err(JunitError::MissingName { position });
    };
    let classname = attr(e, b"classname", position)?.filter(|c| !c.is_empty());
    let suite = classname
        .or_else(|| suites.last().map(|(_, s)| s.clone()))
        .unwrap_or_default();
    let duration_ms = match attr(e, b"time", position)? {
        None => None,
        Some(t) if t.trim().is_empty() => None,
        Some(t) => Some(seconds_to_ms(&t).ok_or_else(|| JunitError::BadTime {
            position,
            name: name.clone(),
            time: t.clone(),
        })?),
    };
    Ok(CaseBuilder {
        suite,
        name,
        duration_ms,
        ..CaseBuilder::default()
    })
}

/// Seconds (e.g. `"0.012"`, `"3"`, `"1e-3"`) to whole milliseconds, rounding
/// to nearest. `None` for anything that is not a finite, non-negative number.
fn seconds_to_ms(text: &str) -> Option<i64> {
    let secs: f64 = text.trim().parse().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    let ms = (secs * 1000.0).round();
    // An i64 holds ~292 million years of milliseconds; anything past that is
    // junk. `i64::MAX` is not exactly representable in f64 and rounds up to
    // 2**63, so this must be a strict `<` or `ms == 2**63` would pass the
    // check and then saturate on the cast below instead of being rejected.
    (ms < i64::MAX as f64).then_some(ms as i64)
}

/// An attribute's unescaped value, if present. `position` is the start of
/// the element: quick-xml's escape errors carry a range relative to the
/// attribute value, which would read as a file offset, so they are reported
/// without it.
fn attr(e: &BytesStart<'_>, key: &[u8], position: u64) -> Result<Option<String>, JunitError> {
    use quick_xml::escape::EscapeError;
    for a in e.attributes() {
        let a = a.map_err(|s| xml_err(position, quick_xml::Error::InvalidAttr(s)))?;
        if a.key.local_name().as_ref() == key {
            return match a.unescape_value() {
                Ok(v) => Ok(Some(v.into_owned())),
                Err(quick_xml::Error::Escape(EscapeError::UnrecognizedEntity(_, name))) => {
                    Err(JunitError::UnknownEntity { position, name })
                }
                Err(quick_xml::Error::Escape(EscapeError::UnterminatedEntity(_))) => {
                    Err(JunitError::Xml {
                        position,
                        message: "cannot find ';' after '&'".into(),
                    })
                }
                Err(s) => Err(xml_err(position, s)),
            };
        }
    }
    Ok(None)
}

/// Check that a `&amp;`-style or `&#NN;`-style reference in text is defined.
fn check_ref(r: &quick_xml::events::BytesRef<'_>, position: u64) -> Result<(), JunitError> {
    let xml = |source| xml_err(position, source);
    if r.resolve_char_ref().map_err(xml)?.is_some() {
        return Ok(());
    }
    let name = r.decode().map_err(|e| xml(quick_xml::Error::Encoding(e)))?;
    match quick_xml::escape::resolve_predefined_entity(&name) {
        Some(_) => Ok(()),
        None => Err(JunitError::UnknownEntity {
            position,
            name: name.into_owned(),
        }),
    }
}

#[cfg(test)]
mod tests;
