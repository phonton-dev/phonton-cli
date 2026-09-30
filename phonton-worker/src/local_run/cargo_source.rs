//! Conservative Cargo test source-inclusion evidence.
//!
//! Cargo's test summary proves that some tests ran, not that an edited module
//! was compiled. Match each test binary Cargo reports running to its rustc
//! dep-info file, then intersect dependencies with active module paths from
//! that binary's package root. Ambiguous or missing evidence contributes no
//! coverage; macro dependencies such as `include_str!` and `include!` are never
//! module edges.

use super::LocalCheck;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use syn::{parse::Parser, punctuated::Punctuated, Expr, Item, Lit, Meta, Token};

const MAX_ARTIFACTS: usize = 32;
const MAX_DEP_INFO_BYTES: u64 = 1024 * 1024;

pub(super) fn compiled_sources(
    directory: &Path,
    stderr: &str,
    check: &LocalCheck,
) -> BTreeSet<PathBuf> {
    let mut sources = BTreeSet::new();
    let Ok(root) = directory.canonicalize() else {
        return sources;
    };
    let Some(manifest_root) = selected_manifest_root(&root, check) else {
        return sources;
    };
    let Some(packages) = selected_packages(check) else {
        return sources;
    };
    if manifest_root.is_none() && packages.is_empty() {
        return sources;
    }
    for line in stderr
        .lines()
        .take(4096)
        .filter(|line| line.trim_start().starts_with("Running "))
        .take(MAX_ARTIFACTS)
    {
        let Some((description, artifact)) = line
            .trim()
            .strip_prefix("Running ")
            .and_then(|line| line.rsplit_once(" ("))
        else {
            continue;
        };
        let Some(artifact) = artifact.strip_suffix(')') else {
            continue;
        };
        let source = description
            .strip_prefix("unittests ")
            .unwrap_or(description);
        if !source.ends_with(".rs") {
            continue;
        }
        let source_candidates = if let Some(manifest_root) = &manifest_root {
            vec![manifest_root.join(source), root.join(source)]
        } else {
            vec![root.join(source)]
        };
        let source_candidates: BTreeSet<_> = source_candidates
            .into_iter()
            .filter_map(|path| path.canonicalize().ok())
            .filter(|path| {
                path.starts_with(&root)
                    && path.is_file()
                    && manifest_root
                        .as_ref()
                        .is_none_or(|manifest| path.starts_with(manifest))
            })
            .collect();
        if source_candidates.len() != 1 {
            continue;
        }
        let Some(source) = source_candidates.into_iter().next() else {
            continue;
        };
        let artifact = root.join(artifact);
        let Ok(artifact) = artifact.canonicalize() else {
            continue;
        };
        if !candidate_target_artifact(&root, &artifact) || !artifact.is_file() {
            continue;
        }
        let dep_info = artifact.with_extension("d");
        let Ok(dep_info) = dep_info.canonicalize() else {
            continue;
        };
        if !candidate_target_artifact(&root, &dep_info)
            || !dep_info.is_file()
            || fs::metadata(&dep_info).map_or(true, |metadata| metadata.len() > MAX_DEP_INFO_BYTES)
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(&dep_info) else {
            continue;
        };
        let Some((declared_target, dependencies)) =
            text.lines().next().and_then(|line| line.split_once(": "))
        else {
            continue;
        };
        if root.join(declared_target).canonicalize().ok().as_deref() != Some(dep_info.as_path()) {
            continue;
        }
        let mut dependencies = dependencies.split_whitespace();
        let Some(root_dependency) = dependencies.next() else {
            continue;
        };
        let root_dependency = Path::new(root_dependency);
        let package_root = if root_dependency.is_absolute() {
            if root_dependency.canonicalize().ok().as_deref() != Some(source.as_path()) {
                continue;
            }
            root.clone()
        } else {
            let Some(package_root) = source
                .ancestors()
                .take_while(|ancestor| ancestor.starts_with(&root))
                .find(|ancestor| {
                    ancestor
                        .join(root_dependency)
                        .canonicalize()
                        .ok()
                        .as_deref()
                        == Some(source.as_path())
                })
            else {
                continue;
            };
            package_root.to_path_buf()
        };
        if manifest_root
            .as_ref()
            .is_some_and(|manifest| manifest != &package_root)
            || !package_selected(&package_root, &packages)
        {
            continue;
        }
        // Bare --features flags can apply to Cargo's current package rather
        // than another workspace member whose tests also ran. Only infer a
        // package's feature set when it owns the selected manifest.
        let owns_selected_manifest = manifest_root
            .as_ref()
            .map_or(package_root == root, |manifest| manifest == &package_root);
        let modules = module_sources(&source, &package_root, check, owns_selected_manifest);
        for dependency in std::iter::once(root_dependency).chain(dependencies.map(Path::new)) {
            let candidate = package_root.join(dependency);
            if let Ok(candidate) = candidate.canonicalize() {
                if candidate.starts_with(&root)
                    && candidate.is_file()
                    && modules.contains(&candidate)
                {
                    sources.insert(candidate);
                }
            }
        }
    }
    sources
}

