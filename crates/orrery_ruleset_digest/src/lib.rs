//! The one D49 source-closure encoder used to generate `RulesetId.digest`.
//!
//! This crate intentionally lives only on `orrery_games`' build-dependency
//! path. It asks Cargo for the resolved normal-dependency closure, then hashes
//! normalized production Rust sources from its first-party members.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use cargo_metadata::{DependencyKind, Metadata, MetadataCommand, Package};
use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};
use quote::ToTokens;

const DOMAIN: &[u8] = b"orrery-ruleset-digest-v1\0";

/// A result produced by the source-closure encoder.
pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// A metadata-derived D49 source closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestClosure {
    manifest_path: PathBuf,
    packages: Vec<ContributingCrate>,
    rerun_inputs: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContributingCrate {
    name: String,
    manifest_path: PathBuf,
    sources: Vec<SourceUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SourceUnit {
    logical_path: PathBuf,
    disk_path: PathBuf,
}

impl DigestClosure {
    /// Resolve the normal first-party dependency closure rooted at `manifest`.
    ///
    /// Metadata is deliberately locked and offline: a build identity cannot
    /// depend on a network lookup or on Cargo silently selecting a new graph.
    pub fn from_manifest(manifest: &Path) -> Result<Self> {
        let manifest_path = manifest.canonicalize()?;
        let metadata = metadata_for(&manifest_path)?;
        let packages = contributing_crates(&metadata, &manifest_path)?;
        let mut rerun_inputs = BTreeSet::new();
        for package in &packages {
            rerun_inputs.insert(package.manifest_path.clone());
            rerun_inputs.extend(
                package
                    .sources
                    .iter()
                    .map(|source| source.disk_path.clone()),
            );
        }
        let lockfile = metadata.workspace_root.as_std_path().join("Cargo.lock");
        if lockfile.is_file() {
            rerun_inputs.insert(lockfile);
        }

        Ok(Self {
            manifest_path,
            packages,
            rerun_inputs,
        })
    }

    /// Check that the encoded sources and Cargo rerun inputs still exactly
    /// match a fresh Cargo-metadata closure.
    ///
    /// This is intentionally independent from digest calculation. A future
    /// accidental omission cannot yield a plausible old digest: the build
    /// script stops before generating its Rust constant.
    pub fn verify_metadata_closure(&self) -> Result<()> {
        let fresh = Self::from_manifest(&self.manifest_path)?;
        if self.packages != fresh.packages || self.rerun_inputs != fresh.rerun_inputs {
            return Err("metadata closure and encoded/rerun input sets differ".into());
        }
        Ok(())
    }

    /// Every file Cargo must watch for this computed build identity.
    pub fn rerun_inputs(&self) -> impl Iterator<Item = &Path> {
        self.rerun_inputs.iter().map(PathBuf::as_path)
    }

    /// Compute blake3 over the versioned, length-prefixed closure encoding.
    pub fn digest(&self) -> Result<[u8; 32]> {
        let mut hasher = blake3::Hasher::new();
        write_record(&mut hasher, b"domain", DOMAIN);
        for package in &self.packages {
            write_record(&mut hasher, b"crate", package.name.as_bytes());
            for source in &package.sources {
                write_record(
                    &mut hasher,
                    b"path",
                    source.logical_path.to_string_lossy().as_bytes(),
                );
                let text = fs::read_to_string(&source.disk_path)?;
                write_record(&mut hasher, b"tokens", canonical_tokens(&text)?.as_bytes());
            }
        }
        Ok(*hasher.finalize().as_bytes())
    }
}

/// Generate the Rust constant and Cargo rerun directives for the package
/// whose build script calls this function.
///
/// The helper is shared so every build-script user has the same metadata
/// validation, source encoder, and fail-closed output path.
pub fn generate_build_output() -> Result<()> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing CARGO_MANIFEST_DIR")?);
    let closure = DigestClosure::from_manifest(&manifest_dir.join("Cargo.toml"))?;
    closure.verify_metadata_closure()?;
    for input in closure.rerun_inputs() {
        println!("cargo:rerun-if-changed={}", input.display());
    }

    let digest = closure.digest()?;
    let output_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    fs::write(
        output_dir.join("ruleset_digest.rs"),
        format!("pub const RULESET_DIGEST: [u8; 32] = {digest:?};\n"),
    )?;
    Ok(())
}

