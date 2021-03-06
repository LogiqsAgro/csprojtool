use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
};

use log::{debug, info};
use xmltree::{Element, XMLNode};

use crate::{
    path_extensions::{relative_path, PathExt},
    utils::{find_dir_csproj, find_git_root},
    xml_extensions::{child_elements, depth_first_visit_nodes, process_tree, transform_xml_file},
};

const ARG_FROM: &'static str = "from";
const ARG_TO: &'static str = "to";
const CMD_MOVE: &'static str = "mv";

#[derive(Debug)]
pub struct MoveCommand {
    old: PathBuf,
    new: PathBuf,
}

impl MoveCommand {
    pub fn subcommand() -> clap::App<'static, 'static> {
        use clap::Arg;
        use clap::SubCommand;

        SubCommand::with_name(CMD_MOVE)
            .about("Move a project")
            .arg(
                Arg::with_name(ARG_FROM)
                    .value_name("FROM")
                    .help("The old path")
                    .required(true)
                    .takes_value(true)
                    .index(1),
            )
            .arg(
                Arg::with_name(ARG_TO)
                    .value_name("TO")
                    .help("The new path")
                    .required(true)
                    .takes_value(true)
                    .index(2),
            )
    }

    pub fn try_from_matches(matches: &clap::ArgMatches) -> Option<Self> {
        matches.subcommand_matches(CMD_MOVE).map(Self::from_matches)
    }

    fn from_matches(matches: &clap::ArgMatches) -> Self {
        Self {
            old: matches.value_of_os(ARG_FROM).unwrap().into(),
            new: matches.value_of_os(ARG_TO).unwrap().into(),
        }
    }

