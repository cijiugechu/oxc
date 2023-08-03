use std::{
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
};

use crate::options::LintOptions;
use crate::walk::Walk;
use miette::{LabeledSpan, NamedSource};
use oxc_allocator::Allocator;
use oxc_diagnostics::{
    miette::{self},
    Error, Severity,
};
use oxc_linter::{Fixer, LintContext, Linter};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{SourceType, VALID_EXTENSIONS};
use ropey::Rope;
use tower_lsp::lsp_types::{self, Position, Range, Url};

struct ErrorWithPosition {
    pub start_pos: Position,
    pub end_pos: Position,
    pub miette_err: Error,
    pub labels_with_pos: Vec<LabeledSpanWithPosition>,
}

struct LabeledSpanWithPosition {
    pub start_pos: Position,
    pub end_pos: Position,
    pub message: Option<String>,
}

impl ErrorWithPosition {
    pub fn new(error: Error, text: &str) -> Self {
        let labels = error.labels().map_or(vec![], Iterator::collect);
        let start =
            labels.iter().min_by_key(|span| span.offset()).map_or(0, |span| span.offset() as u32);
        let end = labels
            .iter()
            .max_by_key(|span| span.offset() + span.len())
            .map_or(0, |span| (span.offset() + span.len()) as u32);
        Self {
            miette_err: error,
            start_pos: offset_to_position(start as usize, text).unwrap_or_default(),
            end_pos: offset_to_position(end as usize, text).unwrap_or_default(),
            labels_with_pos: labels
                .iter()
                .map(|labeled_span| LabeledSpanWithPosition {
                    start_pos: offset_to_position(labeled_span.offset() as usize, text)
                        .unwrap_or_default(),
                    end_pos: offset_to_position(
                        labeled_span.offset() + labeled_span.len() as usize,
                        text,
                    )
                    .unwrap_or_default(),
                    message: labeled_span.label().map(|label| label.to_string()),
                })
                .collect(),
        }
    }

    fn into_lsp_diagnostic(&self, path: &PathBuf) -> lsp_types::Diagnostic {
        let severity = match self.miette_err.severity() {
            Some(Severity::Error) => Some(lsp_types::DiagnosticSeverity::ERROR),
            Some(Severity::Warning) => Some(lsp_types::DiagnosticSeverity::WARNING),
            _ => None,
        };

        let help = self.miette_err.help().map(|help| format!("{}", help)).unwrap_or_default();

        let related_information = Some(
            self.labels_with_pos
                .iter()
                .map(|labeled_span| lsp_types::DiagnosticRelatedInformation {
                    location: lsp_types::Location {
                        uri: lsp_types::Url::from_file_path(path).unwrap(),
                        range: lsp_types::Range {
                            start: lsp_types::Position {
                                line: labeled_span.start_pos.line as u32,
                                character: labeled_span.start_pos.character as u32,
                            },
                            end: lsp_types::Position {
                                line: labeled_span.end_pos.line as u32,
                                character: labeled_span.end_pos.character as u32,
                            },
                        },
                    },
                    message: labeled_span.message.clone().unwrap_or_default(),
                })
                .collect(),
        );

        lsp_types::Diagnostic {
            range: Range { start: self.start_pos, end: self.end_pos },
            severity,
            code: None,
            message: format!("{}\n\n{}", self.miette_err, help),
            source: Some("oxc".into()),
            code_description: None,
            related_information,
            tags: None,
            data: None,
        }
    }
}

pub struct IsolatedLintHandler {
    options: Arc<LintOptions>,
    linter: Arc<Linter>,
}

impl IsolatedLintHandler {
    pub fn new(options: Arc<LintOptions>, linter: Arc<Linter>) -> Self {
        Self { options, linter }
    }

    /// # Panics
    ///
    /// * When `mpsc::channel` fails to send.
    pub fn run_full(&self) -> Vec<(PathBuf, Vec<lsp_types::Diagnostic>)> {
        let number_of_files = Arc::new(AtomicUsize::new(0));
        let (tx_error, rx_error) = mpsc::channel::<(PathBuf, Vec<ErrorWithPosition>)>();

        self.process_paths(&number_of_files, tx_error);
        self.process_diagnostics(&rx_error)
    }

    pub fn run_single(&self, path: PathBuf) -> Option<(PathBuf, Vec<lsp_types::Diagnostic>)> {
        if self.is_wanted_ext(&path) {
            // let (tx_error, rx_error) = mpsc::channel::<(PathBuf, Vec<Error>)>();
            //
            // let linter = Arc::clone(&self.linter);
            // spawn(move || {
            //     if let Some(diagnostics) = Self::lint_path(&linter, &path) {
            //         tx_error.send(diagnostics).unwrap();
            //     }
            //     drop(tx_error);
            // });

            // rx_error.recv().ok().map(|(path, errors)| {
            //     (path, errors.iter().map(|e| e.into_lsp_diagnostic()).collect())
            // })

            Some(Self::lint_path(&self.linter, &path).map_or((path, vec![]), |(p, errors)| {
                (p.clone(), errors.iter().map(|e| e.into_lsp_diagnostic(&p)).collect())
            }))
        } else {
            None
        }
    }

    fn is_wanted_ext(&self, path: &PathBuf) -> bool {
        path.extension()
            .map_or(false, |ext| VALID_EXTENSIONS.contains(&ext.to_string_lossy().as_ref()))
    }

