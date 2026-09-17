use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fmt::Display,
    hash::{Hash, Hasher},
    path::Path,
    str::FromStr,
    sync::Arc,
};

use cargo_metadata::MetadataCommand;
use color_eyre::eyre::{self, ContextCompat, OptionExt as _, WrapErr as _};
use harmonia_store_content_address::ContentAddressMethodAlgorithm;
use harmonia_store_derivation::{
    derivation::{Derivation, DerivationOutput},
    derived_path::SingleDerivedPath,
    placeholder::Placeholder,
};
use harmonia_store_path::{FromStoreDirStr, StoreDir, StorePath, StorePathName};
use harmonia_store_remote::HandshakeDaemonStore as _;
use harmonia_utils_hash::Algorithm::SHA256;

use crate::{
    store::{OUTPUT_FLAGS, OUTPUT_OUT, PLATFORM},
    unit_graph::{CompileMode, Unit, UnitGraph},
    util::{CloneBytes as _, IntoBytes as _},
};

mod store;
mod tools;
mod unit_graph;
mod util;

const SCRIPT_IMMEDIATE_ARGS: &str = "args-immediate";
const SCRIPT_TRANSITIVE_ARGS: &str = "args-transitive";
const SCRIPT_IMMEDIATE_ENV: &str = "env";
const SCRIPT_METADATA_ENV: &str = "metadata";

#[derive(Debug, Clone, Default)]
struct UnitCacheMeta {
    pub base_name: Option<String>,
    pub custom_output: bool,
    pub has_links: bool,
}
#[derive(Debug, Clone)]
struct UnitCache {
    pub drv_path: StorePath,
    pub meta: UnitCacheMeta,
}

#[derive(Debug, serde::Deserialize)]
struct ExternConfig {
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub path: Vec<String>,
}

fn order_units(
    all_transitive_deps: &mut HashMap<usize, BTreeSet<usize>>,
    ordered_units: &mut Vec<usize>,
    all_units: &[Unit],
    idx: usize,
) {
    if all_transitive_deps.contains_key(&idx) {
        return;
    }

    let mut transitive_deps = BTreeSet::new();
    let unit = &all_units[idx];
    for dep in &unit.dependencies {
        order_units(all_transitive_deps, ordered_units, all_units, dep.index);
        // Must not attempt to link dependencies of a build.rs
        transitive_deps.append(&mut all_transitive_deps[&dep.index].clone());
        transitive_deps.insert(dep.index);
    }

    all_transitive_deps.insert(idx, transitive_deps);

    ordered_units.push(idx);
}

fn add_long<T: Display>(args: &mut VecDeque<bytes::Bytes>, option: &str, value: &T) {
    args.push_back(format!("--{}={}", option, value).into());
}

fn add_codegen<T: Display>(args: &mut VecDeque<bytes::Bytes>, option: &str, value: &T) {
    args.push_back("-C".into());
    args.push_back(format!("{}={}", option, value).into());
}

fn add_feature(args: &mut VecDeque<bytes::Bytes>, feature: &str) {
    args.push_back("--cfg".into());
    args.push_back(format!("feature=\"{}\"", feature).into());
}

