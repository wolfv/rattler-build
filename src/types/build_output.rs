use fs_err as fs;
use indicatif::HumanBytes;
use rattler_build_jinja::Variable;
use rattler_build_recipe::{Stage1Recipe, stage1::Source};
use rattler_build_types::NormalizedKey;
use rattler_conda_types::{
    PackageName, Platform, RepoDataRecord, VersionWithSource,
    package::{PathType, PathsEntry, PathsJson},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::{self, Display, Formatter},
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use crate::{
    console_utils::github_integration_enabled,
    render::resolved_dependencies::FinalizedDependencies,
    system_tools::SystemTools,
    types::{BuildConfiguration, BuildSummary, PlatformWithVirtualPackages},
};

/// A output. This is the central element that is passed to the `run_build`
/// function and fully specifies all the options and settings to run the build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOutput {
    /// The rendered recipe that is used to build this output
    pub recipe: Stage1Recipe,
    /// The build configuration for this output (e.g. target_platform, channels,
    /// and other settings)
    pub build_configuration: BuildConfiguration,
    /// The finalized dependencies for this output. If this is `None`, the
    /// dependencies have not been resolved yet. During the `run_build`
    /// functions, the dependencies are resolved and this field is filled.
    pub finalized_dependencies: Option<FinalizedDependencies>,
    /// The finalized sources for this output. Contain the exact git hashes for
    /// the sources that are used to build this output.
    pub finalized_sources: Option<Vec<Source>>,

    /// The finalized dependencies from the cache (if there is a cache
    /// instruction)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finalized_cache_dependencies: Option<FinalizedDependencies>,
    /// The finalized sources from the cache (if there is a cache instruction)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finalized_cache_sources: Option<Vec<Source>>,

    /// Summary of the build
    #[serde(skip)]
    pub build_summary: Arc<Mutex<BuildSummary>>,
    /// The system tools that are used to build this output
    pub system_tools: SystemTools,
    /// Some extra metadata that should be recorded additionally in about.json
    /// Usually it is used during the CI build to record link to the CI job
    /// that created this artifact
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_meta: Option<BTreeMap<String, Value>>,
}

impl BuildOutput {
    /// The name of the package
    pub fn name(&self) -> &PackageName {
        self.recipe.package().name()
    }

    /// The version of the package
    pub fn version(&self) -> &VersionWithSource {
        self.recipe.package().version()
    }

