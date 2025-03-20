pub mod auto_source_detection;
pub mod detect_sources;
pub mod sources;

use crate::extractor::{Extracted, Extractor};
use crate::glob::optimize_patterns;
use crate::scanner::detect_sources::resolve_globs;
use crate::scanner::sources::{
    public_source_entries_to_private_source_entries, PublicSourceEntry, SourceEntry, Sources,
};
use crate::GlobEntry;
use bstr::ByteSlice;
use fxhash::{FxHashMap, FxHashSet};
use ignore::{gitignore::GitignoreBuilder, WalkBuilder};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{self, Arc, Mutex};
use std::time::SystemTime;
use tracing::event;

// @source "some/folder";               // This is auto source detection
// @source "some/folder/**/*";          // This is auto source detection
// @source "some/folder/*.html";        // This is just a glob, but new files matching this should be included
// @source "node_modules/my-ui-lib";    // Auto source detection but since node_modules is explicit we allow it
//                                      // Maybe could be considered `external(…)` automatically if:
//                                      // 1. It's git ignored but listed explicitly
//                                      // 2. It exists outside of the current working directory (do we know that?)
//
// @source "do-include-me.bin";         // `.bin` is typically ignored, but now it's explicit so should be included
// @source "git-ignored.html";          // A git ignored file that is listed explicitly, should be scanned
static SHOULD_TRACE: sync::LazyLock<bool> = sync::LazyLock::new(
    || matches!(std::env::var("DEBUG"), Ok(value) if value.eq("*") || (value.contains("tailwindcss:oxide") && !value.contains("-tailwindcss:oxide"))),
);

fn init_tracing() {
    if !*SHOULD_TRACE {
        return;
    }

    _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::ACTIVE)
        .compact()
        .try_init();
}

#[derive(Debug, Clone)]
pub enum ChangedContent {
    File(PathBuf, String),
    Content(String, String),
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Base path to start scanning from
    pub base: Option<String>,

    /// Glob sources
    pub sources: Vec<GlobEntry>,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub candidates: Vec<String>,
    pub files: Vec<String>,
    pub globs: Vec<GlobEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct Scanner {
    /// Content sources
    sources: Sources,

    /// The walker to detect all files that we have to scan
    walker: Option<WalkBuilder>,

    /// All changed content that we have to parse
    changed_content: Vec<ChangedContent>,

    /// All files that we have to scan
    files: Vec<PathBuf>,

    /// All directories, sub-directories, etc… we saw during source detection
    dirs: Vec<PathBuf>,

    /// All generated globs, used for setting up watchers
    globs: Vec<GlobEntry>,

    /// Track unique set of candidates
    candidates: FxHashSet<String>,
}

impl Scanner {
    pub fn new(sources: Vec<PublicSourceEntry>) -> Self {
        let sources = Sources::new(public_source_entries_to_private_source_entries(sources));

        Self {
            sources: sources.clone(),
            walker: create_walker(sources),
            ..Default::default()
        }
    }

    pub fn scan(&mut self) -> Vec<String> {
        init_tracing();

        let start = std::time::Instant::now();
        self.scan_sources();
        eprintln!("Scanned sources in {:?}", start.elapsed());

        // TODO: performance improvement, bail early if we don't have any changed content
        // if self.changed_content.is_empty() {
        //     return vec![];
        // }

        let changed_content = self.changed_content.drain(..).collect::<Vec<_>>();
        let _new_candidates = self.scan_content(changed_content);

        // Make sure we have a sorted list of candidates
        let mut candidates = self.candidates.iter().cloned().collect::<Vec<_>>();
        candidates.par_sort_unstable();

        // Return all candidates instead of only the new ones
        candidates
    }

    #[tracing::instrument(skip_all)]
    pub fn scan_content(&mut self, changed_content: Vec<ChangedContent>) -> Vec<String> {
        let candidates = parse_all_blobs(read_all_files(changed_content));

        // Only compute the new candidates and ignore the ones we already have. This is for
        // subsequent calls to prevent serializing the entire set of candidates every time.
        let mut new_candidates = candidates
            .into_par_iter()
            .filter(|candidate| !self.candidates.contains(candidate))
            .collect::<Vec<_>>();

        new_candidates.par_sort_unstable();

        // Track new candidates for subsequent calls
        self.candidates.par_extend(new_candidates.clone());

        new_candidates
    }

