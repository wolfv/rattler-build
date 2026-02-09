use marked_yaml::Node;
use rattler_build_yaml_parser::{
    MappingParser, parse_conditional_list, parse_conditional_list_or_item, parse_value,
};

use crate::{
    ParseError,
    stage0::{
        Conditional, ConditionalList, Item, JinjaExpression, NestedItemList, Value,
        parser::helpers::get_span,
        tests::{
            CommandsTest, CommandsTestFiles, CommandsTestRequirements, DownstreamTest,
            PackageContentsCheckFiles, PackageContentsTest, PerlTest, PythonTest, PythonVersion,
            RTest, RubyTest, TestType,
        },
    },
};

/// Parse tests section from YAML (expects a sequence)
/// Returns a ConditionalList<TestType> which supports if/then/else conditionals
pub fn parse_tests(node: &Node) -> Result<ConditionalList<TestType>, ParseError> {
    let seq = node.as_sequence().ok_or_else(|| {
        ParseError::expected_type("sequence", "non-sequence", get_span(node))
            .with_message("Expected 'tests' to be a sequence")
    })?;

    let mut items = Vec::new();
    for item in seq.iter() {
        items.push(parse_test_item(item)?);
    }
    Ok(ConditionalList::new(items))
}

/// Parse a single test item which can be either a TestType or a conditional
/// TODO: Refactor to reduce code duplication with other conditional parsers
fn parse_test_item(node: &Node) -> Result<Item<TestType>, ParseError> {
    // Check if it's a conditional (mapping with "if" key)
    if let Some(mapping) = node.as_mapping()
        && mapping.get("if").is_some()
    {
        return parse_conditional_test_item(mapping);
    }

    // Not a conditional - parse as a regular TestType
    let test = parse_single_test(node)?;
    Ok(Item::Value(Value::new_concrete(test, None)))
}