    /// The build string from the recipe (always present after evaluation)
    pub fn build_string(&self) -> Cow<'_, str> {
        self.recipe.build().string.as_ref().into()
    }

    /// retrieve an identifier for this output ({name}-{version}-{build_string})
    pub fn identifier(&self) -> String {
        format!(
            "{}-{}-{}",
            self.name().as_normalized(),
            self.version(),
            &self.build_string()
        )
    }

    /// Record a warning during the build
    pub fn record_warning(&self, warning: &str) {
        self.build_summary
            .lock()
            .unwrap()
            .warnings
            .push(warning.to_string());
    }

    /// Record the start of the build
    pub fn record_build_start(&self) {
        self.build_summary.lock().unwrap().build_start = Some(chrono::Utc::now());
    }

    /// Record the artifact that was created during the build
    pub fn record_artifact(&self, artifact: &Path, paths: &PathsJson) {
        let mut summary = self.build_summary.lock().unwrap();
        summary.artifact = Some(artifact.to_path_buf());
        summary.paths = Some(paths.clone());
    }

    /// Record the end of the build
    pub fn record_build_end(&self) {
        let mut summary = self.build_summary.lock().unwrap();
        summary.build_end = Some(chrono::Utc::now());
    }

    /// Shorthand to retrieve the variant configuration for this output
    pub fn variant(&self) -> &BTreeMap<NormalizedKey, Variable> {
        &self.build_configuration.variant
    }

    /// Shorthand to retrieve the host prefix for this output
    pub fn prefix(&self) -> &Path {
        &self.build_configuration.directories.host_prefix
    }

    /// Shorthand to retrieve the build prefix for this output
    pub fn build_prefix(&self) -> &Path {
        &self.build_configuration.directories.build_prefix
    }

    /// Shorthand to retrieve the target platform for this output
    pub fn target_platform(&self) -> &Platform {
        &self.build_configuration.target_platform
    }

    /// Shorthand to retrieve the target platform for this output
    pub fn host_platform(&self) -> &PlatformWithVirtualPackages {
        &self.build_configuration.host_platform
    }

    /// Search for the resolved package with the given name in the host prefix
    /// Returns a tuple of the package and a boolean indicating whether the
    /// package is directly requested
    pub fn find_resolved_package(&self, name: &str) -> Option<(&RepoDataRecord, bool)> {
        let host = self.finalized_dependencies.as_ref()?.host.as_ref()?;
        let record = host
            .resolved
            .iter()
            .find(|p| p.package_record.name.as_normalized() == name);

        let is_requested = host.specs.iter().any(|s| {
            s.spec()
                .name
                .as_ref()
                .map(|n| n.to_string() == name)
                .unwrap_or(false)
        });

        record.map(|r| (r, is_requested))
    }

    /// Print a nice summary of the build
    pub fn log_build_summary(&self) -> Result<(), std::io::Error> {
        let summary = self.build_summary.lock().unwrap();
        let identifier = self.identifier();
        let span = tracing::info_span!(
            "Build summary for",
            recipe = identifier,
            span_color = identifier
        );
        let _enter = span.enter();

        tracing::info!("{}", self);

        if !summary.warnings.is_empty() {
            tracing::warn!("Warnings:");
            for warning in &summary.warnings {
                tracing::warn!("{}", warning);
            }
        }

        if let Some(artifact) = &summary.artifact {
            let bytes = HumanBytes(fs::metadata(artifact).map(|m| m.len()).unwrap_or(0));
            tracing::info!("Artifact: {} ({})", artifact.display(), bytes);
        } else {
            tracing::info!("No artifact was created");
        }

        if let Ok(github_summary) = std::env::var("GITHUB_STEP_SUMMARY") {
            if !github_integration_enabled() {
                return Ok(());
            }
            // append to the summary file
            let mut summary_file = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(github_summary)?;

            writeln!(summary_file, "### Build summary for {}", identifier)?;
            if let Some(article) = &summary.artifact {
                let bytes = HumanBytes(fs::metadata(article).map(|m| m.len()).unwrap_or(0));
                writeln!(
                    summary_file,
                    "**Artifact**: {} ({})",
                    article.display(),
                    bytes
                )?;
            } else {
                writeln!(summary_file, "**No artifact was created**")?;
            }

            if let Some(paths) = &summary.paths {
                if paths.paths.is_empty() {
                    writeln!(summary_file, "Included files: **No files included**")?;
                } else {
                    /// Github detail expander
                    fn format_entry(entry: &PathsEntry) -> String {
                        let mut extra_info = Vec::new();
                        if entry.prefix_placeholder.is_some() {
                            extra_info.push("contains prefix");
                        }
                        if entry.no_link {
                            extra_info.push("no link");
                        }
                        match entry.path_type {
                            PathType::SoftLink => extra_info.push("soft link"),
                            // skip default
                            PathType::HardLink => {}
                            PathType::Directory => extra_info.push("directory"),
                        }
                        let bytes = entry.size_in_bytes.unwrap_or(0);

                        format!(
                            "| `{}` | {} | {} |",
                            entry.relative_path.to_string_lossy(),
                            HumanBytes(bytes),
                            extra_info.join(", ")
                        )
                    }

                    writeln!(summary_file, "<details>")?;
                    writeln!(
                        summary_file,
                        "<summary>Included files ({} files)</summary>\n",
                        paths.paths.len()
                    )?;
                    writeln!(summary_file, "| Path | Size | Extra info |")?;
                    writeln!(summary_file, "| --- | --- | --- |")?;
                    for path in &paths.paths {
                        writeln!(summary_file, "{}", format_entry(path))?;
                    }
                    writeln!(summary_file, "\n</details>\n")?;
                }
            }

            if !summary.warnings.is_empty() {
                writeln!(summary_file, "> [!WARNING]")?;
                writeln!(summary_file, "> **Warnings during build:**\n>")?;
                for warning in &summary.warnings {
                    writeln!(summary_file, "> - {}", warning)?;
                }
                writeln!(summary_file)?;
            }

            writeln!(
                summary_file,
                "<details><summary>Resolved dependencies</summary>\n\n{}\n</details>\n",
                self.format_as_markdown()
            )?;
        }
        Ok(())
    }

    /// Format the output as a markdown table
    pub fn format_as_markdown(&self) -> String {
        let mut output = String::new();
        self.format_table_with_option(&mut output, comfy_table::presets::ASCII_MARKDOWN, true)
            .expect("Could not format table");
        output
    }

    fn format_table_with_option(
        &self,
        f: &mut impl fmt::Write,
        table_format: &str,
        long: bool,
    ) -> std::fmt::Result {
        let template = || -> comfy_table::Table {
            let mut table = comfy_table::Table::new();
            if table_format == comfy_table::presets::UTF8_FULL {
                table
                    .load_preset(comfy_table::presets::UTF8_FULL_CONDENSED)
                    .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
            } else {
                table.load_preset(table_format);
            }
            table
        };

        writeln!(f, "Variant configuration (hash: {}):", self.build_string())?;
        let mut table = template();
        if table_format != comfy_table::presets::UTF8_FULL {
            table.set_header(["Key", "Value"]);
        }
        self.build_configuration.variant.iter().for_each(|(k, v)| {
            table.add_row([k.normalize(), format!("{:?}", v)]);
        });
        writeln!(f, "{}\n", table)?;

        if let Some(finalized_dependencies) = &self.finalized_dependencies {
            if let Some(build) = &finalized_dependencies.build {
                writeln!(f, "Build dependencies:")?;
                writeln!(f, "{}\n", build.to_table(template(), long))?;
            }

            if let Some(host) = &finalized_dependencies.host {
                writeln!(f, "Host dependencies:")?;
                writeln!(f, "{}\n", host.to_table(template(), long))?;
            }

            if !finalized_dependencies.run.depends.is_empty() {
                writeln!(f, "Run dependencies:")?;
                writeln!(
                    f,
                    "{}\n",
                    finalized_dependencies.run.to_table(template(), long)
                )?;
            }
        }

        Ok(())
    }

    /// Check if this package is python version independent (ABI3 or noarch) package
    pub(crate) fn is_python_version_independent(&self) -> bool {
        self.recipe.build.python.version_independent
            || self
                .recipe
                .build
                .noarch
                .map(|n| n.is_python())
                .unwrap_or(false)
    }

    /// Create an Output for a subpackage.
    ///
    /// This creates a modified Output with:
    /// - Recipe's package info from the subpackage
    /// - Recipe's about info from the subpackage (inheriting from parent if not specified)
    /// - Recipe's tests from the subpackage
    /// - Build configuration inherited from parent (same directories, platform, etc.)
    /// - Finalized dependencies with subpackage's run requirements
    pub fn for_subpackage(
        &self,
        subpackage: &rattler_build_recipe::stage1::SubPackage,
    ) -> Self {
        use rattler_build_recipe::stage1::{About, Build, Requirements};
        use crate::render::resolved_dependencies::{
            DependencyInfo, FinalizedRunDependencies, SourceDependency,
        };

        // Create a modified recipe for the subpackage
        let mut subpackage_recipe = self.recipe.clone();

        // Replace package info
        subpackage_recipe.package = subpackage.package.clone();

        // Replace about info (subpackage's about, with inheritance from parent)
        let parent_about = &self.recipe.about;
        subpackage_recipe.about = About {
            homepage: subpackage.about.homepage.clone().or_else(|| parent_about.homepage.clone()),
            repository: subpackage.about.repository.clone().or_else(|| parent_about.repository.clone()),
            documentation: subpackage.about.documentation.clone().or_else(|| parent_about.documentation.clone()),
            license: subpackage.about.license.clone().or_else(|| parent_about.license.clone()),
            license_file: subpackage
                .about
                .license_file
                .clone()
                .or_else(|| parent_about.license_file.clone()),
            license_family: subpackage.about.license_family.clone().or_else(|| parent_about.license_family.clone()),
            summary: subpackage.about.summary.clone().or_else(|| parent_about.summary.clone()),
            description: subpackage.about.description.clone().or_else(|| parent_about.description.clone()),
        };

        // Replace tests
        subpackage_recipe.tests = subpackage.tests.clone();

        // Modify build section for subpackage
        // Keep most settings from parent but update subpackage-specific ones
        let parent_build = &self.recipe.build;
        subpackage_recipe.build = Build {
            // Inherit from parent
            number: parent_build.number,
            string: parent_build.string.clone(),
            skip: parent_build.skip.clone(),
            script: parent_build.script.clone(),
            always_copy_files: parent_build.always_copy_files.clone(),
            always_include_files: parent_build.always_include_files.clone(),
            merge_build_and_host_envs: parent_build.merge_build_and_host_envs,
            variant: parent_build.variant.clone(),
            // Override from subpackage
            noarch: subpackage.build.noarch.or(parent_build.noarch),
            python: if subpackage.build.python.is_default() {
                parent_build.python.clone()
            } else {
                subpackage.build.python.clone()
            },
            files: subpackage.build.files.clone(),
            dynamic_linking: if subpackage.build.dynamic_linking.is_default() {
                parent_build.dynamic_linking.clone()
            } else {
                subpackage.build.dynamic_linking.clone()
            },
            prefix_detection: if subpackage.build.prefix_detection.is_default() {
                parent_build.prefix_detection.clone()
            } else {
                subpackage.build.prefix_detection.clone()
            },
            post_process: if subpackage.build.post_process.is_empty() {
                parent_build.post_process.clone()
            } else {
                subpackage.build.post_process.clone()
            },
        };

        // Clear sub_packages since subpackages don't have nested subpackages
        subpackage_recipe.sub_packages = Vec::new();

        // Create modified finalized dependencies for subpackage
        // Keep build/host from parent, but use subpackage's run requirements
        let finalized_dependencies = self.finalized_dependencies.as_ref().map(|deps| {
            // Convert subpackage's run requirements to DependencyInfo
            let subpackage_run_deps: Vec<DependencyInfo> = subpackage
                .requirements
                .run
                .iter()
                .filter_map(|dep| {
                    match dep {
                        rattler_build_recipe::stage1::Dependency::Spec(spec) => {
                            Some(SourceDependency { spec: *spec.clone() }.into())
                        }
                        rattler_build_recipe::stage1::Dependency::PinSubpackage(pin) => {
                            // Resolve pin_subpackage using build_configuration.subpackages
                            if let Some(subpkg_info) = self.build_configuration.subpackages.get(&pin.pin_subpackage.name) {
                                match pin.pin_subpackage.apply(&subpkg_info.version, &subpkg_info.build_string) {
                                    Ok(spec) => Some(
                                        crate::render::resolved_dependencies::PinSubpackageDependency {
                                            spec,
                                            name: pin.pin_subpackage.name.as_normalized().to_string(),
                                            args: pin.pin_subpackage.args.clone(),
                                        }
                                        .into(),
                                    ),
                                    Err(e) => {
                                        tracing::warn!(
                                            "Failed to apply pin_subpackage for {}: {}",
                                            pin.pin_subpackage.name.as_normalized(),
                                            e
                                        );
                                        None
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "pin_subpackage references unknown package: {}",
                                    pin.pin_subpackage.name.as_normalized()
                                );
                                None
                            }
                        }
                        rattler_build_recipe::stage1::Dependency::PinCompatible(pin) => {
                            // pin_compatible needs to be resolved against host packages
                            // For now, just log a warning - this needs more work to properly resolve
                            tracing::warn!(
                                "pin_compatible in subpackage requirements not fully supported: {}",
                                pin.pin_compatible.name.as_normalized()
                            );
                            None
                        }
                    }
                })
                .collect();

            // Convert subpackage's run_constraints to DependencyInfo
            let subpackage_run_constraints: Vec<DependencyInfo> = subpackage
                .requirements
                .run_constraints
                .iter()
                .filter_map(|dep| {
                    match dep {
                        rattler_build_recipe::stage1::Dependency::Spec(spec) => {
                            Some(SourceDependency { spec: *spec.clone() }.into())
                        }
                        _ => None,
                    }
                })
                .collect();

            FinalizedDependencies {
                build: deps.build.clone(),
                host: deps.host.clone(),
                run: FinalizedRunDependencies {
                    depends: subpackage_run_deps,
                    constraints: subpackage_run_constraints,
                    run_exports: Default::default(), // Subpackages can have their own run_exports
                },
            }
        });

        // Modify requirements section to use subpackage's requirements
        subpackage_recipe.requirements = Requirements {
            build: self.recipe.requirements.build.clone(),
            host: self.recipe.requirements.host.clone(),
            run: subpackage.requirements.run.clone(),
            run_constraints: subpackage.requirements.run_constraints.clone(),
            run_exports: subpackage.requirements.run_exports.clone(),
            ignore_run_exports: subpackage.requirements.ignore_run_exports.clone(),
        };

        Self {
            recipe: subpackage_recipe,
            build_configuration: self.build_configuration.clone(),
            finalized_dependencies,
            finalized_sources: self.finalized_sources.clone(),
            finalized_cache_dependencies: self.finalized_cache_dependencies.clone(),
            finalized_cache_sources: self.finalized_cache_sources.clone(),
            build_summary: Arc::new(Mutex::new(BuildSummary::default())),
            system_tools: self.system_tools.clone(),
            extra_meta: self.extra_meta.clone(),
        }
    }
}

impl Display for BuildOutput {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.format_table_with_option(f, comfy_table::presets::UTF8_FULL, false)
    }
}

impl crate::post_process::path_checks::WarningRecorder for BuildOutput {
    fn record_warning(&self, warning: &str) {
        self.record_warning(warning);
    }
}
