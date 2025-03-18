use crate::GlobEntry;
use fxhash::FxHashSet;
use globwalk::DirEntry;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync;
use walkdir::WalkDir;

static KNOWN_EXTENSIONS: sync::LazyLock<Vec<&'static str>> = sync::LazyLock::new(|| {
    include_str!("fixtures/template-extensions.txt")
        .trim()
        .lines()
        // Drop commented lines
        .filter(|x| !x.starts_with('#'))
        // Drop empty lines
        .filter(|x| !x.is_empty())
        .collect()
});

struct GlobResolver {
    base: PathBuf,

    allowed_paths: FxHashSet<PathBuf>,

    // A list of known extensions + a list of extensions we found in the project.
    found_extensions: FxHashSet<String>,

    // A list of directory names where we can't use globs, but we should track each file
    // individually instead. This is because these directories are often used for both source and
    // destination files.
    forced_static_directories: Vec<PathBuf>,

    // All root directories.
    root_directories: FxHashSet<PathBuf>,

    // All directories where we can safely use deeply nested globs to watch all files.
    // In other comments we refer to these as "deep glob directories" or similar.
    //
    // E.g.: `./src/**/*.{html,js}`
    deep_globable_directories: FxHashSet<PathBuf>,

    // All directories where we can only use shallow globs to watch all direct files but not
    // folders.
    // In other comments we refer to these as "shallow glob directories" or similar.
    //
    // E.g.: `./src/*/*.{html,js}`
    shallow_globable_directories: FxHashSet<PathBuf>,
}

impl GlobResolver {
    fn new(base: PathBuf, dirs: &[PathBuf]) -> Self {
        Self {
            base: base.clone(),
            allowed_paths: FxHashSet::from_iter(dirs.iter().cloned()),
            found_extensions: FxHashSet::from_iter(KNOWN_EXTENSIONS.iter().map(|x| x.to_string())),
            forced_static_directories: vec![base.join("public")],
            root_directories: FxHashSet::from_iter(vec![base.clone()]),
            deep_globable_directories: FxHashSet::default(),
            shallow_globable_directories: FxHashSet::default(),
        }
    }
    fn resolve(&mut self) -> Vec<GlobEntry> {
        // Sorting to make sure that we always see the directories before the files. Also sorting
        // alphabetically by default.
        fn sort_by_dir_and_name(a: &DirEntry, z: &DirEntry) -> Ordering {
            match (a.file_type().is_dir(), z.file_type().is_dir()) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => a.file_name().cmp(z.file_name()),
            }
        }

        // Collect all valid paths from the root. This will already filter out ignored files, unknown
        // extensions and binary files.
        let mut it = WalkDir::new(&self.base)
            .sort_by(sort_by_dir_and_name)
            .into_iter();