/// Parse a conditional test item with if/then/else branches
fn parse_conditional_test_item(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<Item<TestType>, ParseError> {
    let if_node = mapping
        .get("if")
        .ok_or_else(|| ParseError::missing_field("if", *mapping.span()))?;

    let condition_str = if_node.as_scalar().ok_or_else(|| {
        ParseError::expected_type("scalar", "non-scalar", get_span(if_node))
            .with_message("'if' condition must be a string")
    })?;

    let condition_span = *condition_str.span();
    let condition = JinjaExpression::new(condition_str.as_str().to_string())
        .map_err(|e| ParseError::jinja_error(e, condition_span))?;

    let then_node = mapping
        .get("then")
        .ok_or_else(|| ParseError::missing_field("then", *mapping.span()))?;

    let then_tests = parse_test_list_as_values(then_node)?;

    let else_tests = if let Some(else_node) = mapping.get("else") {
        Some(parse_test_list_as_values(else_node)?)
    } else {
        None
    };

    Ok(Item::Conditional(Conditional {
        condition,
        then: then_tests,
        else_value: else_tests,
        condition_span: Some(condition_span),
    }))
}

/// Parse a test list from a sequence node (or a single test mapping)
/// Supports nested if/then/else conditionals
fn parse_test_list_as_values(node: &Node) -> Result<NestedItemList<TestType>, ParseError> {
    // If it's a sequence, parse each item as a test or conditional
    if let Some(seq) = node.as_sequence() {
        let mut items = Vec::new();
        for item_node in seq.iter() {
            items.push(parse_test_item(item_node)?);
        }
        Ok(NestedItemList::new(items))
    } else if node.as_mapping().is_some() {
        // Single test mapping - could be a test or a nested conditional
        let item = parse_test_item(node)?;
        Ok(NestedItemList::single(item))
    } else {
        Err(ParseError::expected_type(
            "sequence or mapping",
            "non-sequence/mapping",
            get_span(node),
        )
        .with_message("'then' and 'else' must be sequences of tests or a single test"))
    }
}

fn parse_single_test(node: &Node) -> Result<TestType, ParseError> {
    let mapping = node.as_mapping().ok_or_else(|| {
        ParseError::expected_type("mapping", "non-mapping", get_span(node))
            .with_message("Each test must be a mapping")
    })?;

    // Determine test type by checking which field is present
    if mapping.get("python").is_some() {
        let python_node = mapping.get("python").unwrap();
        let python = parse_python_test(python_node.as_mapping().ok_or_else(|| {
            ParseError::expected_type("mapping", "non-mapping", get_span(python_node))
        })?)?;
        Ok(TestType::Python { python })
    } else if mapping.get("perl").is_some() {
        let perl_node = mapping.get("perl").unwrap();
        let perl = parse_perl_test(perl_node.as_mapping().ok_or_else(|| {
            ParseError::expected_type("mapping", "non-mapping", get_span(perl_node))
        })?)?;
        Ok(TestType::Perl { perl })
    } else if mapping.get("r").is_some() {
        let r_node = mapping.get("r").unwrap();
        let r = parse_r_test(r_node.as_mapping().ok_or_else(|| {
            ParseError::expected_type("mapping", "non-mapping", get_span(r_node))
        })?)?;
        Ok(TestType::R { r })
    } else if mapping.get("ruby").is_some() {
        let ruby_node = mapping.get("ruby").unwrap();
        let ruby = parse_ruby_test(ruby_node.as_mapping().ok_or_else(|| {
            ParseError::expected_type("mapping", "non-mapping", get_span(ruby_node))
        })?)?;
        Ok(TestType::Ruby { ruby })
    } else if mapping.get("script").is_some() {
        Ok(TestType::Commands(parse_commands_test(mapping)?))
    } else if mapping.get("downstream").is_some() {
        Ok(TestType::Downstream(parse_downstream_test(mapping)?))
    } else if mapping.get("package_contents").is_some() {
        let package_contents_node = mapping.get("package_contents").unwrap();
        let package_contents =
            parse_package_contents_test(package_contents_node.as_mapping().ok_or_else(|| {
                ParseError::expected_type("mapping", "non-mapping", get_span(package_contents_node))
            })?)?;
        Ok(TestType::PackageContents { package_contents })
    } else {
        Err(ParseError::invalid_value(
            "test",
            "missing test type field (python, perl, r, ruby, script, downstream, package_contents)",
            get_span(node),
        ))
    }
}

fn parse_python_test(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<PythonTest, ParseError> {
    let parser = MappingParser::new(
        mapping,
        "python test",
        &["imports", "pip_check", "python_version"],
    );

    let result = PythonTest {
        imports: parser
            .custom("imports", parse_conditional_list_or_item)?
            .unwrap_or_default(),
        pip_check: parser.optional("pip_check")?,
        python_version: parser.custom("python_version", parse_python_version)?,
    };

    parser.finish()?;
    Ok(result)
}

fn parse_python_version(node: &Node) -> Result<PythonVersion, ParseError> {
    if node.as_sequence().is_some() {
        // Multiple versions (with if/then/else support)
        let versions = parse_conditional_list(node)?;
        Ok(PythonVersion::Multiple(versions))
    } else {
        // Single version
        Ok(PythonVersion::Single(parse_value(node)?))
    }
}

fn parse_perl_test(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<PerlTest, ParseError> {
    let parser = MappingParser::new(mapping, "perl test", &["uses"]);
    let result = PerlTest {
        uses: parser.optional_list("uses")?,
    };
    parser.finish()?;
    Ok(result)
}

fn parse_r_test(mapping: &marked_yaml::types::MarkedMappingNode) -> Result<RTest, ParseError> {
    let parser = MappingParser::new(mapping, "r test", &["libraries"]);
    let result = RTest {
        libraries: parser.optional_list("libraries")?,
    };
    parser.finish()?;
    Ok(result)
}

fn parse_ruby_test(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<RubyTest, ParseError> {
    let parser = MappingParser::new(mapping, "ruby test", &["requires"]);
    let result = RubyTest {
        requires: parser.optional_list("requires")?,
    };
    parser.finish()?;
    Ok(result)
}

fn parse_commands_test(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<CommandsTest, ParseError> {
    let parser = MappingParser::new(
        mapping,
        "commands test",
        &["script", "requirements", "files"],
    );

    let result = CommandsTest {
        script: parser
            .custom("script", crate::stage0::parser::build::parse_script)?
            .unwrap_or_default(),
        requirements: parser.custom("requirements", |n| {
            let m = n
                .as_mapping()
                .ok_or_else(|| ParseError::expected_type("mapping", "non-mapping", get_span(n)))?;
            parse_commands_test_requirements(m)
        })?,
        files: parser.custom("files", |n| {
            let m = n
                .as_mapping()
                .ok_or_else(|| ParseError::expected_type("mapping", "non-mapping", get_span(n)))?;
            parse_commands_test_files(m)
        })?,
    };

    parser.finish()?;
    Ok(result)
}

fn parse_commands_test_requirements(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<CommandsTestRequirements, ParseError> {
    let parser = MappingParser::new(mapping, "commands test requirements", &["run", "build"]);

    let result = CommandsTestRequirements {
        run: parser.optional_list("run")?,
        build: parser.optional_list("build")?,
    };

    parser.finish()?;
    Ok(result)
}

fn parse_commands_test_files(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<CommandsTestFiles, ParseError> {
    let parser = MappingParser::new(mapping, "commands test files", &["source", "recipe"]);

    let result = CommandsTestFiles {
        source: parser.optional_list("source")?,
        recipe: parser.optional_list("recipe")?,
    };

    parser.finish()?;
    Ok(result)
}

fn parse_downstream_test(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<DownstreamTest, ParseError> {
    let parser = MappingParser::new(mapping, "downstream test", &["downstream"]);

    let downstream = parser.required("downstream")?;

    parser.finish()?;
    Ok(DownstreamTest { downstream })
}

fn parse_package_contents_test(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<PackageContentsTest, ParseError> {
    let parser = MappingParser::new(
        mapping,
        "package_contents test",
        &["files", "site_packages", "bin", "lib", "include", "strict"],
    );

    let result = PackageContentsTest {
        files: parser.custom("files", parse_package_contents_check_files_flexible)?,
        site_packages: parser
            .custom("site_packages", parse_package_contents_check_files_flexible)?,
        bin: parser.custom("bin", parse_package_contents_check_files_flexible)?,
        lib: parser.custom("lib", parse_package_contents_check_files_flexible)?,
        include: parser.custom("include", parse_package_contents_check_files_flexible)?,
        strict: parser
            .custom("strict", |n| {
                let scalar = n.as_scalar().ok_or_else(|| {
                    ParseError::expected_type("scalar", "non-scalar", get_span(n))
                        .with_message("Expected 'strict' to be a boolean")
                })?;
                let s = scalar.as_str();
                let span = *scalar.span();
                match s {
                    "true" | "True" | "yes" | "Yes" => Ok(true),
                    "false" | "False" | "no" | "No" => Ok(false),
                    _ => Err(ParseError::invalid_value(
                        "strict",
                        format!("not a valid boolean value (found '{}')", s),
                        span,
                    )),
                }
            })?
            .unwrap_or(false),
    };

    parser.finish()?;
    Ok(result)
}

/// Parse package contents check files with flexible format support
/// Supports two formats:
/// 1. Shorthand (list): `- foo` means "foo must exist"
/// 2. Explicit (mapping): `exists: [foo]` and/or `not_exists: [bar]`
fn parse_package_contents_check_files_flexible(
    node: &Node,
) -> Result<PackageContentsCheckFiles, ParseError> {
    // Try to parse as a mapping first (explicit format with exists/not_exists)
    if let Some(mapping) = node.as_mapping() {
        return parse_package_contents_check_files(mapping);
    }

    // Try to parse as a sequence (shorthand format - simple list means "exists")
    if node.as_sequence().is_some() {
        let exists = parse_conditional_list(node)?;
        return Ok(PackageContentsCheckFiles {
            exists,
            not_exists: ConditionalList::default(),
        });
    }

    Err(ParseError::expected_type(
        "sequence or mapping with exists/not_exists",
        "other",
        get_span(node),
    )
    .with_message(
        "package_contents field must be either a list of files (shorthand) or a mapping with exists/not_exists keys",
    ))
}

fn parse_package_contents_check_files(
    mapping: &marked_yaml::types::MarkedMappingNode,
) -> Result<PackageContentsCheckFiles, ParseError> {
    let parser = MappingParser::new(
        mapping,
        "package_contents check files",
        &["exists", "not_exists"],
    );

    let result = PackageContentsCheckFiles {
        exists: parser.optional_list("exists")?,
        not_exists: parser.optional_list("not_exists")?,
    };

    parser.finish()?;
    Ok(result)
}