fn metadata_for(manifest: &Path) -> Result<Metadata> {
    MetadataCommand::new()
        .manifest_path(manifest)
        .other_options(vec!["--locked".into(), "--offline".into()])
        .exec()
        .map_err(Into::into)
}

fn contributing_crates(metadata: &Metadata, manifest: &Path) -> Result<Vec<ContributingCrate>> {
    let root = metadata
        .packages
        .iter()
        .find(|package| package.manifest_path.as_std_path() == manifest)
        .ok_or("the digest manifest is absent from cargo metadata")?;
    let workspace_members: BTreeSet<_> = metadata.workspace_members.iter().cloned().collect();
    let packages: HashMap<_, _> = metadata
        .packages
        .iter()
        .map(|package| (package.id.clone(), package))
        .collect();
    let nodes: HashMap<_, _> = metadata
        .resolve
        .as_ref()
        .ok_or("cargo metadata did not return a resolve graph")?
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node))
        .collect();

    let mut pending = VecDeque::from([root.id.clone()]);
    let mut selected = BTreeSet::new();
    while let Some(package_id) = pending.pop_front() {
        if !workspace_members.contains(&package_id) || !selected.insert(package_id.clone()) {
            continue;
        }
        let node = nodes
            .get(&package_id)
            .ok_or("workspace package is absent from the resolve graph")?;
        for dependency in &node.deps {
            if dependency
                .dep_kinds
                .iter()
                .any(|kind| kind.kind == DependencyKind::Normal)
                && workspace_members.contains(&dependency.pkg)
            {
                pending.push_back(dependency.pkg.clone());
            }
        }
    }

    let mut closure = selected
        .iter()
        .map(|id| {
            package_inputs(
                packages
                    .get(id)
                    .copied()
                    .ok_or("resolve package missing metadata")?,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    closure.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(closure)
}

fn package_inputs(package: &Package) -> Result<ContributingCrate> {
    let manifest_path = package.manifest_path.as_std_path().to_path_buf();
    let source_root = manifest_path
        .parent()
        .ok_or("package manifest has no parent")?
        .join("src");
    let mut source_paths = Vec::new();
    collect_rust_sources(&source_root, &mut source_paths)?;
    source_paths.sort();
    let sources = source_paths
        .into_iter()
        .map(|disk_path| {
            let logical_path = disk_path
                .strip_prefix(manifest_path.parent().expect("manifest parent checked"))?
                .to_path_buf();
            Ok(SourceUnit {
                logical_path,
                disk_path,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ContributingCrate {
        name: package.name.to_string(),
        manifest_path,
        sources,
    })
}

fn collect_rust_sources(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_rust_sources(&path, output)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
    Ok(())
}

fn write_record(hasher: &mut blake3::Hasher, tag: &[u8], bytes: &[u8]) {
    hasher.update(&(tag.len() as u64).to_le_bytes());
    hasher.update(tag);
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn canonical_tokens(source: &str) -> Result<String> {
    let mut file = syn::parse_file(source)?;
    strip_test_items(&mut file.items);
    Ok(strip_doc_attributes(file.into_token_stream()).to_string())
}

fn strip_test_items(items: &mut Vec<syn::Item>) {
    items.retain(|item| !item_is_cfg_test(item));
    for item in items {
        if let syn::Item::Mod(module) = item {
            if let Some((_, nested)) = &mut module.content {
                strip_test_items(nested);
            }
        }
    }
}

fn item_is_cfg_test(item: &syn::Item) -> bool {
    let attributes = match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        syn::Item::Verbatim(_) => return false,
        _ => return false,
    };
    attributes.iter().any(is_cfg_test)
}

fn is_cfg_test(attribute: &syn::Attribute) -> bool {
    attribute.path().is_ident("cfg")
        && attribute
            .parse_args::<syn::Path>()
            .is_ok_and(|path| path.is_ident("test"))
}

fn strip_doc_attributes(tokens: TokenStream) -> TokenStream {
    let mut output = TokenStream::new();
    let mut input = tokens.into_iter().peekable();
    while let Some(token) = input.next() {
        if let TokenTree::Punct(punctuation) = &token {
            if punctuation.as_char() == '#' {
                if let Some(TokenTree::Group(group)) = input.peek() {
                    if group.delimiter() == Delimiter::Bracket && is_doc_attribute(group) {
                        input.next();
                        continue;
                    }
                }
            }
        }
        output.extend([match token {
            TokenTree::Group(group) => TokenTree::Group(Group::new(
                group.delimiter(),
                strip_doc_attributes(group.stream()),
            )),
            other => other,
        }]);
    }
    output
}

fn is_doc_attribute(group: &Group) -> bool {
    let mut tokens = group.stream().into_iter();
    matches!(
        (tokens.next(), tokens.next()),
        (Some(TokenTree::Ident(name)), Some(TokenTree::Punct(equals)))
            if name == "doc" && equals.as_char() == '='
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tempfile::TempDir;

    use super::DigestClosure;

    fn fixture() -> TempDir {
        let directory = tempfile::tempdir().expect("temporary workspace");
        write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\nmembers = [\"games\", \"core\"]\n",
        );
        write(
            directory.path().join("games/Cargo.toml"),
            "[package]\nname = \"orrery_games\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\norrery_core = { path = \"../core\" }\n",
        );
        write(
            directory.path().join("core/Cargo.toml"),
            "[package]\nname = \"orrery_core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            directory.path().join("games/src/lib.rs"),
            "pub const RULE: u32 = 7;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 3; }\n",
        );
        write(
            directory.path().join("core/src/lib.rs"),
            "pub const CORE: u32 = 1;\n",
        );
        write(
            directory.path().join("games/tests/rules.rs"),
            "#[test] fn integration() {}\n",
        );
        let status = Command::new(env!("CARGO"))
            .args(["generate-lockfile", "--offline"])
            .current_dir(directory.path())
            .status()
            .expect("run cargo generate-lockfile");
        assert!(status.success(), "fixture lockfile generation failed");
        directory
    }

    fn digest(directory: &TempDir) -> [u8; 32] {
        let closure = DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml"))
            .expect("derive metadata closure");
        closure
            .verify_metadata_closure()
            .expect("metadata closure check");
        closure.digest().expect("hash closure")
    }

    fn write(path: PathBuf, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, contents).expect("write fixture source");
    }

    #[test]
    fn metadata_closure_and_rerun_inputs_are_complete() {
        let directory = fixture();
        let closure = DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml"))
            .expect("derive metadata closure");
        closure
            .verify_metadata_closure()
            .expect("metadata closure check");
        let inputs = closure
            .rerun_inputs()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        assert!(inputs
            .iter()
            .any(|input| input.ends_with("games/src/lib.rs")));
        assert!(inputs
            .iter()
            .any(|input| input.ends_with("core/src/lib.rs")));
        assert!(inputs.iter().any(|input| input.ends_with("Cargo.lock")));
        assert!(!inputs
            .iter()
            .any(|input| input.ends_with("games/tests/rules.rs")));
    }

    #[test]
    fn metadata_check_rejects_a_stale_rerun_set() {
        let directory = fixture();
        let mut closure = DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml"))
            .expect("derive metadata closure");
        let omitted = closure
            .rerun_inputs
            .iter()
            .find(|input| input.ends_with("games/src/lib.rs"))
            .cloned()
            .expect("game source is an explicit rerun input");
        closure.rerun_inputs.remove(&omitted);
        assert!(closure.verify_metadata_closure().is_err());
    }

    #[test]
    fn production_rule_source_changes_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/src/lib.rs"),
            "pub const RULE: u32 = 8;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 3; }\n",
        );
        assert_ne!(digest(&directory), baseline);
    }

    #[test]
    fn ordinary_comment_does_not_change_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/src/lib.rs"),
            "// presentation only\npub const RULE: u32 = 7;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 3; }\n",
        );
        assert_eq!(digest(&directory), baseline);
    }

    #[test]
    fn integration_test_does_not_change_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/tests/rules.rs"),
            "#[test] fn integration() { assert!(true); }\n",
        );
        assert_eq!(digest(&directory), baseline);
    }

    #[test]
    fn cfg_test_module_does_not_change_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/src/lib.rs"),
            "pub const RULE: u32 = 7;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 99; }\n",
        );
        assert_eq!(digest(&directory), baseline);
    }
}
