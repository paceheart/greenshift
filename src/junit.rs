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
//! innermost enclosing `<testsuite>`.

use std::fmt;
use std::path::Path;

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
    #[error("could not read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("not valid UTF-8")]
    NotUtf8,
    #[error("malformed XML at byte {position}: {source}")]
    Xml {
        position: u64,
        #[source]
        source: quick_xml::Error,
    },
    #[error("not JUnit XML: root element is <{0}>, expected <testsuites> or <testsuite>")]
    WrongRoot(String),
    #[error("not JUnit XML: no root element")]
    Empty,
    #[error("malformed XML: unexpected end of document")]
    Truncated,
    #[error("malformed JUnit at byte {position}: <testcase> without a name attribute")]
    MissingName { position: u64 },
    #[error("malformed JUnit at byte {position}: <testcase name=\"{name}\"> has time=\"{time}\", expected seconds")]
    BadTime {
        position: u64,
        name: String,
        time: String,
    },
}

/// Read and parse a JUnit XML file.
pub fn parse_file(path: &Path) -> Result<Report, JunitError> {
    let bytes = std::fs::read(path).map_err(|source| JunitError::Io {
        path: path.display().to_string(),
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
    let xml_err = |reader: &Reader<&[u8]>, source| JunitError::Xml {
        position: reader.error_position(),
        source,
    };

    let mut report = Report::default();
    let mut seen_root = false;
    let mut depth: usize = 0;
    // Names of the enclosing <testsuite> elements, with the depth each opened at.
    let mut suites: Vec<(usize, String)> = Vec::new();
    // The open <testcase>, with the depth it opened at.
    let mut case: Option<(usize, CaseBuilder)> = None;

    loop {
        let position = reader.buffer_position();
        let event = reader.read_event().map_err(|e| xml_err(&reader, e))?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                let tag = e.local_name();
                let tag = tag.as_ref();
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
                            let name = attr(e, b"name").map_err(|s| xml_err(&reader, s))?;
                            suites.push((at, name.unwrap_or_default()));
                        }
                    }
                    (b"testcase", None) => {
                        let builder =
                            start_case(e, &suites, position).map_err(|err| match err {
                                StartCaseError::Xml(s) => xml_err(&reader, s),
                                StartCaseError::Junit(j) => j,
                            })?;
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
                // Text is not kept, but an undefined entity is still malformed XML.
                check_ref(&r).map_err(|e| xml_err(&reader, e))?;
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

enum StartCaseError {
    Xml(quick_xml::Error),
    Junit(JunitError),
}

fn start_case(
    e: &BytesStart<'_>,
    suites: &[(usize, String)],
    position: u64,
) -> Result<CaseBuilder, StartCaseError> {
    let name = attr(e, b"name").map_err(StartCaseError::Xml)?;
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(StartCaseError::Junit(JunitError::MissingName { position }));
    };
    let classname = attr(e, b"classname")
        .map_err(StartCaseError::Xml)?
        .filter(|c| !c.is_empty());
    let suite = classname
        .or_else(|| suites.last().map(|(_, s)| s.clone()))
        .unwrap_or_default();
    let duration_ms = match attr(e, b"time").map_err(StartCaseError::Xml)? {
        None => None,
        Some(t) if t.trim().is_empty() => None,
        Some(t) => Some(seconds_to_ms(&t).ok_or_else(|| {
            StartCaseError::Junit(JunitError::BadTime {
                position,
                name: name.clone(),
                time: t.clone(),
            })
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
    // An i64 holds ~292 million years of milliseconds; anything past that is junk.
    (ms <= i64::MAX as f64).then_some(ms as i64)
}

/// An attribute's unescaped value, if present.
fn attr(e: &BytesStart<'_>, key: &[u8]) -> Result<Option<String>, quick_xml::Error> {
    for a in e.attributes() {
        let a = a.map_err(quick_xml::Error::InvalidAttr)?;
        if a.key.local_name().as_ref() == key {
            return Ok(Some(a.unescape_value()?.into_owned()));
        }
    }
    Ok(None)
}

/// Check that a `&amp;`-style or `&#NN;`-style reference in text is defined.
fn check_ref(r: &quick_xml::events::BytesRef<'_>) -> Result<(), quick_xml::Error> {
    if r.resolve_char_ref()?.is_some() {
        return Ok(());
    }
    let name = r.decode().map_err(quick_xml::Error::Encoding)?;
    match quick_xml::escape::resolve_predefined_entity(&name) {
        Some(_) => Ok(()),
        None => Err(quick_xml::Error::Escape(
            quick_xml::escape::EscapeError::UnrecognizedEntity(0..name.len(), name.into_owned()),
        )),
    }
}

#[cfg(test)]
mod tests;
