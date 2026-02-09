use marked_yaml::Node;
use rattler_build_yaml_parser::ParseError;
use rattler_digest::{Md5, Md5Hash, Sha256, Sha256Hash};

use crate::stage0::{
    parser::helpers::get_span,
    source::{GitRev, GitSource, GitUrl, PathSource, Source, UrlSource},
    types::{ConditionalList, IncludeExclude, Item, JinjaTemplate, NestedItemList, Value},
};

use rattler_build_yaml_parser::{MappingParser, parse_conditional_list, parse_value};

/// Parse a SHA256 hash value (can be concrete or template)
fn parse_sha256_value(node: &Node) -> Result<Value<Sha256Hash>, ParseError> {
    // Check if it's a template
    if let Some(scalar) = node.as_scalar() {
        let s = scalar.as_str();
        let span = *scalar.span();

        // Check if it contains Jinja template syntax
        if s.contains("${{") {
            let template = JinjaTemplate::new(s.to_string())
                .map_err(|e| ParseError::invalid_value("sha256", &e, span))?;
            return Ok(Value::new_template(template, Some(span)));
        }

        // Otherwise parse as concrete SHA256 hash
        let hash = rattler_digest::parse_digest_from_hex::<Sha256>(s).ok_or_else(|| {
            ParseError::invalid_value("sha256", format!("Invalid SHA256 checksum: {}", s), span)
        })?;
        Ok(Value::new_concrete(hash, Some(span)))
    } else {
        Err(ParseError::expected_type(
            "scalar",
            "non-scalar",
            get_span(node),
        ))
    }
}

/// Parse an MD5 hash value (can be concrete or template)
fn parse_md5_value(node: &Node) -> Result<Value<Md5Hash>, ParseError> {
    // Check if it's a template
    if let Some(scalar) = node.as_scalar() {
        let s = scalar.as_str();
        let span = *scalar.span();

        // Check if it contains Jinja template syntax
        if s.contains("${{") {
            let template = JinjaTemplate::new(s.to_string())
                .map_err(|e| ParseError::invalid_value("md5", &e, span))?;
            return Ok(Value::new_template(template, Some(span)));
        }

        // Otherwise parse as concrete MD5 hash
        let hash = rattler_digest::parse_digest_from_hex::<Md5>(s).ok_or_else(|| {
            ParseError::invalid_value("md5", format!("Invalid MD5 checksum: {}", s), span)
        })?;
        Ok(Value::new_concrete(hash, Some(span)))
    } else {
        Err(ParseError::expected_type(
            "scalar",
            "non-scalar",
            get_span(node),
        ))
    }
}

/// Parse source filter field - can be a list or include/exclude mapping
fn parse_source_filter(node: &Node) -> Result<IncludeExclude, ParseError> {
    // Try parsing as a mapping with include/exclude first
    if let Some(mapping) = node.as_mapping() {
        let parser = MappingParser::new(mapping, "filter", &["include", "exclude"]);

        let result = IncludeExclude::Mapping {
            include: parser.optional_list("include")?,
            exclude: parser.optional_list("exclude")?,
        };

        parser.finish()?;
        return Ok(result);
    }

    // Otherwise parse as a simple list
    if node.as_sequence().is_some() {
        return Ok(IncludeExclude::List(parse_conditional_list(node)?));
    }

    Err(ParseError::expected_type(
        "sequence or mapping with include/exclude",
        "other",
        get_span(node),
    )
    .with_message(
        "filter must be either a list of glob patterns or a mapping with include/exclude keys",
    ))
}

/// Parse source section from YAML (can be single or list, with if/then/else support)
pub fn parse_source(node: &Node) -> Result<ConditionalList<Source>, ParseError> {
    match node {
        Node::Sequence(seq) => {
            let mut items = Vec::new();
            for item_node in seq.iter() {
                items.push(parse_source_item(item_node)?);
            }
            Ok(ConditionalList::new(items))
        }
        Node::Mapping(_) => {
            // Single mapping - could be a source or a conditional
            let item = parse_source_item(node)?;
            Ok(ConditionalList::new(vec![item]))
        }
        _ => Err(ParseError::expected_type(
            "mapping or sequence",
            "non-mapping/sequence",
            get_span(node),
        )
        .with_message("Expected 'source' to be a mapping or sequence")),
    }
}