fn add_metadata_env(
    env: &mut BTreeMap<bytes::Bytes, bytes::Bytes>,
    meta: &cargo_metadata::Package,
) {
    env.insert("CARGO_PKG_NAME".into(), meta.name.to_string().into());
    env.insert("CARGO_PKG_VERSION".into(), meta.version.to_string().into());
    env.insert(
        "CARGO_PKG_VERSION_MAJOR".into(),
        meta.version.major.to_string().into(),
    );
    env.insert(
        "CARGO_PKG_VERSION_MINOR".into(),
        meta.version.minor.to_string().into(),
    );
    env.insert(
        "CARGO_PKG_VERSION_PATCH".into(),
        meta.version.patch.to_string().into(),
    );
    env.insert(
        "CARGO_PKG_VERSION_PRE".into(),
        meta.version.pre.as_str().to_owned().into(),
    );
    env.insert(
        "CARGO_PKG_AUTHORS".into(),
        meta.authors.join(":").into(),
    );
    env.insert(
        "CARGO_PKG_DESCRIPTION".into(),
        meta.description.clone().unwrap_or_default().into(),
    );
    env.insert(
        "CARGO_PKG_HOMEPAGE".into(),
        meta.homepage.clone().unwrap_or_default().into(),
    );
    env.insert(
        "CARGO_PKG_REPOSITORY".into(),
        meta.repository.clone().unwrap_or_default().into(),
    );
    env.insert(
        "CARGO_PKG_LICENSE".into(),
        meta.license.clone().unwrap_or_default().into(),
    );
    env.insert(
        "CARGO_PKG_LICENSE_FILE".into(),
        meta.license_file
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_default()
            .into(),
    );
    env.insert(
        "CARGO_PKG_RUST_VERSION".into(),
        meta.rust_version
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default()
            .into(),
    );
    env.insert(
        "CARGO_PKG_README".into(),
        meta.readme
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_default()
            .into(),
    );
}

