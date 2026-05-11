use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TextMode {
    Plain {
        fill_column: usize,
    },
    CommitMessage {
        subject_column: usize,
        body_column: usize,
        comment_prefix: &'static str,
    },
}

impl TextMode {
    pub(super) fn for_path(path: &Path, fill_column: usize) -> Self {
        if is_git_commit_message_path(path) {
            Self::CommitMessage {
                subject_column: 50,
                body_column: 72,
                comment_prefix: "#",
            }
        } else {
            Self::Plain { fill_column }
        }
    }

    pub(super) fn fill_column_for_line(self, line: usize) -> usize {
        match self {
            Self::Plain { fill_column } => fill_column,
            Self::CommitMessage {
                subject_column,
                body_column,
                ..
            } => {
                if line == 0 {
                    subject_column
                } else {
                    body_column
                }
            }
        }
    }

    pub(super) fn is_commit_message(self) -> bool {
        matches!(self, Self::CommitMessage { .. })
    }
}

fn is_git_commit_message_path(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(file_name, "MERGE_MSG" | "SQUASH_MSG" | "TAG_EDITMSG")
        || file_name.ends_with("COMMIT_MSG")
        || file_name.ends_with("COMMIT_EDITMSG")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_git_commit_message_files() {
        assert!(TextMode::for_path(Path::new(".git/COMMIT_EDITMSG"), 80).is_commit_message());
        assert!(TextMode::for_path(Path::new("worktree-COMMIT_MSG"), 80).is_commit_message());
        assert!(TextMode::for_path(Path::new("worktree-COMMIT_EDITMSG"), 80).is_commit_message());
        assert!(TextMode::for_path(Path::new(".git/MERGE_MSG"), 80).is_commit_message());
        assert!(TextMode::for_path(Path::new(".git/SQUASH_MSG"), 80).is_commit_message());
        assert!(TextMode::for_path(Path::new(".git/TAG_EDITMSG"), 80).is_commit_message());
    }

    #[test]
    fn leaves_other_files_plain() {
        assert_eq!(
            TextMode::for_path(Path::new("notes.txt"), 80),
            TextMode::Plain { fill_column: 80 }
        );
    }
}