fn cargo_arguments(check: &LocalCheck) -> &[String] {
    let end = check
        .args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(check.args.len());
    &check.args[..end]
}

fn selected_manifest_root(root: &Path, check: &LocalCheck) -> Option<Option<PathBuf>> {
    let args = cargo_arguments(check);
    let mut selected = None;
    for (index, arg) in args.iter().enumerate() {
        let path = if arg == "--manifest-path" {
            Some(args.get(index + 1)?.as_str())
        } else {
            arg.strip_prefix("--manifest-path=")
        };
        if let Some(path) = path {
            if selected.replace(path).is_some() {
                return None;
            }
        }
    }
    let Some(path) = selected else {
        return Some(None);
    };
    let manifest = root.join(path).canonicalize().ok()?;
    if !manifest.starts_with(root) || !manifest.is_file() || manifest.file_name()? != "Cargo.toml" {
        return None;
    }
    Some(manifest.parent().map(Path::to_path_buf))
}

fn selected_packages(check: &LocalCheck) -> Option<BTreeSet<String>> {
    let args = cargo_arguments(check);
    let mut selected = BTreeSet::new();
    for (index, arg) in args.iter().enumerate() {
        let name = if arg == "--package" || arg == "-p" {
            Some(args.get(index + 1)?.as_str())
        } else {
            arg.strip_prefix("--package=")
                .or_else(|| arg.strip_prefix("-p").filter(|value| !value.is_empty()))
        };
        if let Some(name) = name {
            selected.insert(name.to_owned());
        }
    }
    Some(selected)
}

fn package_selected(package_root: &Path, selected: &BTreeSet<String>) -> bool {
    let Ok(raw) = fs::read_to_string(package_root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&raw) else {
        return false;
    };
    let Some(name) = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
    else {
        return false;
    };
    selected.is_empty() || selected.contains(name)
}

fn candidate_target_artifact(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        relative
            .components()
            .any(|part| part.as_os_str() == "target")
    })
}

fn selected_features(
    package_root: &Path,
    check: &LocalCheck,
    owns_selected_manifest: bool,
) -> BTreeSet<String> {
    let mut selected = BTreeSet::new();
    if !owns_selected_manifest {
        return selected;
    }
    let Ok(raw) = fs::read_to_string(package_root.join("Cargo.toml")) else {
        return selected;
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&raw) else {
        return selected;
    };
    let Some(declared) = manifest.get("features").and_then(toml::Value::as_table) else {
        return selected;
    };
    let args = cargo_arguments(check);
    if args.iter().any(|arg| arg == "--all-features") {
        selected.extend(
            declared
                .keys()
                .filter(|name| name.as_str() != "default")
                .cloned(),
        );
    } else {
        if !args.iter().any(|arg| arg == "--no-default-features") {
            if let Some(defaults) = declared.get("default").and_then(toml::Value::as_array) {
                selected.extend(
                    defaults
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned),
                );
            }
        }
        for (index, arg) in args.iter().enumerate() {
            let value = if arg == "--features" || arg == "-F" {
                args.get(index + 1).map(String::as_str)
            } else {
                arg.strip_prefix("--features=")
                    .or_else(|| arg.strip_prefix("-F").filter(|value| !value.is_empty()))
            };
            if let Some(value) = value {
                selected.extend(
                    value
                        .split([',', ' '])
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                );
            }
        }
        let mut pending: Vec<_> = selected.iter().cloned().collect();
        while let Some(name) = pending.pop() {
            let Some(enables) = declared.get(&name).and_then(toml::Value::as_array) else {
                continue;
            };
            for enabled in enables.iter().filter_map(toml::Value::as_str) {
                if declared.contains_key(enabled) && selected.insert(enabled.to_owned()) {
                    pending.push(enabled.to_owned());
                }
            }
        }
    }
    selected.retain(|name| declared.contains_key(name));
    selected
}

