use std::fmt;

/// Errors returned by the parsers and the XML editing helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// The input ended before any token could be read
    /// (upstream: "xp.get_tag() failed").
    Empty,
    /// The document does not start with the expected element
    /// (upstream: "no start tag").
    NoStartTag(&'static str),
    /// The closing tag of the document or of an element was never found
    /// (upstream: "no end tag", or `ERR_XML_PARSE` from `element_contents`).
    NoEndTag(String),
    /// A marker that an edit helper needs is missing from the XML fragment
    /// (upstream: `insert_after` returning `ERR_XML_PARSE`).
    MarkerNotFound(String),
    /// A required element is missing or empty.
    Missing(&'static str),
    /// More input is needed before the header can be parsed.
    Incomplete,
    /// Anything else that makes the input unusable.
    Invalid(String),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Empty => write!(f, "empty document"),
            ProtoError::NoStartTag(t) => write!(f, "no start tag <{t}>"),
            ProtoError::NoEndTag(t) => write!(f, "no end tag {t}"),
            ProtoError::MarkerNotFound(t) => write!(f, "marker {t:?} not found"),
            ProtoError::Missing(t) => write!(f, "missing {t}"),
            ProtoError::Incomplete => write!(f, "incomplete input"),
            ProtoError::Invalid(t) => write!(f, "invalid input: {t}"),
        }
    }
}

impl std::error::Error for ProtoError {}