    fn process_paths(
        &self,
        number_of_files: &Arc<AtomicUsize>,
        tx_error: mpsc::Sender<(PathBuf, Vec<ErrorWithPosition>)>,
    ) {
        let (tx_path, rx_path) = mpsc::channel::<Box<Path>>();

        let walk = Walk::new(&self.options);
        let number_of_files = Arc::clone(number_of_files);
        rayon::spawn(move || {
            let mut count = 0;
            walk.iter().for_each(|path| {
                count += 1;
                tx_path.send(path).unwrap();
            });
            number_of_files.store(count, Ordering::Relaxed);
        });

        let linter = Arc::clone(&self.linter);
        rayon::spawn(move || {
            while let Ok(path) = rx_path.recv() {
                let tx_error = tx_error.clone();
                let linter = Arc::clone(&linter);
                rayon::spawn(move || {
                    if let Some(diagnostics) = Self::lint_path(&linter, &path) {
                        tx_error.send(diagnostics).unwrap();
                    }
                    drop(tx_error);
                });
            }
        });
    }

    fn process_diagnostics(
        &self,
        rx_error: &mpsc::Receiver<(PathBuf, Vec<ErrorWithPosition>)>,
    ) -> Vec<(PathBuf, Vec<lsp_types::Diagnostic>)> {
        rx_error
            .iter()
            .map(|(path, errors)| {
                (path.clone(), errors.iter().map(|e| e.into_lsp_diagnostic(&path)).collect())
            })
            .collect()
    }

    fn lint_path(linter: &Linter, path: &Path) -> Option<(PathBuf, Vec<ErrorWithPosition>)> {
        let source_text =
            fs::read_to_string(path).unwrap_or_else(|_| panic!("Failed to read {path:?}"));
        let allocator = Allocator::default();
        let source_type =
            SourceType::from_path(path).unwrap_or_else(|_| panic!("Incorrect {path:?}"));
        let ret = Parser::new(&allocator, &source_text, source_type)
            .allow_return_outside_function(true)
            .parse();

        if !ret.errors.is_empty() {
            return Some(Self::wrap_diagnostics(path, &source_text, ret.errors));
        };

        let program = allocator.alloc(ret.program);
        let semantic_ret = SemanticBuilder::new(&source_text, source_type)
            .with_trivias(&ret.trivias)
            .with_check_syntax_error(true)
            .build(program);

        if !semantic_ret.errors.is_empty() {
            return Some(Self::wrap_diagnostics(path, &source_text, semantic_ret.errors));
        };

        let lint_ctx = LintContext::new(&Rc::new(semantic_ret.semantic));
        let result = linter.run(lint_ctx);

        if result.is_empty() {
            return None;
        }

        if linter.has_fix() {
            let fix_result = Fixer::new(&source_text, result).fix();
            fs::write(path, fix_result.fixed_code.as_bytes()).unwrap();
            let errors = fix_result.messages.into_iter().map(|m| m.error).collect();
            return Some(Self::wrap_diagnostics(path, &source_text, errors));
        }

        let errors = result.into_iter().map(|diagnostic| diagnostic.error).collect();
        Some(Self::wrap_diagnostics(path, &source_text, errors))
    }

    fn wrap_diagnostics(
        path: &Path,
        source_text: &str,
        diagnostics: Vec<Error>,
    ) -> (PathBuf, Vec<ErrorWithPosition>) {
        let source = Arc::new(NamedSource::new(path.to_string_lossy(), source_text.to_owned()));
        let diagnostics = diagnostics
            .into_iter()
            .map(|diagnostic| {
                ErrorWithPosition::new(
                    diagnostic.with_source_code(Arc::clone(&source)),
                    source_text,
                )
            })
            .collect();
        (path.to_path_buf(), diagnostics)
    }
}

fn offset_to_position(offset: usize, source_text: &str) -> Option<Position> {
    let rope = Rope::from_str(source_text);
    let line = rope.try_char_to_line(offset).ok()?;
    let first_char_of_line = rope.try_line_to_char(line).ok()?;
    let column = offset - first_char_of_line;
    Some(Position::new(line as u32, column as u32))
}

#[derive(Debug)]
pub struct ServerLinter {
    linter: Arc<Linter>,
}

impl ServerLinter {
    pub fn new() -> Self {
        Self { linter: Arc::new(Linter::new()) }
    }

    pub fn run_full(&self, root_uri: &Url) -> Vec<(PathBuf, Vec<lsp_types::Diagnostic>)> {
        let options = LintOptions {
            paths: vec![root_uri.to_file_path().unwrap()],
            ignore_path: "node_modules".into(),
            ..LintOptions::default()
        };

        IsolatedLintHandler::new(Arc::new(options), Arc::clone(&self.linter)).run_full()
    }

    pub fn run_single(
        &self,
        root_uri: &Url,
        uri: &Url,
    ) -> Option<(PathBuf, Vec<lsp_types::Diagnostic>)> {
        let options = LintOptions {
            paths: vec![root_uri.to_file_path().unwrap()],
            ignore_path: "node_modules".into(),
            ..LintOptions::default()
        };

        IsolatedLintHandler::new(Arc::new(options), Arc::clone(&self.linter))
            .run_single(uri.to_file_path().unwrap())
    }
}