    #[tracing::instrument(skip_all)]
    fn scan_sources(&mut self) {
        let Some(walker) = &mut self.walker else {
            return;
        };

        for entry in walker.build().filter_map(Result::ok) {
            let path = entry.into_path();
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                self.dirs.push(path);
            } else if metadata.is_file() {
                let extension = path
                    .extension()
                    .and_then(|x| x.to_str())
                    .unwrap_or_default(); // In case the file has no extension

                self.changed_content.push(ChangedContent::File(
                    path.to_path_buf(),
                    extension.to_owned(),
                ));

                self.files.push(path);
            }
        }
    }

    #[tracing::instrument(skip_all)]
    pub fn get_files(&mut self) -> Vec<String> {
        self.scan_sources();

        self.files
            .par_iter()
            .filter_map(|x| x.clone().into_os_string().into_string().ok())
            .collect()
    }

    #[tracing::instrument(skip_all)]
    pub fn get_globs(&mut self) -> Vec<GlobEntry> {
        self.scan_sources();

        for source in self.sources.iter() {
            if let SourceEntry::Auto { base } = source {
                // Insert a glob for the base path, so we can see new files/folders in the
                // directory itself.
                self.globs.push(GlobEntry {
                    base: base.to_string_lossy().into(),
                    pattern: "*".into(),
                });

                let globs = resolve_globs((base).to_path_buf(), &self.dirs);
                self.globs.extend(globs);
            }
        }

        // Re-optimize the globs to reduce the number of patterns we have to scan.
        self.globs = optimize_patterns(&self.globs);

        self.globs.clone()
    }

    #[tracing::instrument(skip_all)]
    pub fn get_candidates_with_positions(
        &mut self,
        changed_content: ChangedContent,
    ) -> Vec<(String, usize)> {
        let content = read_changed_content(changed_content).unwrap_or_default();
        let original_content = &content;

        // Workaround for legacy upgrades:
        //
        // `-[]` won't parse in the new parser (`[…]` must contain _something_), but we do need it
        // for people using `group-[]` (which we will later replace with `in-[.group]` instead).
        let content = content.replace("-[]", "XYZ");
        let offset = content.as_ptr() as usize;

        let mut extractor = Extractor::new(&content[..]);

        extractor
            .extract()
            .into_par_iter()
            .flat_map(|extracted| match extracted {
                Extracted::Candidate(s) => {
                    let i = s.as_ptr() as usize - offset;
                    let original = &original_content[i..i + s.len()];
                    if original.contains_str("-[]") {
                        return Some(unsafe {
                            (String::from_utf8_unchecked(original.to_vec()), i)
                        });
                    }

                    // SAFETY: When we parsed the candidates, we already guaranteed that the byte
                    // slices are valid, therefore we don't have to re-check here when we want to
                    // convert it back to a string.
                    Some(unsafe { (String::from_utf8_unchecked(s.to_vec()), i) })
                }

                _ => None,
            })
            .collect()
    }
}

fn read_changed_content(c: ChangedContent) -> Option<Vec<u8>> {
    let (content, extension) = match c {
        ChangedContent::File(file, extension) => match std::fs::read(&file) {
            Ok(content) => (content, extension),
            Err(e) => {
                event!(tracing::Level::ERROR, "Failed to read file: {:?}", e);
                return None;
            }
        },

        ChangedContent::Content(contents, extension) => (contents.into_bytes(), extension),
    };

    Some(pre_process_input(&content, &extension))
}

pub fn pre_process_input(content: &[u8], extension: &str) -> Vec<u8> {
    use crate::extractor::pre_processors::*;

    match extension {
        "clj" | "cljs" | "cljc" => Clojure.process(content),
        "cshtml" | "razor" => Razor.process(content),
        "haml" => Haml.process(content),
        "json" => Json.process(content),
        "pug" => Pug.process(content),
        "rb" | "erb" => Ruby.process(content),
        "slim" => Slim.process(content),
        "svelte" => Svelte.process(content),
        "vue" => Vue.process(content),
        _ => content.to_vec(),
    }
}