        // We are only interested in valid entries
        while let Some(Ok(entry)) = it.next() {
            // Ignore known directories that we don't want to traverse into.
            if entry.file_type().is_dir() && entry.file_name() == ".git" {
                it.skip_current_dir();
                continue;
            }

            if entry.file_type().is_dir() {
                // If we are in a directory where we know that we can't use any globs, then we have to
                // track each file individually.
                if self
                    .forced_static_directories
                    .contains(&entry.path().to_path_buf())
                {
                    self.forced_static_directories
                        .push(entry.path().to_path_buf());
                    self.root_directories.insert(entry.path().to_path_buf());
                    continue;
                }

                // Although normally very unlikely, if running inside a dockerfile
                // the current directory might be "/" with no parent
                if let Some(parent) = entry.path().parent() {
                    // If we are in a directory where the parent is a forced static directory, then this
                    // will become a forced static directory as well.
                    if self
                        .forced_static_directories
                        .contains(&parent.to_path_buf())
                    {
                        self.forced_static_directories
                            .push(entry.path().to_path_buf());
                        self.root_directories.insert(entry.path().to_path_buf());
                        continue;
                    }
                }

                // If we are in a directory, and the directory is git ignored, then we don't have to
                // descent into the directory. However, we have to make sure that we mark the _parent_
                // directory as a shallow glob directory because using deep globs from any of the
                // parent directories will include this ignored directory which should not be the case.
                //
                // Another important part is that if one of the ignored directories is a deep glob
                // directory, then all of its parents (until the root) should be marked as shallow glob
                // directories as well.
                if !self.allowed_paths.contains(&entry.path().to_path_buf()) {
                    let mut parent = entry.path().parent();
                    while let Some(parent_path) = parent {
                        // If the parent is already marked as a valid deep glob directory, then we have
                        // to mark it as a shallow glob directory instead, because we won't be able to
                        // use deep globs for this directory anymore.
                        if self.deep_globable_directories.contains(parent_path) {
                            self.deep_globable_directories.remove(parent_path);
                            self.shallow_globable_directories
                                .insert(parent_path.to_path_buf());

                            // Re-scan the children of the given directory so we can add them as deep
                            // globable directories.
                            let mut child_it = WalkDir::new(parent_path)
                                .max_depth(1)
                                .sort_by(sort_by_dir_and_name)
                                .into_iter();

                            while let Some(Ok(child_entry)) = child_it.next() {
                                // Skip the current directory (which is the parent)
                                if child_entry.path() == parent_path {
                                    continue;
                                }

                                // Skip files
                                if child_entry.path().is_file() {
                                    continue;
                                }

                                // All siblings that come after the current entry will be traversed by
                                // the root iterator
                                if child_entry.path() == entry.path() {
                                    break;
                                }

                                self.deep_globable_directories
                                    .insert(child_entry.path().to_path_buf());
                                self.shallow_globable_directories
                                    .remove(&child_entry.path().to_path_buf());
                            }

                            break;
                        }

                        // If we reached the root, then we can stop.
                        if parent_path == self.base {
                            break;
                        }

                        // Mark the parent directory as a shallow glob directory and continue with its
                        // parent.
                        self.shallow_globable_directories
                            .insert(parent_path.to_path_buf());
                        parent = parent_path.parent();
                    }

                    it.skip_current_dir();
                    continue;
                }

                // If we are in a directory that is not git ignored, then we can mark this directory as
                // a valid deep glob directory. This is only necessary if any of its parents aren't
                // marked as deep glob directories already.
                let mut found_deep_glob_parent = false;
                let mut parent = entry.path().parent();
                while let Some(parent_path) = parent {
                    // If we reached the root, then we can stop.
                    if parent_path == self.base {
                        break;
                    }

                    // If the parent is already marked as a deep glob directory, then we can stop
                    // because this glob will match the current directory already.
                    if self.deep_globable_directories.contains(parent_path) {
                        found_deep_glob_parent = true;
                        break;
                    }

                    parent = parent_path.parent();
                }

                // If we didn't find a deep glob directory parent, then we can mark this directory as a
                // deep glob directory (unless it is the root).
                if !found_deep_glob_parent && entry.path() != self.base {
                    self.deep_globable_directories
                        .insert(entry.path().to_path_buf());
                }
            }

            // Handle allowed content paths
            // if is_allowed_content_path(entry.path())
            //     && allowed_paths.contains(&entry.path().to_path_buf())
            // {
            //     let path = entry.path();
            //
            //     // Collect the extension for future use when building globs.
            //     if let Some(extension) = path.extension().and_then(|x| x.to_str()) {
            //         found_extensions.insert(extension.to_string());
            //     }
            // }
        }

        let mut extension_list = self
            .found_extensions
            .clone()
            .into_iter()
            .collect::<Vec<_>>();

        extension_list.sort();

        let extension_list = extension_list.join(",");

        // Build the globs for all globable directories.
        let shallow_globs = self
            .shallow_globable_directories
            .iter()
            .map(|path| GlobEntry {
                base: path.display().to_string(),
                pattern: format!("*/*.{{{}}}", extension_list),
            });

        let deep_globs = self.deep_globable_directories.iter().map(|path| GlobEntry {
            base: path.display().to_string(),
            pattern: format!("**/*.{{{}}}", extension_list),
        });

        shallow_globs.chain(deep_globs).collect::<Vec<_>>()
    }
}

pub fn resolve_globs(base: PathBuf, dirs: &Vec<PathBuf>) -> Vec<GlobEntry> {
    GlobResolver::new(base, dirs).resolve()
}