fn extern_declaration(
    crate_type: &str,
    extern_crate_name: &str,
    out_path: &Path,
    dep_base_name: &str,
    dep: &Unit,
) -> eyre::Result<String> {
    let dep_crate_type = &dep.target.crate_types[0];
    let needs_rlib = crate_type == "bin" || crate_type == "proc-macro";
    let extension = if dep_crate_type == "lib" || dep_crate_type == "rlib" {
        if needs_rlib { ".rlib" } else { ".rmeta" }
    } else if dep_crate_type == "dylib" || dep_crate_type == "proc-macro" {
        ".so"
    } else {
        ""
    };
    Ok(format!(
        "{}={}/{}{}",
        extern_crate_name,
        out_path.to_str().wrap_err("path not valid utf-8")?,
        dep_base_name,
        extension,
    ))
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    // Someone might want a different one for some reason, idk how to get it
    let store_dir = StoreDir::default();
    let mut store = harmonia_store_remote::DaemonClientBuilder::new()
        .set_store_dir(&store_dir)
        .build_unix(store::daemon_path())
        .await?
        .handshake()
        .await?;

    let tools = tools::Tools::find(&store_dir)?;

    // TODO: accept this via some argument

    let all_extern_config: BTreeMap<String, ExternConfig> = {
        let extern_path = std::env::var("EXTERN_PATH").unwrap_or("extern.json".to_string());

        if let Ok(content) = std::fs::read_to_string(extern_path) {
            serde_json::from_str(&content).context("Could not parse extern configuration")?
        } else {
            Default::default()
        }
    };

    // Shelling out to `cargo` since the cargo crate does not provide what we need
    let sys_args: Vec<_> = std::env::args().collect();

    let unit_graph = UnitGraph::discover(&tools.cargo.real_path, &sys_args[1..])?;

    // The unit graph doesn't give us everything we want,
    // but the remainder can come from the packages in `cargo metadata`
    // The package ids match the unit graph, so we can index on them
    let package_metadata: HashMap<_, _> = {
        let mut command = MetadataCommand::new();
        command.cargo_path(tools.cargo.real_path.clone());
        let mut idx = 1;
        loop {
            if idx >= sys_args.len() - 1 {
                break;
            }
            // TODO: flags
            if &sys_args[idx] == "-m" || &sys_args[idx] == "--manifest-path" {
                command.manifest_path(sys_args[idx + 1].clone());
                idx += 1;
            }
            idx += 1;
        }
        let metadata = command.exec()?;
        metadata
            .packages
            .into_iter()
            .map(|package| (package.id.repr.clone(), package))
            .collect()
    };

    if unit_graph.version != 1 {
        eyre::bail!("Unsupported unit graph version {}", unit_graph.version);
    }

    // TODO: parse Cargo.toml and make registry to cache packages in store.
    // Would be useful to map package IDs to store paths persistently
    // TODO: improve sorting
    let (ordered_units, transitive_deps) = {
        let mut transitive_deps = HashMap::with_capacity(unit_graph.units.len());
        let mut units = Vec::with_capacity(unit_graph.units.len());
        for root in &unit_graph.roots {
            order_units(&mut transitive_deps, &mut units, &unit_graph.units, *root);
        }
        (units, transitive_deps)
    };

    let base_env = tools.base_environment();

    // TODO: find some way of caching this on disk for interactive builds
    let mut drv_cache: Vec<Option<UnitCache>> = vec![None; unit_graph.units.len()];

    for unit_idx in ordered_units {
        // TODO: wrap rustc so we can get additional args from
        // build.rs outputs
        let unit = &unit_graph.units[unit_idx];
        let unit_meta = &package_metadata[&unit.pkg_id];
        let crate_root = unit_meta
            .manifest_path
            .parent()
            .ok_or_eyre("unit does not have a source path")?;

        let drv_name = crate_root.file_name().ok_or_eyre("empty path")?;

        let src_path = store::add_to_store_nar(
            &mut store,
            crate_root.to_path_buf().into(),
            &format!("{}-src", drv_name),
        )
        .await?;

        let mut path: bytes::BytesMut = tools.rustc.path_entry().into();

        let mut env = base_env.clone();
        // TODO: more cargo env vars not from cargo metadata
        add_metadata_env(&mut env, unit_meta);

        env.insert(
            "CARGO_MANIFEST_DIR".into(),
            src_path.to_absolute_path(&store_dir).into_bytes(),
        );

        let mut inputs = BTreeSet::from([
            SingleDerivedPath::Opaque(tools.rustc.store_path.clone()),
            SingleDerivedPath::Opaque(src_path.clone()),
        ]);

        let (drv, meta) = if unit.mode == CompileMode::RunCustomBuild {
            // running build scripts always get HOST_CC/HOST_CXX, but if the target is local
            // (e.g. a build script for a dependency of a build script) its CC will be HOST_CC,
            // not the target CC
            inputs.insert(SingleDerivedPath::Opaque(tools.host_cc.store_path.clone()));
            inputs.insert(SingleDerivedPath::Opaque(tools.host_cxx.store_path.clone()));
            env.insert("HOST_CC".into(), tools.host_cc.real_path.clone_bytes());
            env.insert("HOST_CXX".into(), tools.host_cxx.real_path.clone_bytes());

            // some build scripts won't work if it can't find the CC in PATH
            path.extend_from_slice(b":");
            path.extend_from_slice(tools.host_cc.path_entry());

            // unit graph uses an empty platform to mean native
            if unit.platform.is_some() {
                inputs.insert(SingleDerivedPath::Opaque(
                    tools.target_cc.store_path.clone(),
                ));
                inputs.insert(SingleDerivedPath::Opaque(
                    tools.target_cxx.store_path.clone(),
                ));
                env.insert("CC".into(), tools.target_cc.real_path.clone_bytes());
                env.insert("CXX".into(), tools.target_cxx.real_path.clone_bytes());
                path.extend_from_slice(b":");
                path.extend_from_slice(tools.target_cc.path_entry());
            } else {
                env.insert("CC".into(), tools.host_cc.real_path.clone_bytes());
                env.insert("CXX".into(), tools.host_cxx.real_path.clone_bytes());
            }

            // TODO cfg flags set by dependencies, perhaps it could go through the same
            // path as metadata
            inputs.insert(SingleDerivedPath::Opaque(
                tools.build_wrap.store_path.clone(),
            ));
            inputs.insert(SingleDerivedPath::Opaque(tools.env_wrap.store_path.clone()));

            let mut args = Vec::new();

            // args that need rustc to calculate (e.g. CARGO_CFG_TARGET_OS, HOST, TARGET)
            {
                let target_env_drv =
                    store::target_env_drv(&mut store, &store_dir, &tools, &unit.platform).await?;
                args.push(
                    Placeholder::ca_output(&target_env_drv, &OUTPUT_OUT)
                        .render()
                        .into_bytes(),
                );
                inputs.insert(SingleDerivedPath::Built {
                    drv_path: Arc::new(SingleDerivedPath::Opaque(target_env_drv)),
                    output: OUTPUT_OUT.clone(),
                });
            }

            let mut executable_dep = None;
            // cargo gives us a few dependencies.
            // one has the actual built executable, the others are build script
            // executions of dependencies that set metadata
            for dep in &unit.dependencies {
                let dep_cache = drv_cache[dep.index].as_ref().expect("units out of order");
                let dep_unit = &unit_graph.units[dep.index];
                if dep_unit.mode == CompileMode::Build {
                    executable_dep = Some(dep);
                } else if dep_cache.meta.has_links {
                    inputs.insert(SingleDerivedPath::Built {
                        drv_path: Arc::new(SingleDerivedPath::Opaque(dep_cache.drv_path.clone())),
                        output: OUTPUT_FLAGS.clone(),
                    });
                    let flags = Placeholder::ca_output(&dep_cache.drv_path, &OUTPUT_FLAGS);
                    args.push(flags.render().join(SCRIPT_METADATA_ENV).into_bytes());
                }
            }

            let Some(executable_dep) = executable_dep else {
                eyre::bail!("build script execution did not specify what to run");
            };

            let executable_cache = drv_cache[executable_dep.index]
                .as_ref()
                .expect("units out of order");
            inputs.insert(SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque(executable_cache.drv_path.clone())),
                output: OUTPUT_OUT.clone(),
            });

            // terminate env-wrap args
            args.push("--".into());
            // build-wrap executable (running inside env-wrap)
            args.push(tools.build_wrap.real_path.clone_bytes());
            // cwd (directory for build.rs exeuction)
            args.push(src_path.to_absolute_path(&store_dir).into_bytes());
            // links_key
            args.push(unit_meta.links.clone().unwrap_or_default().into());
            // flags_dir
            args.push(Placeholder::standard_output(&OUTPUT_FLAGS).into_bytes());
            // out_dir
            args.push(Placeholder::standard_output(&OUTPUT_OUT).into_bytes());
            // script
            args.push({
                let mut script =
                    Placeholder::ca_output(&executable_cache.drv_path, &OUTPUT_OUT).render();
                script.push(&executable_dep.extern_crate_name);
                script.into_bytes()
            });

            if let Some(extern_config) = all_extern_config.get(&unit.pkg_id) {
                eprintln!("Handling external config for {}", unit.pkg_id);
                for extra_input in &extern_config.inputs {
                    let store_path = StorePath::from_store_dir_str(&store_dir, extra_input)
                        .wrap_err("Invalid store path in extra")?;
                    inputs.insert(SingleDerivedPath::Opaque(store_path));
                }

                for (var, value) in &extern_config.env {
                    env.insert(var.clone().into(), value.clone().into());
                }

                for item in &extern_config.path {
                    path.extend_from_slice(b":");
                    path.extend_from_slice(item.as_bytes());
                }
            }

            for feature in &unit.features {
                let feature_name = feature.to_uppercase().replace("-", "_");
                env.insert(format!("CARGO_FEATURE_{}", feature_name).into(), "".into());
            }

            env.insert("OPT_LEVEL".into(), unit.profile.opt_level.clone().into());
            env.insert("PATH".into(), path.into());
            (
                Derivation {
                    name: StorePathName::from_str(&format!("{}-run", drv_name))
                        .wrap_err("invalid derivation name")?,
                    outputs: BTreeMap::from([
                        (
                            OUTPUT_OUT.clone(),
                            DerivationOutput::CAFloating(
                                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                            ),
                        ),
                        (
                            OUTPUT_FLAGS.clone(),
                            DerivationOutput::CAFloating(
                                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                            ),
                        ),
                    ]),
                    inputs,
                    platform: PLATFORM.clone(),
                    builder: tools.env_wrap.real_path.clone_bytes(),
                    args,
                    // TODO: cargo build script environment variables
                    // (https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts)
                    env,
                    structured_attrs: None,
                },
                UnitCacheMeta {
                    custom_output: true,
                    has_links: unit_meta.links.is_some(),
                    ..Default::default()
                },
            )
        } else {
            if unit.target.crate_types.len() != 1 {
                eyre::bail!(
                    "unit {} has unexpected crate types, {:?}",
                    unit.pkg_id,
                    unit.target.crate_types
                );
            }
            let crate_type = &unit.target.crate_types[0];

            // nix derivation args start at argv[1], but we put rustc in here anyway.
            // It's easier to add the wrapper if needed
            let mut args = VecDeque::from([tools.rustc.real_path.clone_bytes()]);
            {
                // rustc handles finding other source files for us
                let crate_relative = unit
                    .target
                    .src_path
                    .strip_prefix(crate_root)
                    .wrap_err("internal: crate main is not in crate root???")?;
                let mut path = src_path.to_absolute_path(&store_dir);
                path.push(crate_relative);
                args.push_back(path.into_bytes())
            }

            add_long(
                &mut args,
                "out-dir",
                &Placeholder::standard_output(&OUTPUT_OUT).render().display(),
            );

            add_long(&mut args, "crate-name", &unit.target.name.replace("-", "_"));
            add_long(&mut args, "edition", &unit.target.edition);
            add_long(&mut args, "crate-type", crate_type);

            if unit.mode == CompileMode::Check {
                add_long(&mut args, "emit", &"metadata");
            } else if unit.mode == CompileMode::Build {
                if crate_type == "lib" || crate_type == "rlib" {
                    add_long(&mut args, "emit", &"metadata,link");
                } else {
                    add_long(&mut args, "emit", &"link");
                }
            }

            add_codegen(&mut args, "debuginfo", &unit.profile.debuginfo);

            add_codegen(&mut args, "opt-level", &unit.profile.opt_level);

            // TODO: embed-bitcode, lto
            // TODO: check-cfg
            // TODO: improve hash calculation
            let dep_hash = {
                let mut hasher = std::hash::DefaultHasher::new();
                for direct_dep in &unit.dependencies {
                    let dep = drv_cache[direct_dep.index].as_ref().unwrap();
                    Hash::hash(&dep.drv_path, &mut hasher);
                }
                unit.hash(&mut hasher);

                hasher.finish()
            };
            // Notice that there is metadata but no extra-filename.
            // Since every crate goes in its own derivation with its own output directory,
            // there is no chance filenames will ever conflict.
            // Symbol names, on the other hand, are linked into the same binary,
            // so metadata is required to prevent linking conflicts.
            add_codegen(&mut args, "metadata", &format_args!("{:016x}", dep_hash));
            let base_name = if crate_type == "lib" || crate_type == "proc-macro" {
                // cargo uses rustc outputs to learn rmeta locations.
                // we don't have that luxury, but the default path is documented.
                Some(format!("lib{}", unit.target.name))
            } else {
                None
            };

            for feature in &unit.features {
                add_feature(&mut args, feature);
            }

            for transitive_dep in &transitive_deps[&unit_idx] {
                let dep = drv_cache[*transitive_dep].as_ref().unwrap();
                // Needed for either library deps or OUT_PATH
                inputs.insert(SingleDerivedPath::Built {
                    drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                    output: OUTPUT_OUT.clone(),
                });

                if dep.meta.custom_output {
                    inputs.insert(SingleDerivedPath::Built {
                        drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                        output: OUTPUT_FLAGS.clone(),
                    });

                    let flags = Placeholder::ca_output(&dep.drv_path, &OUTPUT_FLAGS).render();

                    args.push_back(
                        format!("@{}", flags.join(SCRIPT_TRANSITIVE_ARGS).display()).into(),
                    );
                } else {
                    let placeholder = Placeholder::ca_output(&dep.drv_path, &OUTPUT_OUT).render();
                    args.push_back("-L".into());
                    args.push_back(format!("dependency={}", placeholder.display()).into());
                }
            }

            for direct_dep in &unit.dependencies {
                // no need to add inputs here, they're already handled from transitive deps
                let dep = drv_cache[direct_dep.index].as_ref().unwrap();

                let out = Placeholder::ca_output(&dep.drv_path, &OUTPUT_OUT).render();

                if let Some(dep_base_name) = dep.meta.base_name.as_ref() {
                    args.push_back("--extern".into());
                    args.push_back(
                        extern_declaration(
                            crate_type,
                            &direct_dep.extern_crate_name,
                            &out,
                            dep_base_name,
                            &unit_graph.units[direct_dep.index],
                        )?
                        .into(),
                    );
                }
                if dep.meta.custom_output {
                    inputs.insert(SingleDerivedPath::Opaque(tools.env_wrap.store_path.clone()));
                    env.insert("OUT_DIR".into(), out.into_bytes());
                    let flags = Placeholder::ca_output(&dep.drv_path, &OUTPUT_FLAGS).render();

                    // reversed since we're pushing from the front
                    args.push_front("--".into());
                    args.push_front(flags.join(SCRIPT_IMMEDIATE_ENV).into_bytes());
                    args.push_front(tools.env_wrap.real_path.clone_bytes());

                    args.push_back(
                        format!("@{}", flags.join(SCRIPT_IMMEDIATE_ARGS).display()).into(),
                    );
                }
            }

            if let Some(ref target) = unit.platform {
                add_long(&mut args, "target", target);
            }

            {
                // rustc doesn't respect CC environment variable
                // unix-like platforms use cc as linker
                // this won't work for wasm, though.
                let cc = if unit.platform.is_some() {
                    &tools.target_cc
                } else {
                    &tools.host_cc
                };

                inputs.insert(SingleDerivedPath::Opaque(cc.store_path.clone()));
                add_codegen(&mut args, "linker", &cc.real_path.display());
            }
            // internal to rustc, but required
            if crate_type == "proc-macro" {
                args.push_back("--extern".into());
                args.push_back("proc_macro".into());
            }

            env.insert("PATH".into(), path.into());
            let builder = args.pop_front().unwrap();
            (
                Derivation {
                    name: StorePathName::from_str(drv_name).wrap_err("invalid derivation name")?,
                    outputs: BTreeMap::from([(
                        OUTPUT_OUT.clone(),
                        DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::NixArchive(
                            SHA256,
                        )),
                    )]),
                    inputs,
                    platform: PLATFORM.clone(),
                    builder,
                    args: args.into(),
                    // TODO: cargo crate environment variables
                    // (https://doc.rust-lang.org/cargo/reference/environment-variables.html)
                    env,
                    structured_attrs: None,
                },
                UnitCacheMeta {
                    base_name,
                    ..Default::default()
                },
            )
        };

        let drv_path = store::add_drv_to_store(&mut store, &store_dir, drv, drv_name).await?;
        drv_cache[unit_idx] = Some(UnitCache { drv_path, meta });
    }

    for unit_idx in unit_graph.roots {
        let drv_path = &drv_cache[unit_idx].as_ref().unwrap().drv_path;
        println!("{}", store_dir.display(drv_path));

        if store::is_in_derivation() {
            // TODO: need more handling if there is both a lib and a bin of the same crate
            let name = unit_graph.units[unit_idx].target.name.as_str();
            store::submit_wrapper(&mut store, &store_dir, &tools.ln, name, drv_path).await?;
        }
    }

    // TODO: run the build if we are outside a derivation,
    // attach to output if we are inside a derivation.

    Ok(())
}