#[tracing::instrument(skip_all)]
fn read_all_files(changed_content: Vec<ChangedContent>) -> Vec<Vec<u8>> {
    event!(
        tracing::Level::INFO,
        "Reading {:?} file(s)",
        changed_content.len()
    );

    changed_content
        .into_par_iter()
        .filter_map(read_changed_content)
        .collect()
}

#[tracing::instrument(skip_all)]
fn parse_all_blobs(blobs: Vec<Vec<u8>>) -> Vec<String> {
    let mut result: Vec<_> = blobs
        .par_iter()
        .flat_map(|blob| blob.par_split(|x| *x == b'\n'))
        .filter_map(|blob| {
            if blob.is_empty() {
                return None;
            }

            let extracted = crate::extractor::Extractor::new(blob).extract();
            if extracted.is_empty() {
                return None;
            }

            Some(FxHashSet::from_iter(extracted.into_iter().map(
                |x| match x {
                    Extracted::Candidate(bytes) => bytes,
                    Extracted::CssVariable(bytes) => bytes,
                },
            )))
        })
        .reduce(Default::default, |mut a, b| {
            a.extend(b);
            a
        })
        .into_iter()
        .map(|s| unsafe { String::from_utf8_unchecked(s.to_vec()) })
        .collect();

    // SAFETY: Unstable sort is faster and in this scenario it's also safe because we are
    //         guaranteed to have unique candidates.
    result.par_sort_unstable();

    result
}

/// Create a walker for the given sources to detect all the files that we have to scan.
///
/// The `mtimes` map is used to keep track of the last modified time of each file. This is used to
/// determine if a file or folder has changed since the last scan and we can skip folders that
/// haven't changed.
fn create_walker(sources: Sources) -> Option<WalkBuilder> {
    let mtimes: Arc<Mutex<FxHashMap<PathBuf, SystemTime>>> = Default::default();
    let mut roots: FxHashSet<&PathBuf> = FxHashSet::default();
    let mut ignores: BTreeMap<&PathBuf, BTreeSet<String>> = Default::default();

    let mut auto_content_roots = FxHashSet::default();

    for source in sources.iter() {
        match source {
            SourceEntry::Auto { base } => {
                auto_content_roots.insert(base);
                roots.insert(base);
            }
            SourceEntry::IgnoredAuto { base } => {
                ignores.entry(base).or_default().insert("**/*".to_string());
            }
            SourceEntry::Pattern { base, pattern } => {
                roots.insert(base);
                ignores
                    .entry(base)
                    .or_default()
                    .insert(format!("!{}", pattern));
            }
            SourceEntry::IgnoredPattern { base, pattern } => {
                ignores.entry(base).or_default().insert(pattern.to_string());
            }
        }
    }

    let mut roots = roots.into_iter();
    let first_root = roots.next()?;

    let mut builder = WalkBuilder::new(first_root);

    // Scan hidden files / directories
    builder.hidden(false);

    // Don't respect global gitignore files
    builder.git_global(false);

    // By default, allow .gitignore files to be used regardless of whether or not
    // a .git directory is present. This is an optimization for when projects
    // are first created and may not be in a git repo yet.
    builder.require_git(false);

    // If we are in a git repo then require it to ensure that only rules within
    // the repo are used. For example, we don't want to consider a .gitignore file
    // in the user's home folder if we're in a git repo.
    //
    // The alternative is using a call like `.parents(false)` but that will
    // prevent looking at parent directories for .gitignore files from within
    // the repo and that's not what we want.
    //
    // For example, in a project with this structure:
    //
    // home
    // .gitignore
    //  my-project
    //   .gitignore
    //   apps
    //     .gitignore
    //     web
    //       {root}
    //
    // We do want to consider all .gitignore files listed:
    // - home/.gitignore
    // - my-project/.gitignore
    // - my-project/apps/.gitignore
    //
    // However, if a repo is initialized inside my-project then only the following
    // make sense for consideration:
    // - my-project/.gitignore
    // - my-project/apps/.gitignore
    //
    // Setting the require_git(true) flag conditionally allows us to do this.
    for parent in first_root.ancestors() {
        if parent.join(".git").exists() {
            builder.require_git(true);
            break;
        }
    }

    // Add other roots
    for root in roots {
        builder.add(root);
    }

    // Setup auto source detection rules
    builder.add_gitignore(auto_source_detection::RULES.clone());

    // Setup ignores based on `@source` definitions
    for (base, patterns) in ignores {
        let mut ignore_builder = GitignoreBuilder::new(base);
        for pattern in patterns {
            // So... we have to combine patterns with the base path and make them absolute. For
            // some reason this is not handled by the `ignore` crate. (I'm pretty sure we might
            // be doing something wrong as well. But this solves it, for now.)
            let absolute_pattern = match pattern.strip_prefix("!") {
                Some(pattern) => format!("!{}", pattern),
                None => pattern,
            };
            ignore_builder.add_line(None, &absolute_pattern).unwrap();
        }
        let ignore = ignore_builder.build().unwrap();
        builder.add_gitignore(ignore);
    }

    // Setup filter based on changed files
    builder.filter_entry({
        move |entry| {
            let mut mtimes = mtimes.lock().unwrap();
            let current_time = match mtimes.get(entry.path()) {
                Some(time) if entry.path().is_dir() => {
                    // The `modified()` time will not change on the current directory, if a file in
                    // a sub-directory has changed.
                    //
                    // E.g.:
                    //
                    // ```
                    // /dir-1
                    //     /dir-2
                    //        /my-file.html <-- Writing this file, doesn't change the `modified()`
                    //                          time of `dir-1`. So we need to compute the actual
                    //                          time.
                    // ```
                    match changed_time_since(entry.path(), *time) {
                        Ok(time) => time,
                        Err(_) => SystemTime::now(),
                    }
                }

                _ => match entry.metadata() {
                    Ok(metadata) => metadata.modified().unwrap_or(SystemTime::now()),
                    Err(_) => SystemTime::now(),
                },
            };

            let previous_time = mtimes.insert(entry.clone().into_path(), current_time);

            match previous_time {
                // Time has changed, so we need to re-scan the entry
                Some(prev) if prev != current_time => true,

                // Entry was in the cache, no need to re-scan
                Some(_) => false,

                // Entry didn't exist before, so we need to scan it
                None => true,
            }
        }
    });

    Some(builder)
}

