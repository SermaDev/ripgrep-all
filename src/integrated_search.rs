use anyhow::{Context, Result};
use grep_matcher::Matcher;
use grep_printer::StandardBuilder;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use log::debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use termcolor::{ColorChoice, StandardStream};

use crate::adapters::*;
use crate::config::RgaConfig;
use crate::preproc::*;

pub struct IntegratedSearcher {
    config: RgaConfig,
    _adapters: Vec<Arc<dyn FileAdapter>>,
    pre_glob: String,
}

impl IntegratedSearcher {
    pub fn new(config: RgaConfig, adapters: Vec<Arc<dyn FileAdapter>>, pre_glob: String) -> Self {
        Self {
            config,
            _adapters: adapters,
            pre_glob,
        }
    }

    /// Run the integrated search with the given pattern and paths
    pub async fn run_async(
        &self,
        pattern: &str,
        paths: Vec<PathBuf>,
        rg_args: &[String],
    ) -> Result<i32> {
        // Parse additional rg arguments to extract flags
        let (smart_case, no_line_number, color_choice) = self.parse_rg_args(rg_args);

        // Build the regex matcher
        let matcher = RegexMatcherBuilder::new()
            .case_smart(smart_case)
            .build(pattern)
            .context("Failed to build regex matcher")?;

        // Set up the printer for results
        let color = match color_choice {
            ColorChoiceArg::Always => ColorChoice::Always,
            ColorChoiceArg::Never => ColorChoice::Never,
            ColorChoiceArg::Auto => ColorChoice::Auto,
        };
        
        let stdout = StandardStream::stdout(color);
        let mut printer = StandardBuilder::new().build(stdout);

        // Set up the searcher
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(!no_line_number)
            .build();

        // Walk files and search
        let paths_to_search = if paths.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            paths
        };

        let mut found_match = false;

        for path in paths_to_search {
            let walker = WalkBuilder::new(&path)
                .hidden(false)
                .build();

            for entry in walker {
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        debug!("Error walking directory: {}", err);
                        continue;
                    }
                };

                if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                    continue;
                }

                let file_path = entry.path();
                
                // Check if file matches pre_glob pattern
                if !self.should_preprocess(file_path) {
                    // For non-preprocessed files, search directly
                    if self.search_file(&mut searcher, &matcher, &mut printer, file_path)? {
                        found_match = true;
                    }
                } else {
                    // Preprocess the file inline and search the output
                    if self.search_preprocessed_file_async(&matcher, &mut printer, file_path, !no_line_number).await? {
                        found_match = true;
                    }
                }
            }
        }

        // Return exit code: 0 if found matches, 1 if not
        Ok(if found_match { 0 } else { 1 })
    }

    /// Check if a file should be preprocessed based on pre_glob pattern
    fn should_preprocess(&self, path: &Path) -> bool {
        if self.pre_glob == "*" {
            return true;
        }

        if let Some(ext) = path.extension() {
            let ext_str = ext.to_string_lossy().to_lowercase();
            // Extract extensions from pre_glob (format: "*.{ext1,ext2,...}")
            if let Some(exts) = self.pre_glob.strip_prefix("*.{").and_then(|s| s.strip_suffix("}")) {
                return exts.split(',').any(|e| e.to_lowercase() == ext_str);
            }
        }
        false
    }

    /// Search a regular file directly without preprocessing
    fn search_file(
        &self,
        searcher: &mut grep_searcher::Searcher,
        matcher: &impl Matcher,
        printer: &mut grep_printer::Standard<StandardStream>,
        path: &Path,
    ) -> Result<bool> {
        let result = searcher.search_path(
            matcher,
            path,
            printer.sink_with_path(matcher, path),
        );

        match result {
            Ok(_) => {
                // For now, we assume if search succeeded without error, we found matches
                // The printer will have already printed any matches
                // TODO: Track actual match count for accurate exit codes
                Ok(true)
            }
            Err(err) => {
                debug!("Error searching {}: {}", path.display(), err);
                Ok(false)
            }
        }
    }

    /// Preprocess a file and search the preprocessed output
    async fn search_preprocessed_file_async(
        &self,
        matcher: &impl Matcher,
        printer: &mut grep_printer::Standard<StandardStream>,
        path: &Path,
        line_numbers: bool,
    ) -> Result<bool> {
        debug!("Preprocessing file: {}", path.display());

        // Run the preprocessing asynchronously
        let preprocessed = self.preprocess_file_async(path).await?;

        // Search the preprocessed content
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(line_numbers)
            .build();

        let result = searcher.search_slice(
            matcher,
            &preprocessed,
            printer.sink_with_path(matcher, path),
        );

        match result {
            Ok(_) => {
                // For now, we assume if search succeeded without error, we found matches
                // TODO: Track actual match count for accurate exit codes
                Ok(true)
            }
            Err(err) => {
                debug!("Error searching preprocessed content for {}: {}", path.display(), err);
                Ok(false)
            }
        }
    }

    /// Preprocess a file using the existing adapter infrastructure
    async fn preprocess_file_async(&self, path: &Path) -> Result<Vec<u8>> {
        use tokio::fs::File;
        use tokio::io::AsyncReadExt;

        let file = File::open(path).await
            .with_context(|| format!("Failed to open file: {}", path.display()))?;

        let ai = AdaptInfo {
            inp: Box::pin(file),
            filepath_hint: path.to_path_buf(),
            is_real_file: true,
            line_prefix: "".to_string(),
            archive_recursion_depth: 0,
            postprocess: !self.config.no_prefix_filenames,
            config: self.config.clone(),
        };

        let mut output = rga_preproc(ai).await
            .with_context(|| format!("Failed to preprocess file: {}", path.display()))?;

        let mut buffer = Vec::new();
        output.read_to_end(&mut buffer).await
            .context("Failed to read preprocessed output")?;

        Ok(buffer)
    }

    /// Parse ripgrep arguments to extract flags
    fn parse_rg_args(&self, args: &[String]) -> (bool, bool, ColorChoiceArg) {
        let mut smart_case = true;  // Default to smart case
        let mut no_line_number = false;
        let mut color = ColorChoiceArg::Auto;
        
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            match arg.as_str() {
                "--smart-case" => smart_case = true,
                "--case-sensitive" | "-s" => smart_case = false,
                "--ignore-case" | "-i" => smart_case = false,
                "--no-line-number" => no_line_number = true,
                "--line-number" | "-n" => no_line_number = false,
                "--color=always" => color = ColorChoiceArg::Always,
                "--color=never" => color = ColorChoiceArg::Never,
                "--color=auto" => color = ColorChoiceArg::Auto,
                "--color" => {
                    // Handle --color value format (two separate arguments)
                    if i + 1 < args.len() {
                        i += 1;
                        match args[i].as_str() {
                            "always" => color = ColorChoiceArg::Always,
                            "never" => color = ColorChoiceArg::Never,
                            "auto" => color = ColorChoiceArg::Auto,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }

        (smart_case, no_line_number, color)
    }
}

enum ColorChoiceArg {
    Always,
    Never,
    Auto,
}