    pub fn execute(&self) {
        info!("moving {0} to {1}", self.old.display(), self.new.display());

        let (old_dir, old_file) = {
            let old = std::fs::canonicalize(&self.old).unwrap();
            let meta = std::fs::metadata(&old).unwrap();
            if meta.is_file() {
                (old.parent().unwrap().to_owned(), old)
            } else if meta.is_dir() {
                let mut csprojs_in_dir = find_dir_csproj(&old);
                let first = csprojs_in_dir.next();

                let second = csprojs_in_dir.next();
                if second.is_some() {
                    panic!("More than one csproj found in {}", old.display());
                }

                if let Some(first) = first {
                    (old, first)
                } else {
                    panic!("No csproj found in {}", old.display());
                }
            } else {
                panic!(
                    "The path {} does not point to a file nor to a directory",
                    old.display()
                );
            }
        };

        debug!("determined old path to be {}", old_file.display());

        let cur_dir = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();

        let (new_dir, new_file) = {
            // This converts the path to use OS slashes. Without this the joining may fail when combining windows and linux paths.
            let new = self.new.simplify();

            let path = [&cur_dir, &new].iter().collect::<PathBuf>().simplify();

            if path.extension() == Some(OsStr::new("csproj")) {
                (path.parent().unwrap().to_owned(), path)
            } else {
                let name = [path.file_name().unwrap(), OsStr::new(".csproj")]
                    .iter()
                    .copied()
                    .collect::<OsString>();
                let new_file = path.join(name);
                (path, new_file)
            }
        };

        {
            match std::fs::metadata(&new_dir) {
                Ok(_) => {
                    panic!("Target directory {} already exists", new_dir.display());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => Err(e).unwrap(),
            }
        }

        debug!("determined new path to be {}", new_file.display());

        let root = find_git_root(&old_dir).unwrap_or(&cur_dir);

        debug!("root: {}", root.display());

        let csproj_matcher = globset::GlobBuilder::new("*.csproj")
            .build()
            .unwrap()
            .compile_matcher();
        let csproj_paths = ignore::WalkBuilder::new(root)
            .build()
            .filter_map(|entry| match entry {
                Ok(e) => {
                    if e.file_type().map(|t| t.is_file()).unwrap_or_default()
                        && csproj_matcher.is_match(e.path())
                    {
                        Some(Ok(e.path().to_owned()))
                    } else {
                        None
                    }
                }
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<Vec<_>, ignore::Error>>()
            .unwrap();

        // Check for nested projects
        let nested = csproj_paths
            .iter()
            .filter(|&p| p.starts_with(&old_dir) && p != &old_file)
            .collect::<Vec<_>>();
        if !nested.is_empty() {
            panic!(
                "The to-be-moved project contains nested projects: {:#?}",
                nested
            );
        }

        // Move the files
        let mut mv_dir = Command::new("git");
        mv_dir.args(&[OsStr::new("mv"), old_dir.as_os_str(), new_dir.as_os_str()]);
        debug!("{:?}", &mv_dir);
        mv_dir.output().expect("failed to move files");

        {
            let current_path = new_dir.join(old_file.file_name().unwrap());
            if &current_path != &new_file {
                let mut mv_file = Command::new("git");
                mv_file.args(&[
                    OsStr::new("mv"),
                    current_path.as_os_str(),
                    new_file.as_os_str(),
                ]);
                debug!("{:?}", &mv_file);
                mv_file.output().expect("failed to move files");
            }
        }

        for csproj_path in csproj_paths.iter() {
            if csproj_path == &old_file {
                continue;
            }

            let csproj_dir = csproj_path.parent().unwrap();

            let mut edited = false;
            transform_xml_file(csproj_path, |mut root| {
                process_tree(&mut root, |element| match element.name.as_ref() {
                    "ProjectReference" => {
                        if let Some(include) = element.attributes.get_mut("Include") {
                            let ref_path = [csproj_dir, Path::new(include)]
                                .iter()
                                .collect::<PathBuf>()
                                .simplify();

                            if ref_path == old_file {
                                let new_ref = relative_path(csproj_dir, &new_file);
                                debug!(
                                    "replacing project reference {} with {} in {}",
                                    include,
                                    new_ref.display(),
                                    csproj_path.display()
                                );
                                *include = new_ref.to_str().unwrap().to_owned();
                                edited = true;
                            }
                        }
                    }
                    _ => {}
                });

                if edited {
                    Some(root)
                } else {
                    None
                }
            })
            .unwrap();

            if edited {
                let mut add_file = Command::new("git");
                add_file.args(&[OsStr::new("add"), csproj_path.as_os_str()]);
                debug!("{:?}", &add_file);
                add_file.output().expect("failed to add file");
            }
        }

        let mut edited = false;

        transform_xml_file(&new_file, |root| {
            let mut root_node = XMLNode::Element(root);

            depth_first_visit_nodes(&mut root_node, |node| match node {
                XMLNode::Element(element) => match element.name.as_ref() {
                    "Project" => {
                        let name = old_file.file_stem().unwrap().to_str().unwrap();
                        edited |= ensure_root_namespace_and_assembly_name(element, name);
                    }
                    _ => {
                        for (_, val) in element.attributes.iter_mut() {
                            edited |= try_rewrite_relative_path(val, &old_dir, &new_dir);
                        }
                    }
                },
                XMLNode::Text(text) => {
                    edited |= try_rewrite_relative_path(text, &old_dir, &new_dir);
                }
                _ => {}
            });

            let root = match root_node {
                XMLNode::Element(root) => root,
                _ => unreachable!(),
            };

            if edited {
                Some(root)
            } else {
                None
            }
        })
        .unwrap();

        if edited {
            let mut add_file = Command::new("git");
            add_file.args(&[OsStr::new("add"), new_file.as_os_str()]);
            debug!("{:?}", &add_file);
            add_file.output().expect("failed to add file");
        }
    }
}

fn try_rewrite_relative_path(val: &mut String, old_dir: &Path, new_dir: &Path) -> bool {
    if !looks_like_out_of_tree_relative_path(val) {
        return false;
    }

    let mut edited = true;
    let path = Path::new(val);
    if !path.has_root() {
        let path = path.simplify();
        let old_abs_path = old_dir.join(&path).simplify();
        match std::fs::metadata(&old_abs_path) {
            Ok(_) => {
                let new_rel_path = relative_path(&new_dir, &old_abs_path);
                debug!(
                    "rewriting relative path from {} to {}",
                    val,
                    new_rel_path.display(),
                );
                *val = new_rel_path.to_str().unwrap().to_owned();
                edited = true;
            }
            _ => {}
        }
    }
    edited
}

fn looks_like_out_of_tree_relative_path(val: &str) -> bool {
    lazy_static::lazy_static! {
        static ref RE: regex::Regex = regex::Regex::new(r"\.\.[/\\]").unwrap();
    }
    RE.is_match(val)
}

fn ensure_root_namespace_and_assembly_name(element: &mut xmltree::Element, name: &str) -> bool {
    let (root_namespace, assembly_name) =
        child_elements(element).fold((None, None), |state, element| {
            if element.name == "PropertyGroup" {
                (
                    state
                        .0
                        .or_else(|| child_elements(element).find(|e| e.name == "RootNamespace")),
                    state
                        .1
                        .or_else(|| child_elements(element).find(|e| e.name == "AssemblyName")),
                )
            } else {
                state
            }
        });

    if let Some(element) = root_namespace {
        debug!(
            "Project already contains RootNamespace: {}",
            element.get_text().unwrap_or("".into())
        );
    }

    if let Some(element) = assembly_name {
        debug!(
            "Project already contains AssemblyName: {}",
            element.get_text().unwrap_or("".into())
        );
    }

    let has_root_namespace = root_namespace.is_some();
    let has_assembly_name = root_namespace.is_some();

    let mut modified = false;
    if let Some(property_group_element) = element.get_mut_child("PropertyGroup") {
        if !has_root_namespace {
            info!("Adding RootNamespace to project: {}", name.to_owned());
            let mut el = Element::new("RootNamespace");
            el.children.push(XMLNode::Text(name.to_owned()));
            property_group_element.children.push(XMLNode::Element(el));
            modified = true;
        }

        if !has_assembly_name {
            info!("Adding AssemblyName to project: {}", name.to_owned());
            let mut el = Element::new("AssemblyName");
            el.children.push(XMLNode::Text(name.to_owned()));
            property_group_element.children.push(XMLNode::Element(el));
            modified = true;
        }
    }

    modified
}
