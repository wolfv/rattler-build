//! Helper functions for parsing

use marked_yaml::Node as MarkedNode;

use crate::Span;

/// Get the span from a marked_yaml node
pub(crate) fn get_span(node: &MarkedNode) -> Span {
    match node {
        MarkedNode::Scalar(s) => *s.span(),
        MarkedNode::Mapping(m) => *m.span(),
        MarkedNode::Sequence(s) => *s.span(),
    }
}