fn changed_time_since(path: &Path, since: SystemTime) -> std::io::Result<SystemTime> {
    let metadata = path.metadata()?;
    let modified_time = metadata.modified()?;

    if modified_time > since {
        return Ok(modified_time);
    }

    let mut latest_time = modified_time;

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            let time = changed_time_since(&entry.path(), since)?;
            if time > latest_time {
                latest_time = time;
            }
        } else if metadata.modified()? > since {
            return metadata.modified();
        }
    }

    Ok(latest_time)
}

#[cfg(test)]
mod tests {
    use super::{ChangedContent, Scanner};

    #[test]
    fn test_positions() {
        let mut scanner = Scanner::new(vec![]);

        for (input, expected) in [
            // Before migrations
            (
                r#"<div class="!tw__flex sm:!tw__block tw__bg-gradient-to-t flex tw:[color:red] group-[]:tw__flex"#,
                vec![
                    ("class".to_string(), 5),
                    ("!tw__flex".to_string(), 12),
                    ("sm:!tw__block".to_string(), 22),
                    ("tw__bg-gradient-to-t".to_string(), 36),
                    ("flex".to_string(), 57),
                    ("tw:[color:red]".to_string(), 62),
                    ("group-[]:tw__flex".to_string(), 77),
                ],
            ),
            // After migrations
            (
                r#"<div class="tw:flex! tw:sm:block! tw:bg-linear-to-t flex tw:[color:red] tw:in-[.tw\:group]:flex"></div>"#,
                vec![
                    ("class".to_string(), 5),
                    ("tw:flex!".to_string(), 12),
                    ("tw:sm:block!".to_string(), 21),
                    ("tw:bg-linear-to-t".to_string(), 34),
                    ("flex".to_string(), 52),
                    ("tw:[color:red]".to_string(), 57),
                    ("tw:in-[.tw\\:group]:flex".to_string(), 72),
                ],
            ),
        ] {
            let candidates = scanner.get_candidates_with_positions(ChangedContent::Content(
                input.to_string(),
                "html".into(),
            ));
            assert_eq!(candidates, expected);
        }
    }
}