/// Parse a single source item - either a Source or an if/then/else conditional
fn parse_source_item(node: &Node) -> Result<Item<Source>, ParseError> {
    let mapping = node.as_mapping().ok_or_else(|| {
        ParseError::expected_type("mapping", "non-mapping", get_span(node))
            .with_message("Each source item must be a mapping")
    })?;

    // Check if this is an if/then/else conditional
    if mapping.get("if").is_some() {
        return parse_source_conditional(mapping);
    }

    // Otherwise, parse as a regular Source
    let source = parse_single_source(node)?;
    Ok(Item::Value(Value::new_concrete(source, Some(*node.span()))))
}

/// Parse an if/then/else conditional for Source
fn parse_source_conditional(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<Item<Source>, ParseError> {
    use rattler_build_jinja::JinjaExpression;
    use rattler_build_yaml_parser::Conditional;

    let mut condition = None;
    let mut condition_span = None;
    let mut then_values = None;
    let mut else_values = None;

    for (key_node, value_node) in mapping.iter() {
        let key = key_node.as_str();

        match key {
            "if" => {
                let scalar = value_node.as_scalar().ok_or_else(|| {
                    ParseError::expected_type("string", "non-scalar", get_span(value_node))
                })?;
                condition = Some(
                    JinjaExpression::new(scalar.as_str().to_string())
                        .map_err(|e| ParseError::invalid_value("if", &e, *value_node.span()))?,
                );
                condition_span = Some(*value_node.span());
            }
            "then" => {
                then_values = Some(parse_source_then_else(value_node)?);
            }
            "else" => {
                else_values = Some(parse_source_then_else(value_node)?);
            }
            _ => {
                return Err(ParseError::invalid_value(
                    "source conditional",
                    format!("unknown field '{}' in conditional", key),
                    *key_node.span(),
                )
                .with_suggestion("Valid fields in a conditional are: if, then, else"));
            }
        }
    }

    let condition = condition.ok_or_else(|| {
        ParseError::missing_field("if", get_span(&Node::Mapping(mapping.clone())))
    })?;

    let then_values = then_values.ok_or_else(|| {
        ParseError::missing_field("then", get_span(&Node::Mapping(mapping.clone())))
    })?;

    Ok(Item::Conditional(Conditional {
        condition,
        then: then_values,
        else_value: else_values,
        condition_span,
    }))
}

/// Parse the then/else branch of a source conditional (can be single or list)
/// Supports nested if/then/else conditionals
fn parse_source_then_else(node: &Node) -> Result<NestedItemList<Source>, ParseError> {
    match node {
        Node::Sequence(seq) => {
            let mut items = Vec::new();
            for item_node in seq.iter() {
                items.push(parse_source_item(item_node)?);
            }
            Ok(NestedItemList::new(items))
        }
        Node::Mapping(_) => {
            // Single item - could be a source or a nested conditional
            let item = parse_source_item(node)?;
            Ok(NestedItemList::single(item))
        }
        _ => Err(
            ParseError::expected_type("mapping or sequence", "other", get_span(node))
                .with_message("Expected source or list of sources in then/else branch"),
        ),
    }
}

fn parse_single_source(node: &Node) -> Result<Source, ParseError> {
    let mapping = node.as_mapping().ok_or_else(|| {
        ParseError::expected_type("mapping", "non-mapping", get_span(node))
            .with_message("Each source must be a mapping")
    })?;

    // Determine source type by checking which field is present
    if mapping.get("git").is_some() {
        Ok(Source::Git(parse_git_source(mapping)?))
    } else if mapping.get("url").is_some() {
        Ok(Source::Url(parse_url_source(mapping)?))
    } else if mapping.get("path").is_some() {
        Ok(Source::Path(parse_path_source(mapping)?))
    } else {
        Err(
            ParseError::invalid_value("source", "missing git, url, or path field", get_span(node))
                .with_suggestion("Source must have one of: git, url, or path"),
        )
    }
}

fn parse_git_source(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<GitSource, ParseError> {
    let parser = MappingParser::new(
        mapping,
        "git source",
        &[
            "git",
            "rev",
            "tag",
            "branch",
            "depth",
            "patches",
            "target_directory",
            "lfs",
            "expected_commit",
        ],
    );

    let url = parser
        .custom("git", |n| {
            let url_value: Value<String> = parse_value(n)?;
            Ok(GitUrl(url_value))
        })?
        .ok_or_else(|| ParseError::missing_field("git", *mapping.span()))?;

    let rev = parser.custom("rev", |n| Ok(GitRev::Value(parse_value(n)?)))?;
    let tag = parser.custom("tag", |n| Ok(GitRev::Value(parse_value(n)?)))?;
    let branch = parser.custom("branch", |n| Ok(GitRev::Value(parse_value(n)?)))?;

    // Check for conflicting rev/tag/branch
    let rev_count = [rev.is_some(), tag.is_some(), branch.is_some()]
        .iter()
        .filter(|&&x| x)
        .count();
    if rev_count > 1 {
        return Err(ParseError::invalid_value(
            "git source",
            "cannot specify more than one of: rev, tag, branch",
            *mapping.span(),
        ));
    }

    let result = GitSource {
        url,
        rev,
        tag,
        branch,
        depth: parser.optional("depth")?,
        patches: parser.optional_list("patches")?,
        target_directory: parser.optional("target_directory")?,
        lfs: parser.optional("lfs")?,
        expected_commit: parser.optional("expected_commit")?,
    };

    parser.finish()?;
    Ok(result)
}

fn parse_url_source(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<UrlSource, ParseError> {
    let parser = MappingParser::new(
        mapping,
        "url source",
        &[
            "url",
            "sha256",
            "md5",
            "file_name",
            "patches",
            "target_directory",
        ],
    );

    // URL can be a single value or a list
    let url = parser
        .custom("url", |n| {
            let mut urls = Vec::new();
            if let Some(seq) = n.as_sequence() {
                for item in seq.iter() {
                    urls.push(parse_value(item)?);
                }
            } else {
                urls.push(parse_value(n)?);
            }
            Ok(urls)
        })?
        .ok_or_else(|| ParseError::missing_field("url", *mapping.span()))?;

    let result = UrlSource {
        url,
        sha256: parser.custom("sha256", parse_sha256_value)?,
        md5: parser.custom("md5", parse_md5_value)?,
        file_name: parser.optional("file_name")?,
        patches: parser.optional_list("patches")?,
        target_directory: parser.optional("target_directory")?,
    };

    parser.finish()?;
    Ok(result)
}

fn parse_path_source(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<PathSource, ParseError> {
    let parser = MappingParser::new(
        mapping,
        "path source",
        &[
            "path",
            "sha256",
            "md5",
            "patches",
            "target_directory",
            "file_name",
            "use_gitignore",
            "filter",
        ],
    );

    let path = parser
        .optional("path")?
        .ok_or_else(|| ParseError::missing_field("path", *mapping.span()))?;

    let use_gitignore = parser
        .custom("use_gitignore", |n| {
            let scalar = n
                .as_scalar()
                .ok_or_else(|| ParseError::expected_type("boolean", "non-scalar", get_span(n)))?;
            scalar.as_bool().ok_or_else(|| {
                ParseError::invalid_value(
                    "use_gitignore",
                    format!("expected boolean, got '{}'", scalar.as_str()),
                    *n.span(),
                )
            })
        })?
        .unwrap_or(true);

    let result = PathSource {
        path,
        sha256: parser.custom("sha256", parse_sha256_value)?,
        md5: parser.custom("md5", parse_md5_value)?,
        patches: parser.optional_list("patches")?,
        target_directory: parser.optional("target_directory")?,
        file_name: parser.optional("file_name")?,
        use_gitignore,
        filter: parser
            .custom("filter", parse_source_filter)?
            .unwrap_or_default(),
    };

    parser.finish()?;
    Ok(result)
}