fn cfg_value(meta: &Meta, features: &BTreeSet<String>) -> Option<bool> {
    match meta {
        // A Cargo test target may use harness = false, so `Running` alone
        // does not prove that rustc enabled cfg(test) for this binary.
        Meta::Path(path) if path.is_ident("test") => None,
        Meta::NameValue(value) if value.path.is_ident("feature") => {
            let Expr::Lit(expression) = &value.value else {
                return None;
            };
            let Lit::Str(name) = &expression.lit else {
                return None;
            };
            Some(features.contains(&name.value()))
        }
        Meta::List(list)
            if list.path.is_ident("all")
                || list.path.is_ident("any")
                || list.path.is_ident("not") =>
        {
            let nested = Punctuated::<Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .ok()?;
            let values: Vec<_> = nested
                .iter()
                .map(|item| cfg_value(item, features))
                .collect();
            if list.path.is_ident("not") {
                // Cargo feature unification and toolchain cfgs are not fully
                // reconstructed here. A negative predicate cannot establish
                // that a module was compiled from a possibly incomplete set.
                return None;
            }
            if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|value| *value == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if values.contains(&Some(true)) {
                Some(true)
            } else if values.iter().all(|value| *value == Some(false)) {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn active(attrs: &[syn::Attribute], features: &BTreeSet<String>) -> bool {
    attrs.iter().all(|attr| {
        if attr.path().is_ident("cfg_attr") {
            return false;
        }
        if attr.path().is_ident("cfg") {
            return attr
                .parse_args::<Meta>()
                .ok()
                .and_then(|meta| cfg_value(&meta, features))
                == Some(true);
        }
        // Attribute macros can replace an outlined module before Rust loads
        // its source file. Only known inert built-ins can establish an edge.
        [
            "path",
            "allow",
            "warn",
            "deny",
            "forbid",
            "doc",
            "macro_use",
            "no_std",
            "no_main",
            "feature",
            "recursion_limit",
            "type_length_limit",
            "crate_name",
            "crate_type",
        ]
        .iter()
        .any(|name| attr.path().is_ident(name))
    })
}

fn module_sources(
    root_source: &Path,
    package_root: &Path,
    check: &LocalCheck,
    owns_selected_manifest: bool,
) -> BTreeSet<PathBuf> {
    let mut visited = BTreeSet::new();
    let features = selected_features(package_root, check, owns_selected_manifest);
    // Every Cargo `Running` source is a crate root. Crate roots resolve
    // child modules beside the root file, even for integration tests such as
    // tests/smoke.rs (whose stem is not lib/main/mod).
    let module_dir = root_source.parent().map(Path::to_path_buf);
    if let Some(module_dir) = module_dir {
        visit_file(
            root_source,
            &module_dir,
            package_root,
            &features,
            &mut visited,
        );
    }
    visited
}

fn visit_file(
    source: &Path,
    module_dir: &Path,
    package_root: &Path,
    features: &BTreeSet<String>,
    visited: &mut BTreeSet<PathBuf>,
) {
    if visited.len() >= 1024 || !source.starts_with(package_root) || visited.contains(source) {
        return;
    }
    let Ok(metadata) = fs::metadata(source) else {
        return;
    };
    if metadata.len() > 1024 * 1024 {
        return;
    }
    let Ok(raw) = fs::read_to_string(source) else {
        return;
    };
    let Ok(parsed) = syn::parse_file(&raw) else {
        return;
    };
    if !active(&parsed.attrs, features) {
        return;
    }
    visited.insert(source.to_path_buf());
    visit_items(
        &parsed.items,
        source,
        module_dir,
        false,
        package_root,
        features,
        visited,
    );
}

fn visit_items(
    items: &[Item],
    source: &Path,
    module_dir: &Path,
    inside_inline: bool,
    package_root: &Path,
    features: &BTreeSet<String>,
    visited: &mut BTreeSet<PathBuf>,
) {
    for item in items {
        match item {
            Item::Mod(module) if active(&module.attrs, features) => {
                let name = module.ident.to_string();
                // Raw identifiers map to filenames without `r#`. Skip
                // rather than risk resolving a data-only file by spelling.
                if name.starts_with("r#") {
                    continue;
                }
                if let Some((_, inner)) = &module.content {
                    // A path attribute on the enclosing inline module changes
                    // where its children live. Do not guess that directory.
                    if module.attrs.iter().any(|attr| attr.path().is_ident("path")) {
                        continue;
                    }
                    visit_items(
                        inner,
                        source,
                        &module_dir.join(name),
                        true,
                        package_root,
                        features,
                        visited,
                    );
                    continue;
                }
                let explicit = module
                    .attrs
                    .iter()
                    .find(|attr| attr.path().is_ident("path"));
                let choices = if let Some(attr) = explicit {
                    let Meta::NameValue(value) = &attr.meta else {
                        continue;
                    };
                    let Expr::Lit(expression) = &value.value else {
                        continue;
                    };
                    let Lit::Str(path) = &expression.lit else {
                        continue;
                    };
                    // Rust resolves #[path] from the source file's directory
                    // for an out-of-line module, but from the module directory
                    // when the declaration sits inside an inline module.
                    let Some(base) = (if inside_inline {
                        Some(module_dir)
                    } else {
                        source.parent()
                    }) else {
                        continue;
                    };
                    vec![base.join(path.value())]
                } else {
                    vec![
                        module_dir.join(format!("{name}.rs")),
                        module_dir.join(&name).join("mod.rs"),
                    ]
                };
                let existing: Vec<_> = choices.into_iter().filter(|path| path.is_file()).collect();
                if existing.len() != 1 {
                    continue;
                }
                let Ok(next) = existing[0].canonicalize() else {
                    continue;
                };
                let next_dir = if next.file_name().is_some_and(|name| name == "mod.rs") {
                    next.parent().map(Path::to_path_buf)
                } else {
                    next.file_stem().map(|stem| next.with_file_name(stem))
                };
                if let Some(next_dir) = next_dir {
                    visit_file(&next, &next_dir, package_root, features, visited);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_check() -> LocalCheck {
        LocalCheck {
            program: "cargo".into(),
            args: vec!["test".into()],
        }
    }

    #[test]
    fn path_attribute_uses_the_declaring_file_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/bar")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "mod bar;\n").unwrap();
        fs::write(
            root.join("src/bar.rs"),
            "#[path = \"feature.rs\"] mod feature;\nconst DATA: &str = include_str!(\"bar/feature.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("src/feature.rs"), "pub fn compiled() {}\n").unwrap();
        fs::write(root.join("src/bar/feature.rs"), "pub fn data_only() {}\n").unwrap();

        let sources = module_sources(&root.join("src/lib.rs"), &root, &test_check(), true);
        assert!(sources.contains(&root.join("src/feature.rs").canonicalize().unwrap()));
        assert!(!sources.contains(&root.join("src/bar/feature.rs").canonicalize().unwrap()));
    }

    #[test]
    fn inline_module_path_attribute_uses_the_module_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/bar")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "mod bar { #[path = \"feature.rs\"] mod feature; }\n",
        )
        .unwrap();
        fs::write(root.join("src/feature.rs"), "pub fn data_only() {}\n").unwrap();
        fs::write(root.join("src/bar/feature.rs"), "pub fn compiled() {}\n").unwrap();

        let sources = module_sources(&root.join("src/lib.rs"), &root, &test_check(), true);
        assert!(sources.contains(&root.join("src/bar/feature.rs").canonicalize().unwrap()));
        assert!(!sources.contains(&root.join("src/feature.rs").canonicalize().unwrap()));
    }

    #[test]
    fn integration_test_crate_root_resolves_modules_beside_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("tests/smoke")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/smoke.rs"),
            "mod helper;\nconst DATA: &str = include_str!(\"smoke/helper.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("tests/helper.rs"), "pub fn compiled() {}\n").unwrap();
        fs::write(
            root.join("tests/smoke/helper.rs"),
            "pub fn data_only() {}\n",
        )
        .unwrap();

        let sources = module_sources(&root.join("tests/smoke.rs"), &root, &test_check(), true);
        assert!(sources.contains(&root.join("tests/helper.rs").canonicalize().unwrap()));
        assert!(!sources.contains(&root.join("tests/smoke/helper.rs").canonicalize().unwrap()));
    }

    #[test]
    fn inner_cfg_does_not_credit_data_only_dependency() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/foo")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "mod foo;\nconst DATA: &str = include_str!(\"foo/bar.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("src/foo.rs"), "#![cfg(any())]\nmod bar;\n").unwrap();
        fs::write(root.join("src/foo/bar.rs"), "pub fn data_only() {}\n").unwrap();

        let sources = module_sources(&root.join("src/lib.rs"), &root, &test_check(), true);
        assert!(!sources.contains(&root.join("src/foo/bar.rs").canonicalize().unwrap()));
    }

    #[test]
    fn unknown_attribute_cannot_establish_module_inclusion() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "#[replace_module] mod feature;\nconst DATA: &str = include_str!(\"feature.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("src/feature.rs"), "pub fn data_only() {}\n").unwrap();

        let sources = module_sources(&root.join("src/lib.rs"), &root, &test_check(), true);
        assert!(!sources.contains(&root.join("src/feature.rs").canonicalize().unwrap()));
    }

    #[test]
    fn cfg_test_needs_harness_proof() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n[[test]]\nname = 'smoke'\nharness = false\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/smoke.rs"),
            "#[cfg(test)] mod feature;\nconst DATA: &str = include_str!(\"feature.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("tests/feature.rs"), "pub fn data_only() {}\n").unwrap();

        let sources = module_sources(&root.join("tests/smoke.rs"), &root, &test_check(), true);
        assert!(!sources.contains(&root.join("tests/feature.rs").canonicalize().unwrap()));
    }

    #[test]
    fn include_macro_cannot_establish_source_inclusion() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "macro_rules! include { ($p:literal) => { const _: () = (); } }\ninclude!(\"feature.rs\");\nconst DATA: &str = include_str!(\"feature.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("src/feature.rs"), "pub fn data_only() {}\n").unwrap();

        let sources = module_sources(&root.join("src/lib.rs"), &root, &test_check(), true);
        assert!(!sources.contains(&root.join("src/feature.rs").canonicalize().unwrap()));
    }

    #[test]
    fn raw_module_name_does_not_credit_similarly_spelled_data_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "mod r#type;\nconst DATA: &str = include_str!(\"r#type.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("src/type.rs"), "pub fn compiled() {}\n").unwrap();
        fs::write(root.join("src/r#type.rs"), "pub fn data_only() {}\n").unwrap();

        let sources = module_sources(&root.join("src/lib.rs"), &root, &test_check(), true);
        assert!(!sources.contains(&root.join("src/r#type.rs").canonicalize().unwrap()));
    }

    #[test]
    fn feature_flags_do_not_activate_another_workspace_package() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\n[features]\nprobe = []\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "#[cfg(feature = \"probe\")] mod feature;\nconst DATA: &str = include_str!(\"feature.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("src/feature.rs"), "pub fn data_only() {}\n").unwrap();
        let check = LocalCheck {
            program: "cargo".into(),
            args: vec!["test".into(), "--features".into(), "probe".into()],
        };

        let current = module_sources(&root.join("src/lib.rs"), &root, &check, true);
        assert!(current.contains(&root.join("src/feature.rs").canonicalize().unwrap()));
        let other = module_sources(&root.join("src/lib.rs"), &root, &check, false);
        assert!(!other.contains(&root.join("src/feature.rs").canonicalize().unwrap()));
    }

    #[test]
    fn resolves_nested_package_dep_info_without_attributing_another_package() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let package = root.join("crates/feature");
        fs::create_dir_all(package.join("src")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod feature;\n").unwrap();
        fs::write(root.join("src/feature.rs"), "pub fn wrong_package() {}\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'root-fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(
            package.join("Cargo.toml"),
            "[package]\nname = 'feature-fixture'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(package.join("src/lib.rs"), "mod feature;\n").unwrap();
        fs::write(package.join("src/feature.rs"), "pub fn value() {}\n").unwrap();
        let artifact_dir = package.join("target/debug/deps");
        fs::create_dir_all(&artifact_dir).unwrap();
        let artifact_name = if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        };
        let artifact = artifact_dir.join(artifact_name);
        fs::write(&artifact, b"test executable placeholder").unwrap();
        let dep_info = artifact.with_extension("d");
        fs::write(
            &dep_info,
            format!("{}: src/lib.rs src/feature.rs\n", dep_info.display()),
        )
        .unwrap();
        let stderr = format!(
            "     Running unittests src/lib.rs (crates/feature/target/debug/deps/{artifact_name})\n"
        );
        let check = LocalCheck {
            program: "cargo".into(),
            args: vec![
                "test".into(),
                "--package".into(),
                "feature-fixture".into(),
                "--manifest-path".into(),
                "crates/feature/Cargo.toml".into(),
            ],
        };
        let compiled = compiled_sources(root, &stderr, &check);
        assert!(compiled.contains(&package.join("src/feature.rs").canonicalize().unwrap()));
        assert!(!compiled.contains(&root.join("src/feature.rs").canonicalize().unwrap()));

        fs::write(&dep_info, format!("{}: src/lib.rs\n", dep_info.display())).unwrap();
        assert!(!compiled_sources(root, &stderr, &check)
            .contains(&package.join("src/feature.rs").canonicalize().unwrap()));
    }
}
