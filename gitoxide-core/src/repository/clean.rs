use crate::OutputFormat;

#[derive(Default, Copy, Clone)]
pub enum FindRepository {
    #[default]
    NonBare,
    All,
}

pub struct Options {
    pub debug: bool,
    pub format: OutputFormat,
    pub execute: bool,
    pub ignored: bool,
    pub precious: bool,
    pub directories: bool,
    pub repositories: bool,
    pub skip_hidden_repositories: Option<FindRepository>,
    pub find_untracked_repositories: FindRepository,
}
pub(crate) mod function {
    use crate::repository::clean::{FindRepository, Options};
    use crate::OutputFormat;
    use anyhow::bail;
    use gix::bstr::BString;
    use gix::bstr::ByteSlice;
    use gix::dir::entry::{Kind, Status};
    use gix::dir::walk::EmissionMode::CollapseDirectory;
    use gix::dir::walk::ForDeletionMode::*;
    use gix::dir::{walk, EntryRef};
    use std::borrow::Cow;
    use std::path::Path;

    pub fn clean(
        repo: gix::Repository,
        out: &mut dyn std::io::Write,
        err: &mut dyn std::io::Write,
        patterns: Vec<BString>,
        Options {
            debug,
            format,
            mut execute,
            ignored,
            precious,
            directories,
            repositories,
            skip_hidden_repositories,
            find_untracked_repositories,
        }: Options,
    ) -> anyhow::Result<()> {
        if format != OutputFormat::Human {
            bail!("JSON output isn't implemented yet");
        }
        let Some(workdir) = repo.work_dir() else {
            bail!("Need a worktree to clean, this is a bare repository");
        };

        let index = repo.index_or_empty()?;
        let has_patterns = !patterns.is_empty();
        let mut collect = InterruptableCollect::default();
        let collapse_directories = CollapseDirectory;
        let options = repo
            .dirwalk_options()?
            .emit_pruned(true)
            .for_deletion(if (ignored || precious) && directories {
                match skip_hidden_repositories {
                    Some(FindRepository::NonBare) => Some(FindNonBareRepositoriesInIgnoredDirectories),
                    Some(FindRepository::All) => Some(FindRepositoriesInIgnoredDirectories),
                    None => None,
                }
            } else {
                Some(IgnoredDirectoriesCanHideNestedRepositories)
            })
            .classify_untracked_bare_repositories(matches!(find_untracked_repositories, FindRepository::All))
            .emit_untracked(collapse_directories)
            .emit_ignored(Some(collapse_directories))
            .emit_empty_directories(true);
        repo.dirwalk(&index, patterns, options, &mut collect)?;
        let prefix = repo.prefix()?.unwrap_or(Path::new(""));
        let prefix_len = if prefix.as_os_str().is_empty() {
            0
        } else {
            prefix.to_str().map_or(0, |s| s.len() + 1 /* slash */)
        };

        let entries = collect.inner.into_entries_by_path();
        let mut entries_to_clean = 0;
        let mut skipped_directories = 0;
        let mut skipped_ignored = 0;
        let mut skipped_precious = 0;
        let mut skipped_repositories = 0;
        let mut pruned_entries = 0;
        let mut saw_ignored_directory = false;
        let mut saw_untracked_directory = false;
        for (entry, dir_status) in entries.into_iter() {
            if dir_status.is_some() {
                if debug {
                    writeln!(
                        err,
                        "DBG: prune '{}' {:?} as parent dir is used instead",
                        entry.rela_path, entry.status
                    )
                    .ok();
                }
                continue;
            }

            let pathspec_includes_entry = entry
                .pathspec_match
                .map_or(false, |m| m != gix::dir::entry::PathspecMatch::Excluded);
            pruned_entries += usize::from(!pathspec_includes_entry);
            if !pathspec_includes_entry && debug {
                writeln!(err, "DBG: prune '{}' as it is excluded by pathspec", entry.rela_path).ok();
            }
            if entry.status.is_pruned() || !pathspec_includes_entry {
                continue;
            }

            let mut disk_kind = entry.disk_kind.expect("present if not pruned");
            let keep = match entry.status {
                Status::DotGit | Status::Pruned | Status::TrackedExcluded => {
                    unreachable!("BUG: Pruned are skipped already as their pathspec is always None")
                }
                Status::Tracked => {
                    unreachable!("BUG: tracked aren't emitted")
                }
                Status::Ignored(gix::ignore::Kind::Expendable) => {
                    skipped_ignored += usize::from(!ignored);
                    ignored
                }
                Status::Ignored(gix::ignore::Kind::Precious) => {
                    skipped_precious += usize::from(!precious);
                    precious
                }
                Status::Untracked => true,
            };
            if !keep {
                if debug {
                    writeln!(err, "DBG: prune '{}' as -x or -p is missing", entry.rela_path).ok();
                }
                continue;
            }

            if disk_kind == gix::dir::entry::Kind::Directory
                && gix::discover::is_git(&workdir.join(gix::path::from_bstr(entry.rela_path.as_bstr()))).is_ok()
            {
                if debug {
                    writeln!(err, "DBG: upgraded directory '{}' to bare repository", entry.rela_path).ok();
                }
                disk_kind = gix::dir::entry::Kind::Repository;
            }

            match disk_kind {
                Kind::File | Kind::Symlink => {}
                Kind::EmptyDirectory | Kind::Directory => {
                    if !directories {
                        skipped_directories += 1;
                        if debug {
                            writeln!(err, "DBG: prune '{}' as -d is missing", entry.rela_path).ok();
                        }
                        continue;
                    }
                }
                Kind::Repository => {
                    if !repositories {
                        skipped_repositories += 1;
                        if debug {
                            writeln!(err, "DBG: skipped repository at '{}'", entry.rela_path)?;
                        }
                        continue;
                    }
                }
            };

            let is_ignored = matches!(entry.status, gix::dir::entry::Status::Ignored(_));
            let display_path = entry.rela_path[prefix_len..].as_bstr();
            if disk_kind == gix::dir::entry::Kind::Directory {
                saw_ignored_directory |= is_ignored;
                saw_untracked_directory |= entry.status == gix::dir::entry::Status::Untracked;
            }
            writeln!(
                out,
                "{maybe}{suffix} {}{} {status}",
                display_path,
                disk_kind.is_dir().then_some("/").unwrap_or_default(),
                status = match entry.status {
                    Status::Ignored(kind) => {
                        Cow::Owned(format!(
                            "({})",
                            match kind {
                                gix::ignore::Kind::Precious => "💲",
                                gix::ignore::Kind::Expendable => "🗑️",
                            }
                        ))
                    }
                    Status::Untracked => {
                        "".into()
                    }
                    status =>
                        if debug {
                            format!("(DBG: {status:?})").into()
                        } else {
                            "".into()
                        },
                },
                maybe = if execute { "removing" } else { "WOULD remove" },
                suffix = match disk_kind {
                    Kind::File | Kind::Symlink | Kind::Directory => {
                        ""
                    }
                    Kind::EmptyDirectory => {
                        " empty"
                    }
                    Kind::Repository => {
                        " repository"
                    }
                },
            )?;

            if gix::interrupt::is_triggered() {
                execute = false;
            }
            if execute {
                let path = workdir.join(gix::path::from_bstr(entry.rela_path));
                if disk_kind.is_dir() {
                    std::fs::remove_dir_all(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
            } else {
                entries_to_clean += 1;
            }
        }
        if !execute {
            let mut messages = Vec::new();
            messages.extend(
                (skipped_directories > 0).then(|| format!("Skipped {skipped_directories} directories - show with -d")),
            );
            messages.extend(
                (skipped_repositories > 0)
                    .then(|| format!("Skipped {skipped_repositories} repositories - show with -r")),
            );
            messages.extend(
                (skipped_ignored > 0).then(|| format!("Skipped {skipped_ignored} expendable entries - show with -x")),
            );
            messages.extend(
                (skipped_precious > 0).then(|| format!("Skipped {skipped_precious} precious entries - show with -p")),
            );
            messages.extend(
                (pruned_entries > 0 && has_patterns).then(|| {
                    format!("try to adjust your pathspec to reveal some of the {pruned_entries} pruned entries")
                }),
            );
            let make_msg = || -> String {
                if messages.is_empty() {
                    return String::new();
                }
                messages.join("; ")
            };
            let wrap_in_parens = |msg: String| if msg.is_empty() { msg } else { format!(" ({msg})") };
            if entries_to_clean > 0 {
                let mut wrote_nl = false;
                let msg = make_msg();
                let mut msg = if msg.is_empty() { None } else { Some(msg) };
                if saw_ignored_directory && skip_hidden_repositories.is_none() {
                    writeln!(err).ok();
                    wrote_nl = true;
                    writeln!(
                        err,
                        "WARNING: would remove repositories hidden inside ignored directories - use --skip-hidden-repositories to skip{}",
                        wrap_in_parens(msg.take().unwrap_or_default())
                    )?;
                }
                if saw_untracked_directory && matches!(find_untracked_repositories, FindRepository::NonBare) {
                    if !wrote_nl {
                        writeln!(err).ok();
                        wrote_nl = true;
                    }
                    writeln!(
                        err,
                        "WARNING: would remove repositories hidden inside untracked directories - use --find-untracked-repositories to find{}",
                        wrap_in_parens(msg.take().unwrap_or_default())
                    )?;
                }
                if let Some(msg) = msg.take() {
                    if !wrote_nl {
                        writeln!(err).ok();
                    }
                    writeln!(err, "{msg}").ok();
                }
            } else {
                writeln!(err, "Nothing to clean{}", wrap_in_parens(make_msg()))?;
            }
            if gix::interrupt::is_triggered() {
                writeln!(err, "Result may be incomplete as it was interrupted")?;
            }
        }
        Ok(())
    }

    #[derive(Default)]
    struct InterruptableCollect {
        inner: gix::dir::walk::delegate::Collect,
    }

    impl gix::dir::walk::Delegate for InterruptableCollect {
        fn emit(&mut self, entry: EntryRef<'_>, collapsed_directory_status: Option<Status>) -> walk::Action {
            let res = self.inner.emit(entry, collapsed_directory_status);
            if gix::interrupt::is_triggered() {
                return walk::Action::Cancel;
            }
            res
        }
    }
}
